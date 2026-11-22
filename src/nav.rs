// In-tab navigation: goto, back, forward and reload, with an honest outcome.
//
// Kept out of the `tabs` arm because the interesting part is the wait: the old
// document already reports `readyState: complete`, so the answer has to come
// from Page events. A hash change or a History API push answers with
// `sameDocument` at once instead of waiting for a document that never arrives.

use serde_json::{json, Value};

use crate::wait::wait_idle;
use crate::{cdp, scan_cache, scope_cache};

/// How long a navigation gets to commit before the tool says what it knows.
const COMMIT_MS: u64 = 15_000;
const IDLE_MS: u64 = 15_000;

#[derive(Clone)]
pub(crate) enum Action {
    Goto(String),
    Back,
    Forward,
    Reload { ignore_cache: bool },
}

/// The page asked before leaving: report it as an outcome, not an error. The
/// command that starts a navigation can itself be interrupted by the dialog, and
/// a beforeunload is exactly that case.
fn dialog_outcome(conn: &mut cdp::Conn, tab: &str) -> Value {
    json!({
        "tab": tab, "moved": false, "url": conn.target_url().unwrap_or_default(),
        "dialog": conn.dialog.clone().unwrap_or(Value::Null),
        "note": "the page is asking before it leaves; answer it with \
                 input { actions: [{ dialog: { accept: true } }] }",
    })
}

/// A new document invalidates every index and the scope it was scanned in.
fn clear_page_state(tab: &str) {
    scan_cache().lock().unwrap().remove(tab);
    scope_cache().lock().unwrap().remove(tab);
}

pub(crate) fn navigate(
    conn: &mut cdp::Conn,
    tab: &str,
    action: Action,
    idle: bool,
) -> Result<Value, String> {
    let current = |conn: &mut cdp::Conn| conn.target_url().unwrap_or_default();
    let mut same_document = false;

    match &action {
        Action::Goto(url) => {
            conn.begin_navigation();
            let r = match conn.call("Page.navigate", json!({ "url": url })) {
                Ok(r) => r,
                Err(_) if conn.dialog.is_some() => return Ok(dialog_outcome(conn, tab)),
                Err(e) => return Err(e),
            };
            if let Some(err) = r.get("errorText").and_then(Value::as_str).filter(|e| !e.is_empty()) {
                return Err(format!("goto failed: {err}"));
            }
            // No loader id means no new document: a hash change or pushState.
            // Waiting for a commit there would sit out the whole deadline.
            same_document = r.get("loaderId").map(Value::is_null).unwrap_or(true);
        }
        Action::Back | Action::Forward => {
            let v = conn.call("Page.getNavigationHistory", json!({}))?;
            let idx = v.get("currentIndex").and_then(Value::as_i64).unwrap_or(0);
            let entries = v.get("entries").and_then(Value::as_array).cloned().unwrap_or_default();
            let entry = match action {
                Action::Back if idx > 0 => entries.get((idx - 1) as usize).cloned(),
                Action::Forward if (idx + 1) < entries.len() as i64 => {
                    entries.get((idx + 1) as usize).cloned()
                }
                _ => None,
            };
            let Some(entry) = entry else {
                // The ends of history are an answer, not a CDP error.
                return Ok(json!({
                    "tab": tab, "moved": false, "url": current(conn),
                    "note": "already at that end of the history",
                }));
            };
            let id = entry.get("id").and_then(Value::as_i64).ok_or("the history entry has no id")?;
            conn.begin_navigation();
            if let Err(e) = conn.call("Page.navigateToHistoryEntry", json!({ "entryId": id })) {
                if conn.dialog.is_some() {
                    return Ok(dialog_outcome(conn, tab));
                }
                return Err(e);
            }
        }
        Action::Reload { ignore_cache } => {
            conn.begin_navigation();
            if let Err(e) = conn.call("Page.reload", json!({ "ignoreCache": ignore_cache })) {
                if conn.dialog.is_some() {
                    return Ok(dialog_outcome(conn, tab));
                }
                return Err(e);
            }
        }
    }

    if same_document {
        return Ok(json!({
            "tab": tab, "moved": true, "url": current(conn), "sameDocument": true,
        }));
    }

    match conn.wait_navigation(COMMIT_MS)? {
        Some(cdp::NavOutcome::Dialog) => Ok(dialog_outcome(conn, tab)),
        Some(cdp::NavOutcome::SameDocument(url)) => Ok(json!({
            "tab": tab, "moved": true, "url": url, "sameDocument": true,
        })),
        Some(cdp::NavOutcome::Committed(url)) => {
            // A commit happened either way, so the old indexes are gone.
            clear_page_state(tab);
            if url.starts_with("chrome-error://") {
                // The browser committed its error page: the navigation failed,
                // and reporting moved:true would hide it.
                return Err(format!(
                    "the navigation failed: the browser showed its own error page ({url})"
                ));
            }
            let mut out = json!({ "tab": tab, "moved": true, "url": url });
            // The page committed and then asked something: the move stands, and
            // the dialog is reported for the next call to answer.
            if let Some(d) = conn.dialog.clone() {
                out["dialog"] = d;
                out["note"] = json!("the document is the new one; a load-time dialog is open");
            }
            if idle {
                match wait_idle(conn, IDLE_MS, 300) {
                    Ok(n) => {
                        out["idle"] = json!(true);
                        out["resources"] = json!(n);
                    }
                    Err(e) => {
                        // A dialog or a dead socket is not "settled": say which.
                        if let Some(d) = conn.dialog.clone() {
                            out["dialog"] = d;
                        } else {
                            return Err(e);
                        }
                    }
                }
            }
            Ok(out)
        }
        None => Ok(json!({
            "tab": tab, "moved": false, "started": true, "url": current(conn),
            "note": format!("the browser accepted it but no commit arrived within {COMMIT_MS} ms"),
        })),
    }
}
