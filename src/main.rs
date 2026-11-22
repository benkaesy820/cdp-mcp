//! cdp-mcp — a tiny MCP server that drives your own Chromium browser over the
//! DevTools protocol.
//!
//! Design notes, because they are the product:
//!   * no extension, no relay, no second browser — it attaches to the browser
//!     you are already using, so your logins are already there;
//!   * `input` sends real mouse/key events, which is the only way to drive
//!     rich editors (React/Draft) that ignore synthetic DOM events;
//!   * `elements` returns actionable targets with coordinates, so an agent
//!     never has to guess a selector or hold a reference that goes stale;
//!   * loopback only by default — CDP is unauthenticated, so a remote endpoint
//!     has to be an explicit opt-in.

mod cdp;
mod clip;
mod emulate;
mod nav;
mod pdf;
mod scan;
mod scroll;
mod wait;

use clip::*;
use wait::*;

use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::io::{self, BufRead, Write};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Irreversible actions already performed in this process.
/// Keyed by tab + label + index: a retry loop is caught, a *different* post is
/// not, and a same-labelled button on another tab is a different click.
fn guard() -> &'static Mutex<HashSet<String>> {
    static DONE: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    DONE.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Final confirmations only. A bare "Share" opens a dialog and is not included;
/// "Share now" is, because that is the click that publishes.
fn irreversible(label: &str) -> bool {
    let l = label.trim().to_lowercase();
    ["post", "publish", "share now", "send", "delete", "remove", "pay", "buy",
     "place order", "confirm", "submit order"]
        .iter()
        .any(|w| l == *w || l.starts_with(&format!("{w} ")) || l.ends_with(&format!(" {w}")))
}

/// Refuse a second click on the same final confirmation in one run, unless the
/// caller forced it. One place, because the frame path and the page path must
/// not drift: a frame click that skipped this would be a way around it.
fn guard_irreversible(tab: &str, label: &str, i: u64, force: bool) -> Result<(), String> {
    let key = format!("{tab}|{label}|{i}");
    let already = guard().lock().unwrap().contains(&key);
    if already && !force {
        return Err(format!(
            "'{label}' was already clicked in this run. \
             Verify the result with elements {{ mode: \"delta\" }} first; \
             pass input {{ force: true }} only if you mean to do it twice."
        ));
    }
    guard().lock().unwrap().insert(key);
    Ok(())
}

/// An empty or whitespace-only selector is no selector: it must not read as a
/// scope that matched everything, and it must not be passed to querySelector.
fn nonempty_scope(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() { None } else { Some(t.to_string()) }
}

/// The refusal a blocked indexed click reports. One shape for the page and the
/// frame path, so a reason and its target cannot drift apart.
fn blocked_click(
    reason: &str,
    i: u64,
    tag: &Value,
    label: &Value,
    obscured_by: Value,
    in_frame: bool,
) -> String {
    let mut target = json!({ "i": i, "tag": tag, "label": label });
    if in_frame {
        target["inFrame"] = json!(true);
    }
    json!({ "blocked": true, "reason": reason, "target": target, "obscuredBy": obscured_by })
        .to_string()
}

/// Which platform's conventions to follow. A value, not a `cfg`, so the
/// resolution below can be tested for every platform on any machine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Os { Linux, Mac, Windows }

fn current_os() -> Os {
    if cfg!(target_os = "macos") {
        Os::Mac
    } else if cfg!(windows) {
        Os::Windows
    } else {
        Os::Linux
    }
}

fn env_nonempty(get: &dyn Fn(&str) -> Option<String>, key: &str) -> Option<String> {
    get(key).filter(|s| !s.is_empty())
}

fn home(get: &dyn Fn(&str) -> Option<String>) -> std::path::PathBuf {
    std::path::PathBuf::from(get("HOME").unwrap_or_default())
}

/// Where settings live: XDG first on Linux and macOS, `%APPDATA%` on Windows.
fn config_root_for(os: Os, get: &dyn Fn(&str) -> Option<String>) -> std::path::PathBuf {
    match os {
        Os::Linux => env_nonempty(get, "XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| home(get).join(".config")),
        Os::Mac => env_nonempty(get, "XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| home(get).join("Library").join("Application Support")),
        Os::Windows => env_nonempty(get, "APPDATA")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(get("USERPROFILE").unwrap_or_default())
                .join("AppData").join("Roaming")),
    }
}

/// Where reports, clipboard history and screenshots live.
fn data_root_for(os: Os, get: &dyn Fn(&str) -> Option<String>) -> std::path::PathBuf {
    match os {
        Os::Linux => env_nonempty(get, "XDG_DATA_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| home(get).join(".local").join("share")),
        Os::Mac => env_nonempty(get, "XDG_DATA_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| home(get).join("Library").join("Application Support")),
        Os::Windows => env_nonempty(get, "LOCALAPPDATA")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(get("USERPROFILE").unwrap_or_default())
                .join("AppData").join("Local")),
    }
}

fn config_dir_for(os: Os, get: &dyn Fn(&str) -> Option<String>) -> std::path::PathBuf {
    config_root_for(os, get).join("cdp-mcp")
}

fn data_dir_for(os: Os, get: &dyn Fn(&str) -> Option<String>) -> std::path::PathBuf {
    data_root_for(os, get).join("cdp-mcp")
}

fn config_dir() -> std::path::PathBuf {
    config_dir_for(current_os(), &|k| std::env::var(k).ok())
}

fn data_dir() -> std::path::PathBuf {
    data_dir_for(current_os(), &|k| std::env::var(k).ok())
}

/// The MIME the bytes really are. `shot` used to report a WebP capture as JPEG.
fn mime_for(format: &str) -> &'static str {
    match format {
        "png" => "image/png",
        "webp" => "image/webp",
        _ => "image/jpeg",
    }
}

/// Last scan per tab, so `mode:"delta"` can say what changed instead of
/// re-sending the whole page after every click. Keyed by tab id.
fn scan_cache() -> &'static Mutex<HashMap<String, Value>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Value>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn conn_cache() -> &'static Mutex<HashMap<String, cdp::Conn>> {
    static CONNS: OnceLock<Mutex<HashMap<String, cdp::Conn>>> = OnceLock::new();
    CONNS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The tab the last successful call used, so `tab: None` needs no HTTP lookup.
fn last_tab() -> &'static Mutex<Option<String>> {
    static LAST: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(None))
}

/// Run `f` against a connection that is kept alive per tab.
///
/// Two costs had to go: reconnecting per call (a websocket handshake plus two
/// enable round trips), and resolving the tab through `/json/list` over HTTP on
/// every call. With both cached, a one-line eval is a single websocket message.
///
/// A cached socket can be dead (tab closed, browser restarted). `f` is run a
/// second time on a fresh connection ONLY when the socket died before the page
/// answered anything: `f` is usually a list of clicks and keystrokes, and
/// re-running one that got half-way would click and type twice. Any other
/// failure is the page's answer, and is returned as it is.
/// How a dead socket may be retried.
#[derive(PartialEq)]
enum Retry {
    /// Re-run only if nothing reached the page. A half-finished click or key
    /// sequence must never be repeated.
    IfUntouched,
    /// Re-run once on a fresh connection. For a read-only wait nothing can be
    /// repeated, and starting over is better than failing a ten-minute wait
    /// because the browser was restarted under it.
    Always,
}

fn with_conn<R>(
    endpoint: &str,
    tab: Option<&str>,
    f: impl FnMut(&mut cdp::Conn, &str) -> Result<R, String>,
) -> Result<R, String> {
    with_conn_retry(endpoint, tab, Retry::IfUntouched, false, f)
}

/// `with_conn` for the one action that may run while a dialog is open: answering
/// it. Everything else fails fast, so a blocked page is reported, not waited on.
fn with_conn_dialog<R>(
    endpoint: &str,
    tab: Option<&str>,
    f: impl FnMut(&mut cdp::Conn, &str) -> Result<R, String>,
) -> Result<R, String> {
    with_conn_retry(endpoint, tab, Retry::IfUntouched, true, f)
}

/// The refusal a tool gets while a dialog is open: the dialog itself, plus the
/// action that answers it, instead of a 30 s timeout.
fn dialog_block(d: &Value) -> String {
    json!({
        "dialog": d,
        "hint": "the page is blocked by this dialog: answer it with \
                 input { actions: [{ dialog: { accept: true } }] } (add text for a prompt)",
    })
    .to_string()
}

/// `with_conn` for a call with no side effects.
fn with_conn_readonly<R>(
    endpoint: &str,
    tab: Option<&str>,
    f: impl FnMut(&mut cdp::Conn, &str) -> Result<R, String>,
) -> Result<R, String> {
    with_conn_retry(endpoint, tab, Retry::Always, false, f)
}

fn with_conn_retry<R>(
    endpoint: &str,
    tab: Option<&str>,
    retry: Retry,
    allow_dialog: bool,
    mut f: impl FnMut(&mut cdp::Conn, &str) -> Result<R, String>,
) -> Result<R, String> {
    // A retry must land on the page the first attempt was talking to. With no
    // tab named, `resolve` would pick whichever page is first in the list, so a
    // wait that died could silently resume on a different tab.
    let mut pinned: Option<String> = None;
    let cached_key = {
        let cache = conn_cache().lock().unwrap();
        match tab {
            // cached connection for an explicitly named tab (exact, then prefix)
            Some(want) => cache
                .keys()
                .find(|k| k.as_str() == want)
                .or_else(|| cache.keys().find(|k| k.starts_with(want)))
                .cloned(),
            // no tab given: reuse whatever we used last, if it is still cached
            None => last_tab().lock().unwrap().clone().filter(|k| cache.contains_key(k)),
        }
    };

    if let Some(k) = cached_key {
        pinned = Some(k.clone());
        let mut cache = conn_cache().lock().unwrap();
        if let Some(conn) = cache.get_mut(&k) {
            if cdp::is_owned(&k) {
                // A tab we opened lives in the background so the human keeps
                // their keyboard; it still has to render for screenshots and
                // touch. Free after the first call: the flag is in the Conn.
                let _ = conn.set_focus_emulation(true);
            }
            let before = conn.replies;
            // Whatever already arrived is read first: a dialog opened between
            // calls must be seen before this call decides anything, and an
            // idle-dead socket fails here, where the retry below handles it.
            let mut attempt = |conn: &mut cdp::Conn| -> Result<R, String> {
                conn.drain_arrived()?;
                conn.enable_page()?;
                conn.cleanup_domains()?;
                if !allow_dialog {
                    if let Some(d) = conn.dialog.clone() {
                        return Err(dialog_block(&d));
                    }
                }
                f(conn, &k)
            };
            match attempt(conn) {
                Ok(v) => {
                    *last_tab().lock().unwrap() = Some(k);
                    return Ok(v);
                }
                Err(e) if !conn.dead => return Err(e),
                Err(e) => {
                    let untouched = retry == Retry::Always || conn.replies == before;
                    cache.remove(&k);
                    if !untouched {
                        return Err(format!(
                            "{e} — the connection dropped part-way through, so it was not retried \
                             (retrying could repeat what already happened)"
                        ));
                    }
                }
            }
        }
    }

    // cold path: one HTTP lookup, then connect and keep it. A tab this server
    // opened is connected to directly: its address is known, and `/json/list`
    // can lag behind a target created a moment ago.
    let want = tab.or(pinned.as_deref());
    let (id, ws_url) = match want.filter(|t| cdp::is_owned(t)) {
        Some(t) => (t.to_string(), cdp::page_ws_url(endpoint, t)?),
        None => {
            let target = cdp::resolve(endpoint, want)?;
            (target.id, target.ws_url)
        }
    };
    let mut conn = cdp::Conn::open(&ws_url)?;
    if cdp::is_owned(&id) {
        let _ = conn.set_focus_emulation(true);
    }
    let mut attempt = |conn: &mut cdp::Conn| -> Result<R, String> {
        conn.drain_arrived()?;
        conn.enable_page()?;
        conn.cleanup_domains()?;
        if !allow_dialog {
            if let Some(d) = conn.dialog.clone() {
                return Err(dialog_block(&d));
            }
        }
        f(conn, &id)
    };
    let out = attempt(&mut conn);
    *last_tab().lock().unwrap() = Some(id.clone());
    if !conn.dead {
        conn_cache().lock().unwrap().insert(id, conn);
    }
    out
}

/* ------------------------------ cancellation ------------------------------ */

/// Set when the client cancels the request being worked on (or goes away).
/// Long loops check it, so "stop" in the client stops the clicking.
static CANCELLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn check_cancel() -> Result<(), String> {
    if CANCELLED.load(std::sync::atomic::Ordering::Relaxed) {
        Err("cancelled by the client".into())
    } else {
        Ok(())
    }
}

/// Sleep that a cancellation cuts short.
fn nap(ms: u64) -> Result<(), String> {
    let until = std::time::Instant::now() + std::time::Duration::from_millis(ms);
    loop {
        check_cancel()?;
        let left = until.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        std::thread::sleep(left.min(std::time::Duration::from_millis(50)));
    }
}

/// The scope (`within`) of the last `elements` scan per tab. An index is only
/// meaningful in the scan that produced it, so a click must resolve it in the
/// same scope or it lands on a different element.
fn scope_cache() -> &'static Mutex<HashMap<String, Option<String>>> {
    static SCOPES: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
    SCOPES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn scope_for(tab_id: &str) -> Option<String> {
    scope_cache().lock().unwrap().get(tab_id).cloned().flatten()
}

/// Cut a string to at most `max` bytes on a character boundary. Slicing at a
/// byte offset panics inside a multi-byte character, and a panic ends the
/// server (panic = "abort").
fn clip_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// SCAN_JS with its scope placeholder substituted (None = whole document).
fn scan_js(scope: Option<&str>) -> String {
    let lit = match scope {
        Some(s) => serde_json::to_string(s).unwrap_or_else(|_| "null".into()),
        None => "null".into(),
    };
    scan::SCAN_JS.replace("__SCOPE__", &lit).replace("__PICK__", "null")
}

/// Resolve element `i` (in `scope`) inside the page, scroll it into view if
/// asked, and report where a click on it lands. One evaluation: this used to
/// be four full scans per click.
fn pick(conn: &mut cdp::Conn, scope: Option<&str>, i: u64, scroll: bool, select: Option<&Value>) -> Result<Value, String> {
    let lit = match scope {
        Some(s) => serde_json::to_string(s).unwrap_or_else(|_| "null".into()),
        None => "null".into(),
    };
    let mut p = json!({ "i": i, "scroll": scroll });
    if let Some(v) = select {
        p["select"] = json!(v);
    }
    let js = scan::SCAN_JS.replace("__SCOPE__", &lit).replace("__PICK__", &p.to_string());
    let v = conn.eval(&js)?;
    if v.get("missing").and_then(Value::as_bool) == Some(true) {
        let total = v.get("total").and_then(Value::as_u64).unwrap_or(0);
        return Err(format!(
            "no element with index {i} ({total} found{}); call elements again",
            scope.map(|s| format!(" within {s}")).unwrap_or_default()
        ));
    }
    Ok(v)
}

/// Scan the page plus every cross-origin iframe inside it.
///
/// The page's own script cannot read across a site boundary — that is site
/// isolation, not a bug in the scanner — so an embedded widget's buttons are
/// absent from a normal scan. Chromium exposes each such frame as its own
/// target with its own session, which is what `Conn::frames` collects.
///
/// Every frame's elements are appended after the page's own, so an index still
/// means one element and every page element keeps the index it already had. A
/// scoped scan is the exception: a CSS selector cannot reach into a frame.
/// Coordinates are translated into the embedding page's viewport, which is the
/// space a click is dispatched in.
fn scan_with_frames(conn: &mut cdp::Conn, scope: Option<&str>) -> Result<Value, String> {
    let mut scan_v = conn.eval(&scan_js(scope))?;
    // A CSS selector can only match this document. Frame elements appended to a
    // scoped scan would be indexes the caller never asked for, in a list it
    // believes is scoped.
    if scope.is_some() {
        return Ok(scan_v);
    }
    let frames = match conn.frames() {
        Ok(f) => f,
        Err(_) => return Ok(scan_v), // older browser: page-only scan is still useful
    };
    if frames.is_empty() {
        return Ok(scan_v);
    }
    let mut elements = scan_v.get("elements").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut added = 0u64;
    for (target, session, url) in frames {
        // Read the frame's own document, then find where its box sits in the
        // parent so local coordinates can be made absolute.
        // The frame returns a JSON string: one value in, one value out, and no
        // surprise about how deep the protocol will serialise.
        let body = match conn.eval_in(&session, scan::FRAME_ELEMENTS_JS) {
            Ok(Value::String(body)) => serde_json::from_str::<Value>(&body).unwrap_or(Value::Null),
            Ok(v) => v,
            Err(_) => continue, // sandboxed, detached, or no document yet
        };
        let kids = body.get("elements").and_then(Value::as_array).cloned().unwrap_or_default();
        if kids.is_empty() {
            continue;
        }
        let origin = match conn.eval(&format!(
            "(() => {{ const f = [...document.querySelectorAll('iframe, frame')].find(f => (f.src || '') === {src}); \
               if (!f) return null; const r = f.getBoundingClientRect(); \
               if (r.width < 2 || r.height < 2) return null; \
               if (f.checkVisibility && !f.checkVisibility({{ checkOpacity: true, checkVisibilityCSS: true }})) return null; \
               return JSON.stringify({{ x: r.left + f.clientLeft, y: r.top + f.clientTop, w: f.clientWidth, h: f.clientHeight }}); }})()",
            src = serde_json::to_string(&url).unwrap_or_else(|_| "null".into())
        )) {
            Ok(Value::String(s)) => serde_json::from_str::<Value>(&s).ok(),
            _ => None,
        };
        let origin = match origin {
            Some(o) if !o.is_null() => o,
            _ => continue, // cannot place it on screen, so a click there is a guess
        };
        let (ox, oy) = (
            origin.get("x").and_then(Value::as_f64).unwrap_or(0.0),
            origin.get("y").and_then(Value::as_f64).unwrap_or(0.0),
        );
        let label = url
            .split("://").nth(1).unwrap_or(&url)
            .split('/').next().unwrap_or(&url)
            .chars().take(40).collect::<String>();
        for mut k in kids {
            // A frame can be scrolled: only what the parent actually shows is
            // clickable, so anything outside the frame's box is marked off-screen.
            let (x, y) = (
                k.get("x").and_then(Value::as_f64).unwrap_or(0.0),
                k.get("y").and_then(Value::as_f64).unwrap_or(0.0),
            );
            let (w, h) = (
                k.get("w").and_then(Value::as_f64).unwrap_or(0.0),
                k.get("h").and_then(Value::as_f64).unwrap_or(0.0),
            );
            // The click lands at the child's centre, so the centre is the point
            // that has to be inside the frame: an edge-flush element whose
            // rounded edges overshoot by a pixel is still clickable, and one
            // whose centre is outside is not.
            let (lx, ly) = ((x + w / 2.0).round(), (y + h / 2.0).round());
            if let Some(o) = k.as_object_mut() {
                o.insert("i".into(), json!(elements.len()));
                o.insert("x".into(), json!((x + ox + w / 2.0).round() as i64));
                o.insert("y".into(), json!((y + oy + h / 2.0).round() as i64));
                o.insert("frame".into(), json!(label));
                o.insert("session".into(), json!(session));
                // The target id survives a reconnect; the session id does not.
                o.insert("target".into(), json!(target));
                // Local centre, which is what a click inside the frame needs.
                o.insert("lx".into(), json!(lx as i64));
                o.insert("ly".into(), json!(ly as i64));
            }
            let fw = origin.get("w").and_then(Value::as_f64).unwrap_or(0.0);
            let fh = origin.get("h").and_then(Value::as_f64).unwrap_or(0.0);
            let inside = lx >= 0.0 && ly >= 0.0 && lx <= fw && ly <= fh;
            if !inside {
                if let Some(o) = k.as_object_mut() {
                    o.insert("off".into(), json!(true));
                }
            }
            elements.push(k);
            added += 1;
            if elements.len() >= 600 {
                break;
            }
        }
    }
    if added > 0 {
        if let Some(o) = scan_v.as_object_mut() {
            o.insert("elements".into(), Value::Array(elements));
            o.insert("framesScanned".into(), json!(added));
        }
    }
    Ok(scan_v)
}

/// If element `i` of tab `tab` lives inside a cross-origin frame, where to
/// click it.
///
/// Returns `(local x, local y, session)`. Cheap in the common case: one cached
/// scan lookup, no protocol traffic; the browser is only asked when the element
/// carries a `session`, which `scan_with_frames` sets only for frames.
///
/// The target id proves the frame is the same one, not that the document inside
/// it still shows the same element: a same-site navigation keeps both the target
/// and the old coordinates, so the click can land on whatever moved there.
fn frame_target(
    conn: &mut cdp::Conn,
    tab: &str,
    scope: Option<&str>,
    i: u64,
    force: bool,
) -> Result<Option<(f64, f64, String)>, String> {
    // A scoped click indexes the scoped list, which holds no frame elements: a
    // CSS selector cannot reach into a child document. Resolving it in the tab's
    // unscoped cache would click a different element than the one named.
    if scope.is_some() {
        return Ok(None);
    }
    // The cache holds one entry per tab. Reading whichever entry came first sent
    // this click into a session that belongs to a different page's connection.
    let cached = {
        let cache = scan_cache().lock().unwrap();
        cache
            .get(tab)
            .and_then(|v| v.get("elements").and_then(Value::as_array).cloned())
    };
    let el = cached
        .and_then(|els| els.get(i as usize).cloned())
        .filter(|e| e.get("session").and_then(Value::as_str).is_some());
    let el = match el {
        Some(e) => e,
        None => return Ok(None),
    };
    let mut session = el.get("session").and_then(Value::as_str).unwrap_or_default().to_string();
    if session.is_empty() {
        return Ok(None);
    }
    // Resolve the session on this connection: a cached id goes stale when the
    // page reconnects, and ids attached elsewhere are not usable here. The
    // target id survives both, which is why the scan records it.
    let want = el.get("target").and_then(Value::as_str).unwrap_or_default();
    if !want.is_empty() {
        match conn.frames() {
            Ok(frames) => match frames.iter().find(|f| f.0 == want) {
                Some((_, s, _)) => session = s.clone(),
                // The frame is gone, or it is same-origin now. Falling through to
                // the main document would act on a different element with this index.
                None => {
                    return Err(format!(
                        "element {i} was inside a frame that is no longer there; call elements again"
                    ));
                }
            },
            // Without the frame list the cached session cannot be trusted: it may
            // belong to a connection that is gone.
            Err(e) => {
                return Err(format!(
                    "cannot resolve the frame for element {i} ({e}); call elements again"
                ));
            }
        }
    }
    // The same refusals the page path makes, from the state the frame scan
    // recorded: without them a frame was a way around both. They come after the
    // resolution, so a frame that is gone does not consume the guard.
    let tag = el.get("tag").unwrap_or(&Value::Null);
    let label = el.get("label").unwrap_or(&Value::Null);
    if !force && el.get("disabled").and_then(Value::as_bool) == Some(true) {
        return Err(blocked_click("disabled", i, tag, label, Value::Null, true));
    }
    // Below the frame's own fold: what scrolls is the frame's document, not the
    // page, so a dispatch at these coordinates lands outside its viewport. An
    // unfelt click that still burns the guard is the worst of both.
    if !force && el.get("off").and_then(Value::as_bool) == Some(true) {
        return Err(blocked_click("outside_frame", i, tag, label, Value::Null, true));
    }
    if let Some(text) = label.as_str() {
        if irreversible(text) {
            guard_irreversible(tab, text, i, force)?;
        }
    }
    let x = el.get("lx").and_then(Value::as_f64).unwrap_or(0.0);
    let y = el.get("ly").and_then(Value::as_f64).unwrap_or(0.0);
    Ok(Some((x, y, session)))
}

/// Send one mouse event to a frame's own session, at frame-local coordinates.
fn frame_click(
    conn: &mut cdp::Conn,
    session: &str,
    x: f64,
    y: f64,
    button: &str,
    count: u64,
    mods: u32,
) -> Result<(), String> {
    // The same three events the page path sends, and the same held-button mask:
    // a right click with buttons:1 is a left-button state to the page's scripts.
    let held = match button { "right" => 2, "middle" => 4, _ => 1 };
    conn.call_in(Some(session), "Input.dispatchMouseEvent", json!({
        "type": "mouseMoved", "x": x, "y": y, "modifiers": mods,
    }))?;
    conn.call_in(Some(session), "Input.dispatchMouseEvent", json!({
        "type": "mousePressed", "x": x, "y": y, "button": button,
        "clickCount": count, "buttons": held, "modifiers": mods,
    }))?;
    conn.call_in(Some(session), "Input.dispatchMouseEvent", json!({
        "type": "mouseReleased", "x": x, "y": y, "button": button,
        "clickCount": count, "buttons": 0, "modifiers": mods,
    }))?;
    Ok(())
}

/// Is this empty scan more likely a page that has not painted yet?
///
/// A React app mid-hydration and a genuinely empty document both return no
/// elements. What tells them apart is how much DOM exists: a page still being
/// built has nodes but no interactive ones yet, while a truly empty document has
/// almost nothing at all.
fn suspect_render(total: u64, nodes: u64) -> bool {
    total < 3 && nodes < 400
}

fn fingerprint(e: &Value) -> String {
    format!(
        "{}|{}|{}|{}",
        e.get("tag").and_then(Value::as_str).unwrap_or(""),
        e.get("label").and_then(Value::as_str).unwrap_or(""),
        e.get("x").and_then(Value::as_i64).unwrap_or(0),
        e.get("y").and_then(Value::as_i64).unwrap_or(0),
    )
}
const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:9222";


fn help() -> String {
    format!(
        "cdp-mcp {VERSION}\n\
         A tiny MCP server that drives your own Chromium browser over DevTools.\n\n\
         USAGE:\n  cdp-mcp [--cdp URL] [--allow-remote]\n\n\
         OPTIONS:\n\
         \x20 --cdp URL        DevTools endpoint (default {DEFAULT_ENDPOINT})\n\
         \x20 --allow-remote   permit a non-loopback endpoint (experimental builds only; CDP has no auth)\n\
         \x20 -h, --help       print this\n\
         \x20 -V, --version    print the version\n\n\
         The browser must be started with --remote-debugging-port=9222.\n"
    )
}

/// Per-agent settings. Deliberately a small JSON file, not a database: a
/// handful of keys, readable and editable by hand, no crate and no migration.
#[derive(Clone)]
struct Profile {
    name: String,
    path: std::path::PathBuf,
    data: Value,
}

impl Cfg {
    /// The profile as it is on disk *now*: `profile set` may have changed it
    /// during this process's lifetime, and a stale copy would silently write
    /// screenshots to the old folder.
    fn profile_now(&self) -> Profile {
        Profile::load(&self.profile.name)
    }
}

impl Profile {
    fn load(name: &str) -> Self {
        let path = config_dir().join(format!("{name}.json"));
        let data = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .unwrap_or_else(|| json!({}));
        Profile { name: name.to_string(), path, data }
    }
    fn get_str(&self, key: &str) -> Option<String> {
        self.data.get(key).and_then(Value::as_str).map(str::to_string)
    }
    fn get_u64(&self, key: &str) -> Option<u64> {
        self.data.get(key).and_then(Value::as_u64)
    }
    fn set(&mut self, key: &str, value: Value) -> Result<(), String> {
        if let Some(obj) = self.data.as_object_mut() {
            obj.insert(key.to_string(), value);
        } else {
            self.data = json!({ key: value });
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        let body = serde_json::to_string_pretty(&self.data).map_err(|e| e.to_string())?;
        std::fs::write(&self.path, body).map_err(|e| format!("cannot write {}: {e}", self.path.display()))
    }
    /// Where a screenshot goes when the caller gave no path.
    fn shot_dir(&self) -> std::path::PathBuf {
        self.get_str("screenshot_dir").map(std::path::PathBuf::from)
            .unwrap_or_else(|| data_dir().join("shots"))
    }
}

#[derive(Clone)]
struct Cfg {
    endpoint: String,
    profile: Profile,
}

fn is_loopback(endpoint: &str) -> bool {
    let host = endpoint
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("")
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or("");
    matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1")
}

/// A non-loopback endpoint is refused unless the build opted into the
/// experimental feature and the caller passed the flag. CDP has no auth, so
/// this is the one guard that must not be on by default.
fn remote_allowed(loopback: bool, requested: bool) -> bool {
    loopback || (requested && cfg!(feature = "experimental"))
}

/* ------------------------------- the tools ------------------------------- */

fn tool_defs() -> Value {
    json!([
        {
            "name": "flow",
            "description": "Run a sequence of tool calls inside the server, optionally repeating until a condition is true. The point is cost: N browser steps for ONE model turn. Use `repeat:{untilText|untilGone, max, delayMs}` for 'click more until the thing appears' without a turn per click. Returns iteration count, whether the condition was met, and the last three iterations only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "steps": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": { "tool": { "type": "string" }, "args": { "type": "object" } },
                            "required": ["tool"]
                        }
                    },
                    "repeat": {
                        "type": "object",
                        "properties": {
                            "untilText": { "type": "string" },
                            "untilGone": { "type": "string" },
                            "max": { "type": "integer", "description": "default 1, max 200" },
                            "delayMs": { "type": "integer", "description": "between iterations, default 700" }
                        }
                    },
                    "stopOnError": { "type": "boolean", "description": "default true" },
                    "tab": { "type": "string" }
                },
                "required": ["steps"]
            }
        },
        {
            "name": "idle",
            "description": "Wait without spinning and without asking: a MutationObserver inside the page calls back the moment something changes, so a still page costs no traffic at all and a change is noticed within a few milliseconds instead of on a timer. Returns when `untilText` or `untilSelector` matches, or when the page has been still for `maxIdleMs`. The result names what changed (`appeared`, `dialogs`, `changes`, `textChars`/`textDelta`, `url`, `navigated`), so you usually do not need to scan the page afterwards. `changes` counts reports from the page, coalesced to at most one per 80 ms while it is moving, so read it as activity rather than as a mutation count. Use it instead of a sleep loop when waiting for a page.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "timeoutMs": { "type": "integer", "description": "stop waiting after this long, message or not (default 600000 = 10 min, max 900000)" },
                    "maxIdleMs": { "type": "integer", "description": "treat the page as idle after this long with no change (defaults to timeoutMs). Values below about 100 ms are approximate: reports are coalesced" },
                    "untilText": { "type": "string", "description": "also wake when this text appears" },
                    "untilSelector": { "type": "string", "description": "also wake when this CSS selector matches something, e.g. input[name=email] or [role=dialog]" },

                    "tab": { "type": "string" }
                }
            }
        },
        {
            "name": "report",
            "description": "Report a problem or an improvement idea about these tools, while it is fresh. One line of JSONL per report, with version, profile, agent and the page it happened on. Use it instead of only complaining in prose — it is the channel that gets read.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["add", "list"], "description": "default add" },
                    "kind": { "type": "string", "enum": ["issue", "idea"] },
                    "summary": { "type": "string", "description": "one line: what went wrong, or what would make this better" },
                    "details": { "type": "string", "description": "optional: what you tried, what you expected" },
                    "tool": { "type": "string", "description": "which tool it concerns, e.g. elements" },
                    "tab": { "type": "string", "description": "optional: capture the page URL for context" },
                    "limit": { "type": "integer", "description": "for list, default 20" }
                },
                "required": ["summary"]
            }
        },
        {
            "name": "batch",
            "description": "Run several tool calls in one request, in order. Use it for a whole flow (observe, act, verify) to cut the round trips between you and the browser. Each entry is {tool, args}; results come back in order with per-call timing. A failed call does not stop the rest.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "calls": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": { "tool": { "type": "string" }, "args": { "type": "object" } },
                            "required": ["tool"]
                        }
                    }
                },
                "required": ["calls"]
            }
        },
        {
            "name": "status",
            "description": "Report the browser version and every open page tab. Start here.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "tabs",
            "description": "List, open, close or navigate page tabs. goto, back, forward and reload act on the tab in place: they report moved and the resulting url, return moved:false at the ends of history, and return the dialog instead of waiting when the page asks before it leaves. A document navigation also drops this tab's scanned indexes and scope, so rescan before using an old index.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "open", "close", "goto", "back", "forward", "reload"] },
                    "url": { "type": "string", "description": "for action=open or goto" },
                    "idle": { "type": "boolean", "description": "for action=open or a navigation: also wait for loading to settle (single-page apps paint after readyState)" },
                    "ignoreCache": { "type": "boolean", "description": "for action=reload: bypass the HTTP cache" },
                    "foreground": { "type": "boolean", "description": "for action=open: bring the tab to the front and focus the browser. Default false, because activating a tab takes the keyboard away from whoever is typing." },
                    "tab": { "type": "string", "description": "the tab to act on; omit for the last tab this server used" },
                    "all": { "type": "boolean", "description": "for action=list: include chrome-extension:// pages (default false)" }
                },
                "required": ["action"]
            }
        },
        {
            "name": "pdf",
            "description": "Print the page to a PDF. Saves to `path` when given, otherwise under this profile's data directory as pdfs/page-<ms>.pdf. Backgrounds print by default: a styled page is what a human expects to read. Letter by default; a4, landscape, scale, pageRanges and preferCSSPageSize are explicit.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab": { "type": "string" },
                    "path": { "type": "string", "description": "where to write it; omit for the profile's pdfs folder" },
                    "format": { "type": "string", "enum": ["letter", "a4"], "description": "paper size (default letter)" },
                    "landscape": { "type": "boolean", "description": "default false" },
                    "printBackground": { "type": "boolean", "description": "default true" },
                    "scale": { "type": "number", "description": "0.1..2.0, default 1.0" },
                    "pageRanges": { "type": "string", "description": "e.g. \"1-2,4\"" },
                    "preferCSSPageSize": { "type": "boolean", "description": "honour an @page size when the page sets one" }
                }
            }
        },
        {
            "name": "emulate",
            "description": "Emulate a viewport, touch input and CSS media. A preset sets all of it: mobile 390x844@3, tablet 768x1024@2, desktop 1280x800@1; explicit width/height/deviceScaleFactor/mobile/touch override the preset. Media calls add up: colorScheme then reducedMotion keeps both, because Emulation.setEmulatedMedia replaces the whole list and the active set is tracked per tab. reset clears metrics, touch and media. A viewport change moves every element, so this tab's scan cache is dropped and the next elements rescans; a within scope survives.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab": { "type": "string" },
                    "reset": { "type": "boolean", "description": "clear device metrics, touch and media" },
                    "preset": { "type": "string", "enum": ["mobile", "tablet", "desktop"] },
                    "width": { "type": "integer", "description": "viewport width, 100..7680" },
                    "height": { "type": "integer", "description": "viewport height, 100..7680" },
                    "deviceScaleFactor": { "type": "number", "description": "device pixel ratio, greater than 0" },
                    "mobile": { "type": "boolean", "description": "emulate a mobile viewport" },
                    "touch": { "type": "boolean", "description": "defaults to mobile when a viewport is applied; works on its own too" },
                    "colorScheme": { "type": "string", "enum": ["light", "dark", "no-preference"] },
                    "reducedMotion": { "type": "string", "enum": ["reduce", "no-preference"] },
                    "media": { "type": "string", "enum": ["screen", "print", ""] }
                }
            }
        },
        {
            "name": "eval",
            "description": "Evaluate a JavaScript expression in the tab and return its value as JSON. Await promises. Omit `tab` for the first page tab.",
            "inputSchema": {
                "type": "object",
                "properties": { "expression": { "type": "string" }, "tab": { "type": "string" } },
                "required": ["expression"]
            }
        },
        {
            "name": "elements",
            "description": "Scan the page: `overlays` lists dialogs/modals/cookie walls with their dismiss buttons, and `elements` lists visible interactive nodes with a clickable index `i` and centre point. Filter with `find` and cap with `limit` to keep the payload small. Indexes are absolute in the unfiltered list and are re-resolved at click time, so they cannot silently go stale. A cross-origin frame element is clicked at the coordinates this scan recorded (rescan after the frame navigates), and only `click` routes an index into a frame; `hover`, `select`, `link` and `paste` resolve indexes in the main document. Form controls include current `value` (passwords masked), `checked`, `selected` and `disabled`; `within` scopes the scan to a selector in the main document (cross-origin frame elements are left out), the result echoes `scope`, and a selector that matches nothing returns no elements with `scopeMissing: true` instead of the whole page, and `overlays` stay page-wide and are still reported.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab": { "type": "string" },
                    "find": { "type": "string", "description": "only elements whose label contains this text (case-insensitive)" },
                    "limit": { "type": "integer", "description": "max elements to return (default 120)" },
                    "full": { "type": "boolean", "description": "include width and height" },
                    "mode": { "type": "string", "enum": ["full", "viewport", "delta"],
                              "description": "full = everything; viewport = only what is on screen; delta = only what changed since the previous scan (use after a click)" },
                    "shot": { "type": "string", "enum": ["none", "viewport", "full", "custom"],
                              "description": "also return a PNG in this same result: viewport = what you see, full = whole page (height-capped), custom = region {x,y,w,h}" },
                    "region": { "type": "object", "description": "for shot=custom: {x,y,w,h} in CSS pixels",
                                "properties": { "x": {"type":"number"}, "y": {"type":"number"}, "w": {"type":"number"}, "h": {"type":"number"} } },
                    "shotMaxWidth": { "type": "integer", "description": "downscale the attached capture to this width (default 1024)" },
                    "shotFormat": { "type": "string", "enum": ["jpeg", "png", "webp"], "description": "format of the attached capture (default jpeg)" },
                    "unique": { "type": "boolean", "description": "collapse elements that share a label (repeated nav links)" },
                    "within": { "type": "string", "description": "CSS selector to scope the scan, e.g. \"main\" — much cheaper on big pages" },
                    "role": { "type": "string", "description": "only elements with this aria role or tag, e.g. button, link, input" }
                }
            }
        },
        {
            "name": "text",
            "description": "The page as readable text, top to bottom: headings, labels, paragraphs, list items. Use this to find out what a page says or to confirm a sentence is there; use `elements` for what can be clicked. `within` scopes it to a CSS selector in the main document (main, article, [role=dialog]); a selector that matches nothing returns empty text with `scopeMissing: true`, and an invalid selector is an error. `limit` caps characters (default 4000).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "within": { "type": "string", "description": "CSS selector to read, default body" },
                    "limit": { "type": "integer", "description": "max characters to return (default 4000)" },
                    "tab": { "type": "string" }
                }
            }
        },
        {
            "name": "input",
            "description": "Send real user input, in order. Actions: {click:{i|x,y[,button:right|middle,count:2,modifiers:[Shift]]}} (an index is scrolled into view and resolved in the same `within` scope as the last elements call, or in the action's own `within` when it carries one), {hover:{i}}, {drag:{from:{i},to:{i},steps:12}}, {tap:{i}} (touch), {swipe:{from,to}} (touch), {select:{i,value}} (<select>; value is a string, or an array for a multiple select), {insert:\"text\"}, {key:\"Enter\"|\"Control+a\"|\"Control+Shift+z\"|\"a\"|\"Escape\"|\"Tab\"|\"ArrowDown\"|\"F5\"}, {scroll:{dy[,dx][,i|x,y]}} (a real wheel event at that element, that point, or the middle of the viewport: scrolls the page or the pane under it, and a canvas receives the wheel too; the log reports from/to/moved, `delivered` for whether the wheel reached the page, `container` and the wheel point), {dismiss:{prefer:\"reject\"}} (closes cookie walls and modals, refusing cookies when offered), {wait:{idle:true,ms:15000}} (waits for loading to settle), {wait:{text:\"...\",ms:15000}}, {sleep:{ms:300}}. Real events, so React/Draft editors accept them. An indexed click is refused when the element is detached, zero-size, disabled or covered, with `{blocked, reason, obscuredBy}`; `force` bypasses that check, and the log entry then carries `covered` with what was on top. With `settleMs`, the result also reports `navigated`, `url`, `openedTab`, `downloads` (started, with filename and guid) and same-origin errors plus a failed destination document. A JavaScript dialog blocks the page: the call that hit it returns `dialog:{type,message}`, every other tool then fails fast with that dialog, and `{dialog:{accept:true,text:\"...\"}}` answers it — one per call, so a chained alert after a confirm is reported for a second answer.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "actions": { "type": "array", "items": { "type": "object" } },
                    "tab": { "type": "string" },
                    "announce": { "type": "boolean", "description": "also check for a dialog that appeared because of these actions (default false: costs a round trip)" },
                    "force": { "type": "boolean", "description": "allow repeating a final confirmation (Post/Publish/Delete/…) that was already clicked in this run, and bypass the click obstruction check. Verify with delta first." },
                    "settleMs": { "type": "integer", "description": "after the actions, read CDP events for up to this many ms and return same-origin HTTP 4xx/5xx, uncaught exceptions and console errors in `errors` (default 0 = off). Adds the settle time to the call." }
                },
                "required": ["actions"]
            }
        },
        {
            "name": "upload",
            "description": "Attach a local file to the page by setting it on a file input (default selector input[type=file]). Works for images and video. If the picker only exists after a button press, click that first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "absolute local path" },
                    "selector": { "type": "string", "description": "CSS selector for the file input (default input[type=file])" },
                    "index": { "type": "integer", "description": "which match to use when several inputs share the selector (default 0). An avatar and a banner on one form are the common case." },
                    "tab": { "type": "string" }
                },
                "required": ["path"]
            }
        },
        {
            "name": "paste",
            "description": "Paste text or a file (image, video, PDF) into the focused element. Optionally click element `i` first to focus it. Default method \"page\" hands the data to the page as a paste event (a file nobody takes as a paste is offered as a drop; text nobody intercepts is inserted as typed input) and never touches the system clipboard. method \"system\" uses the real clipboard and Ctrl+V, which only works while the browser window has keyboard focus; the backend is Wayland wl-clipboard, X11 xclip, or macOS pbcopy/pbpaste, and an image on macOS is refused rather than silently dropped.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "local file to paste" },
                    "text": { "type": "string", "description": "text to paste (instead of a file)" },
                    "i": { "type": "integer", "description": "click this element index first to focus it" },
                    "method": { "type": "string", "enum": ["page", "system"], "description": "default page; system = real clipboard + Ctrl+V (browser must be focused)" },
                    "restore": { "type": "boolean", "description": "method system only: put the previous clipboard back afterwards (default true)" },
                    "tab": { "type": "string" }
                }
            }
        },
        {
            "name": "images",
            "description": "List the images on the page with their source URL and natural size. Download one with download{i}.",
            "inputSchema": { "type": "object", "properties": { "tab": { "type": "string" } } }
        },
        {
            "name": "clip",
            "description": "An agent-owned clipboard that never touches the system one. set/copy {text}, get {id?}, list {limit}, clear, and toSystem (the explicit opt-in when a real Ctrl+V is the only way). Because it is a file, a copy here cannot clobber what the human last copied.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["set", "copy", "get", "list", "clear", "toSystem"] },
                    "text": { "type": "string" },
                    "label": { "type": "string" },
                    "id": { "type": "integer" },
                    "limit": { "type": "integer" }
                },
                "required": ["action"]
            }
        },
        {
            "name": "link",
            "description": "Links: {action:\"open\"} opens a link (or the current page) in a new tab, {action:\"copy\"} stores its URL in the agent clipboard. Target with {i} from elements (any element inside an <a href>), or {url}; omit both for the current page.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["open", "copy"] },
                    "i": { "type": "integer" },
                    "url": { "type": "string" },
                    "foreground": { "type": "boolean", "description": "bring the new tab to the front. Default false: focus stays where the human left it." },
                    "tab": { "type": "string" }
                },
                "required": ["action"]
            }
        },
        {
            "name": "image",
            "description": "Work with an image on the page: {action:\"open\"} opens it in a new tab, {action:\"link\"} copies its URL to the clipboard, {action:\"copy\"} puts the pixels on the clipboard so a real Ctrl+V can paste them into Gemini/Docs/chat. Target with {i} from `images` or {url}.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["open", "link", "copy"] },
                    "i": { "type": "integer", "description": "index from images" },
                    "url": { "type": "string" },
                    "foreground": { "type": "boolean", "description": "bring the new tab to the front. Default false: focus stays where the human left it." },
                    "tab": { "type": "string" }
                },
                "required": ["action"]
            }
        },
        {
            "name": "download",
            "description": "Download a resource to a local path, fetched inside the page so cookies and referer are correct (authenticated images work). Choose the target with url, or with i = an index from images.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "url": { "type": "string", "description": "absolute or relative URL to fetch" },
                    "i": { "type": "integer", "description": "index from images (alternative to url)" },
                    "maxBytes": { "type": "integer", "description": "refuse larger files (default 10 MiB)" },
                    "tab": { "type": "string" }
                },
                "required": ["path"]
            }
        },
        {
            "name": "profile",
            "description": "Read or change this agent's settings (a small JSON file). Keys: screenshot_dir, shot_format (png|jpeg|webp), shot_quality. Use set once, and shot() will keep writing there.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["show", "set", "remember", "history"], "description": "show: all settings; set: {key,value}; remember: {note} appends to a capped history; history: {limit} reads it" },
                    "key": { "type": "string" },
                    "value": {},
                    "note": { "type": "string", "description": "for remember" },
                    "limit": { "type": "integer", "description": "for history, default 20" }
                },
                "required": ["action"]
            }
        },
        {
            "name": "shot",
            "description": "Screenshot the tab: the viewport by default, the whole page, a region, or one element by index. `path` is optional: omit it and the file lands in this profile's screenshot_dir with a timestamped name. `i` resolves in the main document, open shadow roots and same-origin frames (not cross-origin iframes), scrolls the element into view and captures just its box; `padding` widens that box.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "tab": { "type": "string" },
                    "mode": { "type": "string", "enum": ["viewport", "full", "custom"],
                              "description": "viewport (default) = what you see; full = whole page, height-capped at 8000px; custom = region {x,y,w,h}" },
                    "region": { "type": "object", "description": "for mode=custom: {x,y,w,h} in CSS pixels",
                                "properties": { "x": {"type":"number"}, "y": {"type":"number"}, "w": {"type":"number"}, "h": {"type":"number"} } },
                    "format": { "type": "string", "enum": ["png", "jpeg", "webp"], "description": "default: the profile's shot_format, else png" },
                    "quality": { "type": "integer", "description": "jpeg/webp quality 0-100 (default 85)" },
                    "i": { "type": "integer", "description": "capture this element's box, from elements; scrolls it into view first" },
                    "within": { "type": "string", "description": "scope for i; defaults to the last elements scope for this tab" },
                    "padding": { "type": "number", "description": "extra pixels around an element capture, 0..200 (default 0)" }
                }
            }
        }
    ])
}

/// What CDP needs to know about one key: DOM `key`, physical `code`, Windows
/// virtual key code, and the text it types (None for keys that type nothing).
fn key_spec(name: &str) -> (String, String, i64, Option<String>) {
    let named = |key: &str, code: &str, vk: i64, text: Option<&str>| {
        (key.to_string(), code.to_string(), vk, text.map(str::to_string))
    };
    match name.to_lowercase().as_str() {
        "enter" | "return" => named("Enter", "Enter", 13, Some("\r")),
        "tab" => named("Tab", "Tab", 9, None),
        "escape" | "esc" => named("Escape", "Escape", 27, None),
        "backspace" => named("Backspace", "Backspace", 8, None),
        "delete" | "del" => named("Delete", "Delete", 46, None),
        "insert" => named("Insert", "Insert", 45, None),
        "space" | " " => named(" ", "Space", 32, Some(" ")),
        "arrowup" | "up" => named("ArrowUp", "ArrowUp", 38, None),
        "arrowdown" | "down" => named("ArrowDown", "ArrowDown", 40, None),
        "arrowleft" | "left" => named("ArrowLeft", "ArrowLeft", 37, None),
        "arrowright" | "right" => named("ArrowRight", "ArrowRight", 39, None),
        "home" => named("Home", "Home", 36, None),
        "end" => named("End", "End", 35, None),
        "pageup" => named("PageUp", "PageUp", 33, None),
        "pagedown" => named("PageDown", "PageDown", 34, None),
        f if f.len() >= 2 && f.starts_with('f') && f[1..].parse::<u8>().is_ok_and(|n| (1..=12).contains(&n)) => {
            let n: i64 = f[1..].parse().unwrap_or(1);
            let k = format!("F{n}");
            (k.clone(), k, 111 + n, None)
        }
        _ => {
            let mut chars = name.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if c.is_ascii_alphabetic() => {
                    let up = c.to_ascii_uppercase();
                    (c.to_string(), format!("Key{up}"), up as i64, Some(c.to_string()))
                }
                (Some(c), None) if c.is_ascii_digit() => {
                    (c.to_string(), format!("Digit{c}"), c as i64, Some(c.to_string()))
                }
                (Some(c), None) => (c.to_string(), String::new(), 0, Some(c.to_string())),
                _ => (name.to_string(), name.to_string(), 0, None),
            }
        }
    }
}

/// Press and release one key, with modifiers given as "Control+Shift+z".
/// Returns the modifier mask that was applied.
fn press(conn: &mut cdp::Conn, combo: &str) -> Result<i64, String> {
    // "+" alone, or a trailing "++", means the plus key itself.
    let (mods, base) = if combo == "+" {
        ("", "+")
    } else if let Some(m) = combo.strip_suffix("++") {
        (m, "+")
    } else {
        combo.rsplit_once('+').unwrap_or(("", combo))
    };
    let mut mask = 0;
    for m in mods.split('+').filter(|m| !m.is_empty()) {
        mask |= match m.to_lowercase().as_str() {
            "alt" | "option" => 1,
            "control" | "ctrl" => 2,
            "meta" | "cmd" | "command" | "super" => 4,
            "shift" => 8,
            other => return Err(format!("unknown modifier {other:?} in {combo:?}")),
        };
    }
    let (key, code, vk, mut text) = key_spec(base);
    // A shortcut types nothing; Shift alone types the shifted character.
    if mask & (1 | 2 | 4) != 0 {
        text = None;
    } else if mask & 8 != 0 {
        text = text.map(|t| t.to_uppercase());
    }
    let key = match (&text, mask & 8 != 0) {
        (Some(t), true) if key.chars().count() == 1 => t.clone(),
        _ => key,
    };
    let mut down = json!({
        "type": if text.is_some() { "keyDown" } else { "rawKeyDown" },
        "modifiers": mask, "key": key, "code": code,
        "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk,
    });
    if let Some(t) = &text {
        down["text"] = json!(t);
        down["unmodifiedText"] = json!(t);
    }
    let up = json!({
        "type": "keyUp", "modifiers": mask, "key": key, "code": code,
        "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk,
    });
    conn.call_many(&[("Input.dispatchKeyEvent", down), ("Input.dispatchKeyEvent", up)])?;
    Ok(mask)
}

fn click_at(conn: &mut cdp::Conn, x: f64, y: f64) -> Result<(), String> {
    // The three events are pipelined: the browser still sees them in order, but
    // there is one round trip instead of three.
    conn.call_many(&[
        ("Input.dispatchMouseEvent", json!({ "type": "mouseMoved", "x": x, "y": y })),
        ("Input.dispatchMouseEvent", json!({ "type": "mousePressed", "x": x, "y": y,
                                            "button": "left", "clickCount": 1, "buttons": 1 })),
        ("Input.dispatchMouseEvent", json!({ "type": "mouseReleased", "x": x, "y": y,
                                            "button": "left", "clickCount": 1, "buttons": 0 })),
    ])?;
    Ok(())
}


/// A click with a non-left button, a click count or modifiers.
fn click_precise(conn: &mut cdp::Conn, x: f64, y: f64, button: &str, count: u64, mods: i64) -> Result<(), String> {
    let held = match button { "right" => 2, "middle" => 4, _ => 1 };
    conn.call_many(&[
        ("Input.dispatchMouseEvent", json!({ "type": "mouseMoved", "x": x, "y": y, "modifiers": mods })),
        ("Input.dispatchMouseEvent", json!({ "type": "mousePressed", "x": x, "y": y,
            "button": button, "clickCount": count, "buttons": held, "modifiers": mods })),
        ("Input.dispatchMouseEvent", json!({ "type": "mouseReleased", "x": x, "y": y,
            "button": button, "clickCount": count, "buttons": 0, "modifiers": mods })),
    ])?;
    Ok(())
}

/// Click element `i`: resolved, scrolled into view and measured in one
/// evaluation, then clicked. Returns what `pick` saw.
fn click_index(conn: &mut cdp::Conn, scope: Option<&str>, i: u64) -> Result<Value, String> {
    let at = pick(conn, scope, i, true, None)?;
    let x = at.get("x").and_then(Value::as_f64).unwrap_or(0.0);
    let y = at.get("y").and_then(Value::as_f64).unwrap_or(0.0);
    click_at(conn, x, y)?;
    Ok(at)
}

/// The log entry for a click by index: where it went, and a warning when
/// something else was on top of the target.
fn click_log(at: &Value) -> Value {
    let mut c = json!({ "i": at.get("i"), "x": at.get("x"), "y": at.get("y") });
    if at.get("scrolled").and_then(Value::as_bool) == Some(true) {
        c["scrolled"] = json!(true);
    }
    if at.get("hit").and_then(Value::as_bool) == Some(false) {
        c["covered"] = at.get("coveredBy").cloned().unwrap_or(json!(true));
    }
    c
}


/* -------------------------------- capture -------------------------------- */

fn b64encode(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { A[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { A[n as usize & 63] as char } else { '=' });
    }
    out
}

/// mode: "viewport" (what you see), "full" (whole document, height-capped) or
/// "custom" (a region in CSS pixels).
fn capture_png(
    conn: &mut cdp::Conn,
    mode: &str,
    region: Option<(f64, f64, f64, f64)>,
    max_w: Option<f64>,
    format: &str,
    quality: u32,
) -> Result<(Vec<u8>, bool, String), String> {
    let mut params = json!({ "format": format });
    if format == "jpeg" || format == "webp" {
        params["quality"] = json!(quality.min(100));
        // Trade a little file size for encode speed; a human glances at it.
        params["optimizeForSpeed"] = json!(true);
    }
    let mut clipped = false;

    match mode {
        "full" => {
            let metrics = conn.call("Page.getLayoutMetrics", json!({}))?;
            let size = metrics
                .get("cssContentSize")
                .or_else(|| metrics.get("contentSize"))
                .cloned()
                .unwrap_or(Value::Null);
            let w = size.get("width").and_then(Value::as_f64).unwrap_or(1280.0);
            let h_raw = size.get("height").and_then(Value::as_f64).unwrap_or(900.0);
            let h = h_raw.min(8000.0);
            clipped = h_raw > h;
            let scale = max_w.map(|m| (m / w).min(1.0)).unwrap_or(1.0);
            params["clip"] = json!({ "x": 0.0, "y": 0.0, "width": w, "height": h, "scale": scale });
            params["captureBeyondViewport"] = json!(true);
        }
        "custom" => {
            let (x, y, w, h) = region.ok_or("custom capture needs region {x,y,w,h}")?;
            let scale = max_w.map(|m| (m / w).min(1.0)).unwrap_or(1.0);
            params["clip"] = json!({ "x": x, "y": y, "width": w, "height": h, "scale": scale });
            params["captureBeyondViewport"] = json!(true);
        }
        _ => {
            if let Some(m) = max_w {
                let metrics = conn.call("Page.getLayoutMetrics", json!({}))?;
                let vp = metrics.get("cssVisualViewport").cloned().unwrap_or(Value::Null);
                let w = vp.get("clientWidth").and_then(Value::as_f64).unwrap_or(0.0);
                let h = vp.get("clientHeight").and_then(Value::as_f64).unwrap_or(0.0);
                let x = vp.get("pageX").and_then(Value::as_f64).unwrap_or(0.0);
                let y = vp.get("pageY").and_then(Value::as_f64).unwrap_or(0.0);
                if w > 0.0 && h > 0.0 && m < w {
                    params["clip"] = json!({ "x": x, "y": y, "width": w, "height": h, "scale": (m / w).min(1.0) });
                    params["captureBeyondViewport"] = json!(true);
                }
            }
        }
    }

    let r = conn.call("Page.captureScreenshot", params)?;
    let b64 = r.get("data").and_then(Value::as_str).ok_or("screenshot returned no data")?;
    let mime = mime_for(format).to_string();
    Ok((b64decode(b64)?, clipped, mime))
}

/// The capture box for an element, widened by padding but never past the
/// document origin: CDP rejects a clip that starts before (0, 0).
fn padded_box(x: f64, y: f64, w: f64, h: f64, padding: f64) -> (f64, f64, f64, f64) {
    let px = (x - padding).max(0.0);
    let py = (y - padding).max(0.0);
    (px, py, (x + w + padding) - px, (y + h + padding) - py)
}

fn parse_region(v: Option<&Value>) -> Option<(f64, f64, f64, f64)> {
    let r = v?;
    Some((r.get("x")?.as_f64()?, r.get("y")?.as_f64()?, r.get("w")?.as_f64()?, r.get("h")?.as_f64()?))
}

/// Resolve `{i}` or `{x,y}` to a screen point. An index is scrolled into
/// view first, so the point is on screen where input can reach it.
fn point_of(conn: &mut cdp::Conn, scope: Option<&str>, v: &Value) -> Result<(f64, f64), String> {
    if let Some(i) = v.get("i").and_then(Value::as_u64) {
        let at = pick(conn, scope, i, true, None)?;
        return Ok((
            at.get("x").and_then(Value::as_f64).unwrap_or(0.0),
            at.get("y").and_then(Value::as_f64).unwrap_or(0.0),
        ));
    }
    let x = v.get("x").and_then(Value::as_f64).ok_or("point needs i or x,y")?;
    let y = v.get("y").and_then(Value::as_f64).ok_or("point needs i or x,y")?;
    Ok((x, y))
}

/// Modifier bitmask for CDP: Alt=1, Ctrl=2, Meta=4, Shift=8.
fn mod_mask(v: Option<&Value>) -> i64 {
    let mut m = 0;
    if let Some(arr) = v.and_then(Value::as_array) {
        for x in arr.iter().filter_map(Value::as_str) {
            match x.to_lowercase().as_str() {
                "alt" => m |= 1,
                "control" | "ctrl" => m |= 2,
                "meta" | "cmd" | "super" => m |= 4,
                "shift" => m |= 8,
                _ => {}
            }
        }
    }
    m
}

/// A pointer move in small steps: pages that track drag gestures (canvas, lists,
/// sliders) ignore a single jump from A to B.
fn drag_path(conn: &mut cdp::Conn, from: (f64, f64), to: (f64, f64), steps: u32) -> Result<(), String> {
    let steps = steps.clamp(2, 40);
    // One round trip per move, deliberately. Pipelining the whole path into a
    // single turn is 5x faster but Chromium coalesces mouse-moved events that
    // arrive together: measured on a 9-point path, the page saw 2 points and a
    // release. A slider, a canvas or a signature field needs the path, so each
    // step keeps its own turn. `steps` is the knob when a slow page makes the
    // cost visible (a tab with a <video> element measured ~100-500 ms a step).
    conn.call_many(&[("Input.dispatchMouseEvent",
        json!({ "type": "mouseMoved", "x": from.0, "y": from.1 }))])?;
    conn.call_many(&[("Input.dispatchMouseEvent",
        json!({ "type": "mousePressed", "x": from.0, "y": from.1,
                "button": "left", "clickCount": 1, "buttons": 1 }))])?;
    for s in 1..=steps {
        let f = s as f64 / steps as f64;
        conn.call_many(&[("Input.dispatchMouseEvent",
            json!({ "type": "mouseMoved",
                    "x": from.0 + (to.0 - from.0) * f,
                    "y": from.1 + (to.1 - from.1) * f,
                    "button": "left", "buttons": 1 }))])?;
    }
    conn.call_many(&[("Input.dispatchMouseEvent",
        json!({ "type": "mouseReleased", "x": to.0, "y": to.1,
                "button": "left", "clickCount": 1, "buttons": 0 }))])?;
    Ok(())
}

/// JS: does the page's text contain `want`? Evaluated in the page, so only a
/// boolean crosses the socket instead of the whole body text.
fn has_text_js(want: &str) -> String {
    // `innerText` only: `textContent` includes inline <script> source, so a wait
    // for a word the page's own code mentions matched before it was ever shown.
    format!(
        "(() => {{ const w = {w}; const b = document.body; \
         return !!b && (b.innerText || '').includes(w); }})()",
        w = Value::String(want.to_string())
    )
}

/// Did the editor take the paste after all? Compared against the snapshot the
/// page stored when a handler cancelled the event and nothing had changed yet.
const PASTE_LANDED_JS: &str = r#"(() => {
    let el = document.activeElement || document.body;
    while (el && el.shadowRoot && el.shadowRoot.activeElement) el = el.shadowRoot.activeElement;
    const read = (n) => (n && n.isContentEditable) ? ((n.innerText) || '')
        : ((n && typeof n.value === 'string') ? n.value : '');
    const sel = (n) => {
        if (n && typeof n.selectionStart === 'number') return n.selectionEnd > n.selectionStart ? 'range' : 'caret';
        const s = window.getSelection();
        return s && !s.isCollapsed ? 'range' : 'caret';
    };
    const p = window.__cdpPaste;
    delete window.__cdpPaste;
    if (!p) return { changed: false };
    return { changed: read(el) !== p.text || (p.sel === 'range' && sel(el) === 'caret') };
})()"#;

/// Snapshot the focused editor before an insert, so the next
/// `PASTE_LANDED_JS` can tell whether the text actually landed.
const PASTE_SNAP_JS: &str = r#"(() => {
    let el = document.activeElement || document.body;
    while (el && el.shadowRoot && el.shadowRoot.activeElement) el = el.shadowRoot.activeElement;
    const read = (n) => (n && n.isContentEditable) ? ((n.innerText) || '')
        : ((n && typeof n.value === 'string') ? n.value : '');
    const sel = (n) => {
        if (n && typeof n.selectionStart === 'number') return n.selectionEnd > n.selectionStart ? 'range' : 'caret';
        const s = window.getSelection();
        return s && !s.isCollapsed ? 'range' : 'caret';
    };
    window.__cdpPaste = { text: read(el), sel: sel(el) };
    return true;
})()"#;

/// Scroll by `dy`, in one evaluation. The document first; when it does not
/// move (an app whose content scrolls inside a container), the scrollable
/// container under the middle of the viewport, like a wheel would.
/// A multiple selection matches when the sets are equal. The page reports its
/// options in its own order, and a repeated request value is the same request.
fn selection_matches(want: &[String], got: &Value) -> bool {
    let mut want: Vec<&str> = want.iter().map(String::as_str).collect();
    let mut have: Vec<&str> = got
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    want.sort_unstable();
    want.dedup();
    have.sort_unstable();
    have.dedup();
    want == have
}

/// `select.value` as the action takes it: one string, or a non-empty array of
/// strings for a multiple select.
fn select_values(v: Option<&Value>) -> Result<Value, String> {
    match v {
        Some(Value::String(s)) => Ok(Value::String(s.clone())),
        Some(Value::Array(a)) if !a.is_empty() => {
            let mut out = Vec::with_capacity(a.len());
            for x in a {
                out.push(Value::String(
                    x.as_str().ok_or("select values must be strings")?.to_string(),
                ));
            }
            Ok(Value::Array(out))
        }
        Some(Value::Array(_)) => Err("select needs at least one value".into()),
        Some(_) => Err("select value must be a string or an array of strings".into()),
        None => Err("select needs value".into()),
    }
}

fn run_input(conn: &mut cdp::Conn, tab: &str, actions: &[Value], force: bool, scope: Option<&str>) -> Result<Value, String> {
    let mut log = Vec::new();
    for a in actions {
        check_cancel()?;
        let obj = a.as_object().ok_or("each action must be an object")?;
        if let Some(d) = obj.get("dialog") {
            // The one action allowed while the renderer is blocked. One answer
            // per call: a chained alert is reported, not auto-dismissed.
            let accept = d.get("accept").and_then(Value::as_bool).unwrap_or(true);
            let text = d.get("text").and_then(Value::as_str);
            let next = conn.handle_dialog(accept, text)?;
            log.push(json!({ "dialogAnswered": { "accept": accept } }));
            // A chained dialog can open as the first one closes: its event may
            // land just after the reply, so read what has arrived before
            // reporting. Answering it is the caller's decision, not ours.
            let _ = conn.drain_arrived();
            if next.is_some() || conn.dialog.is_some() {
                return Ok(json!(log));
            }
        } else if let Some(c) = obj.get("click") {
            let button = c.get("button").and_then(Value::as_str).unwrap_or("left").to_string();
            let count = c.get("count").and_then(Value::as_u64).unwrap_or(1);
            let mods = mod_mask(c.get("modifiers"));
            if let Some(i) = c.get("i").and_then(Value::as_u64) {
                // A per-action scope replaces the ambient one; an empty selector
                // is no scope at all, not a scope that matches everything.
                let scope = match c.get("within").and_then(Value::as_str) {
                    Some(w) if w.trim().is_empty() => None,
                    Some(w) => Some(w.trim()),
                    None => scope,
                };
                // An element inside a cross-origin frame lives in another
                // renderer: a click at its page coordinates lands in the frame's
                // box but the frame's own document receives it at LOCAL
                // coordinates, so the event has to be sent to its session.
                if let Some(target) = frame_target(conn, tab, scope, i, force)? {
                    let (lx, ly) = (target.0, target.1);
                    let clicked = if button != "left" || count > 1 || mods != 0 {
                        frame_click(conn, &target.2, lx, ly, &button, count, mods as u32)
                    } else {
                        frame_click(conn, &target.2, lx, ly, "left", 1, 0)
                    };
                    // A handler that opened a dialog blocks the renderer, so the
                    // click's outcome is not known yet: report the dialog instead
                    // of an error, and mark the entry as unsettled.
                    if let Err(e) = clicked {
                        if conn.dialog.is_some() {
                            log.push(json!({ "clicked": { "i": i, "inFrame": true, "x": lx, "y": ly, "dialog": true } }));
                            return Ok(json!(log));
                        }
                        return Err(e);
                    }
                    log.push(json!({ "clicked": { "i": i, "inFrame": true, "x": lx, "y": ly } }));
                    continue;
                }
                // Resolve first (one evaluation), then check the label before
                // clicking: a repeat of a final confirmation is the failure that
                // bites an unattended run.
                let at = pick(conn, scope, i, true, None)?;
                if !force {
                    let reason = if at.get("connected").and_then(Value::as_bool) == Some(false) {
                        Some("detached")
                    } else if at.get("zero").and_then(Value::as_bool) == Some(true) {
                        Some("zero_size")
                    } else if at.get("disabled").and_then(Value::as_bool) == Some(true) {
                        Some("disabled")
                    } else if at.get("hit").and_then(Value::as_bool) == Some(false) {
                        Some("obscured")
                    } else {
                        None
                    };
                    if let Some(reason) = reason {
                        return Err(blocked_click(
                            reason,
                            i,
                            at.get("tag").unwrap_or(&Value::Null),
                            at.get("label").unwrap_or(&Value::Null),
                            at.get("blocker").cloned()
                                .or_else(|| at.get("coveredBy").cloned())
                                .unwrap_or(Value::Null),
                            false,
                        ));
                    }
                }
                if let Some(label) = at.get("label").and_then(Value::as_str) {
                    if irreversible(label) {
                        guard_irreversible(tab, label, i, force)?;
                    }
                }
                let x = at.get("x").and_then(Value::as_f64).unwrap_or(0.0);
                let y = at.get("y").and_then(Value::as_f64).unwrap_or(0.0);
                let clicked = if button != "left" || count > 1 || mods != 0 {
                    click_precise(conn, x, y, &button, count, mods)
                } else {
                    click_at(conn, x, y)
                };
                if let Err(e) = clicked {
                    if conn.dialog.is_some() {
                        log.push(json!({ "clicked": { "i": i, "dialog": true } }));
                        return Ok(json!(log));
                    }
                    return Err(e);
                }
                log.push(json!({ "clicked": click_log(&at) }));
            } else {
                let (x, y) = point_of(conn, scope, c)?;
                let clicked = if button != "left" || count > 1 || mods != 0 {
                    click_precise(conn, x, y, &button, count, mods)
                } else {
                    click_at(conn, x, y)
                };
                if let Err(e) = clicked {
                    if conn.dialog.is_some() {
                        log.push(json!({ "clicked": { "x": x, "y": y, "dialog": true } }));
                        return Ok(json!(log));
                    }
                    return Err(e);
                }
                log.push(json!({ "clicked": { "x": x, "y": y, "button": button, "count": count, "modifiers": mods } }));
            }
        } else if let Some(t) = obj.get("insert").and_then(Value::as_str) {
            conn.call("Input.insertText", json!({ "text": t }))?;
            log.push(json!({ "inserted": t.chars().count() }));
        } else if let Some(k) = obj.get("key").and_then(Value::as_str) {
            // Enter can submit a form, and a submit handler can open a dialog.
            let mask = match press(conn, k) {
                Ok(m) => m,
                Err(e) => {
                    if conn.dialog.is_some() {
                        log.push(json!({ "key": k, "dialog": true }));
                        return Ok(json!(log));
                    }
                    return Err(e);
                }
            };
            if mask != 0 {
                log.push(json!({ "key": k, "modifiers": mask }));
            } else {
                log.push(json!({ "key": k }));
            }
        } else if let Some(s) = obj.get("scroll") {
            let (dx, dy) = scroll::deltas(
                s.get("dx").and_then(Value::as_f64),
                s.get("dy").and_then(Value::as_f64),
            )?;
            // A real wheel goes where the pointer is: an element, a point, or the
            // middle of the viewport for a plain page scroll.
            let (x, y) = if let Some(i) = s.get("i").and_then(Value::as_u64) {
                let at = pick(conn, scope, i, true, None)?;
                (
                    at.get("x").and_then(Value::as_f64).unwrap_or(0.0),
                    at.get("y").and_then(Value::as_f64).unwrap_or(0.0),
                )
            } else if let (Some(x), Some(y)) = (
                s.get("x").and_then(Value::as_f64),
                s.get("y").and_then(Value::as_f64),
            ) {
                (x, y)
            } else {
                let v = conn.eval("[innerWidth / 2, innerHeight / 2]")?;
                (
                    v.get(0).and_then(Value::as_f64).unwrap_or(0.0),
                    v.get(1).and_then(Value::as_f64).unwrap_or(0.0),
                )
            };
            log.push(json!({ "scrolled": scroll::send(conn, x, y, dx, dy)? }));
        } else if let Some(d) = obj.get("dismiss") {
            let prefer = d.get("prefer").and_then(Value::as_str).unwrap_or("reject").to_lowercase();
            // Closer indexes come from a whole-document scan, so they are
            // clicked in that same (unscoped) scan below.
            let scan_v = conn.eval(&scan_js(None))?;
            let overlays = scan_v.get("overlays").and_then(Value::as_array).cloned().unwrap_or_default();
            let mut cands: Vec<(String, i64)> = Vec::new();
            for o in &overlays {
                if let Some(cs) = o.get("closers").and_then(Value::as_array) {
                    for c in cs {
                        if let (Some(l), Some(idx)) = (
                            c.get("label").and_then(Value::as_str),
                            c.get("i").and_then(Value::as_i64),
                        ) {
                            cands.push((l.to_lowercase(), idx));
                        }
                    }
                }
            }
            if cands.is_empty() {
                log.push(json!({ "dismissed": null, "overlays": overlays.len(),
                                 "note": "no dismiss buttons found" }));
                continue;
            }
            // Policy: refuse cookies unless refusing is not offered. "Only
            // necessary" and "decline" are refusals; "accept all" is the last resort.
            let refuse = ["reject", "decline", "block", "deny", "refuse", "necessary", "essential",
                          "only needed", "do not sell", "manage"];
            let acceptish = ["accept", "agree", "allow", "got it", "ok", "continue", "close",
                             "dismiss", "later", "not now", "×", "✕"];
            let want: &[&str] = if prefer == "accept" { &acceptish } else { &refuse };
            let pick_c = cands
                .iter()
                .find(|(l, _)| want.iter().any(|w| l.contains(w)))
                .or_else(|| cands.iter().find(|(l, _)| acceptish.iter().any(|w| l.contains(w))));
            match pick_c {
                Some((label, idx)) => {
                    let i = *idx as u64;
                    click_index(conn, None, i)?;
                    log.push(json!({ "dismissed": { "label": label, "i": i, "preference": prefer } }));
                }
                None => log.push(json!({ "dismissed": null,
                                         "candidates": cands.iter().map(|(l, _)| l.clone()).collect::<Vec<_>>() })),
            }
        } else if let Some(h) = obj.get("hover") {
            let (x, y) = point_of(conn, scope, h)?;
            conn.call_many(&[("Input.dispatchMouseEvent", json!({ "type": "mouseMoved", "x": x, "y": y }))])?;
            log.push(json!({ "hovered": { "x": x, "y": y } }));
        } else if let Some(d) = obj.get("drag") {
            let from = point_of(conn, scope, d.get("from").ok_or("drag needs from")?)?;
            let to = point_of(conn, scope, d.get("to").ok_or("drag needs to")?)?;
            let steps = d.get("steps").and_then(Value::as_u64).unwrap_or(12) as u32;
            drag_path(conn, from, to, steps)?;
            log.push(json!({ "dragged": { "from": from, "to": to, "steps": steps } }));
        } else if let Some(tp) = obj.get("tap") {
            // Touch input, for pages that only respond to touch (mobile layouts,
            // canvas apps). One finger, press and release at the same point.
            let (x, y) = point_of(conn, scope, tp)?;
            conn.call_many(&[
                ("Input.dispatchTouchEvent", json!({ "type": "touchStart",
                    "touchPoints": [{ "x": x, "y": y, "radiusX": 12, "radiusY": 12, "force": 1 }] })),
                ("Input.dispatchTouchEvent", json!({ "type": "touchEnd", "touchPoints": [] })),
            ])?;
            log.push(json!({ "tapped": { "x": x, "y": y } }));
        } else if let Some(s) = obj.get("swipe") {
            let from = point_of(conn, scope, s.get("from").ok_or("swipe needs from")?)?;
            let to = point_of(conn, scope, s.get("to").ok_or("swipe needs to")?)?;
            let steps = s.get("steps").and_then(Value::as_u64).unwrap_or(10).clamp(1, 40);
            // One round trip per move, like drag_path: moves that arrive
            // together are coalesced (measured: 10 sent, the page saw 2), and a
            // gesture recogniser needs the path, not just the ends.
            conn.call("Input.dispatchTouchEvent", json!({ "type": "touchStart",
                "touchPoints": [{ "x": from.0, "y": from.1, "radiusX": 12, "radiusY": 12, "force": 1 }] }))?;
            for n in 1..=steps {
                let f = n as f64 / steps as f64;
                let x = from.0 + (to.0 - from.0) * f;
                let y = from.1 + (to.1 - from.1) * f;
                conn.call("Input.dispatchTouchEvent", json!({ "type": "touchMove",
                    "touchPoints": [{ "x": x, "y": y, "radiusX": 12, "radiusY": 12, "force": 1 }] }))?;
            }
            conn.call("Input.dispatchTouchEvent", json!({ "type": "touchEnd", "touchPoints": [] }))?;
            log.push(json!({ "swiped": { "from": from, "to": to } }));
        } else if let Some(v) = obj.get("select") {
            // <select> by index: setting .value directly works where a click menu
            // would need a second overlay navigation.
            let i = v.get("i").and_then(Value::as_u64).ok_or("select needs i")?;
            let value = select_values(v.get("value"))?;
            let at = pick(conn, scope, i, false, Some(&value))?;
            if let Some(e) = at.get("selectError").and_then(Value::as_str) {
                return Err(format!("select failed: {e}"));
            }
            let got = at.get("selected").cloned().unwrap_or(Value::Null);
            match &value {
                Value::Array(want) => {
                    let want: Vec<String> =
                        want.iter().filter_map(Value::as_str).map(str::to_string).collect();
                    if !selection_matches(&want, &got) {
                        return Err(format!("select failed: asked for {want:?}, the page has {got}"));
                    }
                    log.push(json!({ "selected": { "i": i, "value": got } }));
                }
                Value::String(want) => {
                    if got.as_str() != Some(want.as_str()) {
                        return Err(format!(
                            "select failed: the <select> has no option {want:?} (it is {got})"
                        ));
                    }
                    log.push(json!({ "selected": { "i": i, "value": got } }));
                }
                _ => {}
            }
        } else if let Some(w) = obj.get("wait") {
            let ms = w.get("ms").and_then(Value::as_u64).unwrap_or(15_000);
            if w.get("idle").and_then(Value::as_bool).unwrap_or(false) {
                let started = std::time::Instant::now();
                let n = wait_idle(conn, ms, 300)?;
                log.push(json!({ "waitedFor": "idle", "resources": n,
                                 "ms": started.elapsed().as_millis() as u64 }));
                continue;
            }
            let text = w.get("text").and_then(Value::as_str).ok_or("wait needs text, or idle:true")?;
            let started = std::time::Instant::now();
            if !wait_for_text(conn, text, ms)? {
                return Err(format!("wait: text not found within {ms}ms: {text}"));
            }
            log.push(json!({ "waitedFor": text, "ms": started.elapsed().as_millis() as u64 }));
        } else if let Some(s) = obj.get("sleep") {
            let ms = s.get("ms").and_then(Value::as_u64).unwrap_or(300);
            nap(ms)?;
            log.push(json!({ "slept": ms }));
        } else {
            return Err(format!("unknown action: {a}"));
        }
    }
    Ok(json!(log))
}

/// One `input` call: the actions, the optional settle window, and the fields
/// that say what the page did. Split out so the dialog-aware connection guard
/// can be chosen without duplicating the body.
fn run_batch(
    conn: &mut cdp::Conn,
    tab_id: &str,
    actions: &[Value],
    announce: bool,
    force: bool,
    settle_ms: u64,
) -> Result<Value, String> {
    let scope = scope_for(tab_id);
    let mut navigated = false;
    let mut opened_tab: Option<Value> = None;
    let mut note: Option<String> = None;
    // A batch that answers a dialog must not read the page first, and neither
    // must any batch while one is open: the renderer is blocked. The browser
    // process still answers Target.getTargetInfo.
    // Only a page that is already blocked reaches here with a dialog action
    // first (with_conn_dialog is only used for that shape), so the live state is
    // the whole answer.
    let blocked_before = conn.dialog.is_some();
    // A settle window is opt-in: enabling Network on every click would add
    // latency to the common case, and most actions are not being debugged.
    let (log, errors, downloads) = if settle_ms > 0 {
        // A blocked renderer cannot answer `Runtime.enable`/`Network.enable`
        // either: measured, arming receipts there dismissed the dialog before
        // the answer was sent. Leave them off; the post-action URL comes from
        // the browser process instead.
        let url = if blocked_before {
            String::new()
        } else {
            conn.eval("location.href")
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default()
        };
        let on = !url.is_empty() && conn.begin_receipts(&url, tab_id).is_ok();
        conn.contexts_cleared = false;
        let result = run_input(conn, tab_id, actions, force, scope.as_deref());
        let outcome = if on {
            if result.is_err() {
                // Nothing was dispatched cleanly; do not make the caller wait
                // out the quiet window as well.
                conn.settle_receipts(0, 0)
            } else {
                conn.settle_receipts(250, settle_ms)
            }
        } else {
            cdp::ReceiptOutcome::default()
        };
        navigated = conn.contexts_cleared;
        opened_tab = outcome.opened_tab;
        let downloads = outcome.downloads;
        let errors = outcome.entries;
        let log = match result {
            Ok(v) => v,
            // An action this wrapper does not itemise can still be interrupted
            // by a dialog; that is an outcome, not a tool error.
            Err(e) if conn.dialog.is_some() => {
                note = Some(format!("{e} — a dialog is open"));
                json!([])
            }
            // A click that navigated can fail on the trailing synthetic event.
            Err(e) if navigated => {
                note = Some(e);
                json!([])
            }
            Err(e) => return Err(e),
        };
        (log, errors, downloads)
    } else {
        let log = match run_input(conn, tab_id, actions, force, scope.as_deref()) {
            Ok(v) => v,
            Err(e) if conn.dialog.is_some() => {
                note = Some(format!("{e} — a dialog is open"));
                json!([])
            }
            Err(e) => return Err(e),
        };
        (log, Vec::new(), Vec::new())
    };
    // Post-action evals would sit behind a dialog; the browser-process URL works.
    let blocked_after = conn.dialog.is_some();
    let (overlays, mut url) = if announce && !blocked_after {
        // cheap post-action check: only what appeared because we acted
        let ov = conn.eval(scan::OVERLAYS_QUICK_JS).unwrap_or(json!([]));
        let u = conn.eval("location.href").unwrap_or(Value::Null);
        (ov, u)
    } else {
        (json!([]), Value::Null)
    };
    if navigated && !blocked_after {
        if let Ok(u) = conn.eval("location.href") {
            if let Some(s) = u.as_str() {
                url = json!(s);
            }
        }
    }
    if url.is_null() && blocked_after {
        if let Some(s) = conn.target_url() {
            url = json!(s);
        }
    }
    let mut out = json!({ "tab": tab_id, "url": url, "log": log, "overlays": overlays });
    if navigated {
        out["navigated"] = json!(true);
    }
    if let Some(t) = opened_tab {
        out["openedTab"] = t;
    }
    if let Some(n) = note {
        out["note"] = json!(n);
    }
    if !errors.is_empty() {
        out["errors"] = json!(errors);
    }
    if !downloads.is_empty() {
        // Started, not saved: the browser reports the beginning of a download,
        // not where it landed or whether it finished.
        out["downloads"] = json!(downloads);
    }
    if let Some(d) = conn.dialog.clone() {
        out["dialog"] = d;
    }
    Ok(out)
}

/// A step inherits the caller's tab unless it names one: `flow {tab}` used to
/// leave its steps on whatever tab the last call happened to touch.
fn inherit_tab(mut a: Value, tab: Option<&str>) -> Value {
    if let (Some(t), Some(o)) = (tab, a.as_object_mut()) {
        if !o.contains_key("tab") {
            o.insert("tab".into(), json!(t));
        }
    }
    a
}

fn call_tool(cfg: &Cfg, name: &str, args: &Value) -> Result<Value, String> {
    let tab = args.get("tab").and_then(Value::as_str);
    match name {
        "flow" => {
            // A sequence that runs *inside* the server: N browser steps for one
            // model turn. This is the token lever — not the transport. `batch`
            // does the same in one request; `flow` adds a repeat-until loop,
            // which is what heavy work actually needs (click "more" until the
            // item appears, without a turn per click).
            let steps = args.get("steps").and_then(Value::as_array).cloned().unwrap_or_default();
            if steps.is_empty() {
                return Err("flow needs steps[]".into());
            }
            let repeat = args.get("repeat").cloned().unwrap_or(Value::Null);
            let max_iter = repeat.get("max").and_then(Value::as_u64).unwrap_or(1).clamp(1, 200);
            let until_text = repeat.get("untilText").and_then(Value::as_str).map(str::to_string);
            let until_gone = repeat.get("untilGone").and_then(Value::as_str).map(str::to_string);
            let between = repeat.get("delayMs").and_then(Value::as_u64).unwrap_or(700);
            let stop_on_error = args.get("stopOnError").and_then(Value::as_bool).unwrap_or(true);

            let mut all = Vec::new();
            let started = std::time::Instant::now();
            let mut hit = false;
            for iteration in 1..=max_iter {
                check_cancel()?;
                let mut iteration_log = Vec::new();
                for step in &steps {
                    let name = step.get("tool").and_then(Value::as_str).unwrap_or("");
                    let a = inherit_tab(step.get("args").cloned().unwrap_or(json!({})), tab);
                    match call_tool(cfg, name, &a) {
                        Ok(v) => {
                            let dialog = v.get("dialog").cloned();
                            iteration_log.push(json!({ "tool": name, "ok": true, "result": v }));
                            // A dialog stops the flow: everything after it would
                            // run against a blocked page, and the caller has to
                            // decide whether to accept it.
                            if let Some(d) = dialog {
                                all.push(json!({ "iteration": iteration, "log": iteration_log }));
                                return Ok(json!({
                                    "iterations": iteration,
                                    "elapsedMs": started.elapsed().as_millis() as u64,
                                    "stoppedOnDialog": true,
                                    "dialog": d,
                                    "trail": all,
                                }));
                            }
                        }
                        Err(e) => {
                            iteration_log.push(json!({ "tool": name, "ok": false, "error": e }));
                            if stop_on_error {
                                all.push(json!({ "iteration": iteration, "log": iteration_log }));
                                return Ok(json!({ "iterations": iteration, "elapsedMs": started.elapsed().as_millis() as u64,
                                                  "stoppedOnError": true, "trail": all }));
                            }
                        }
                    }
                }
                all.push(json!({ "iteration": iteration, "log": iteration_log }));

                // Did we reach the state we were waiting for?
                if until_text.is_some() || until_gone.is_some() {
                    // Asked in the page: only booleans cross the socket.
                    // A dialog is not "the text is absent": it blocks every later
                    // step, so the flow stops here instead of sleeping and asking
                    // a renderer that cannot answer.
                    let mut stopped: Option<Value> = None;
                    let mut page_has = |want: &str| -> Option<bool> {
                        match with_conn(&cfg.endpoint, tab, |conn, _| conn.eval(&has_text_js(want))) {
                            Ok(v) => Some(v.as_bool().unwrap_or(false)),
                            Err(e) => {
                                let raw = e.strip_prefix("ERROR: ").unwrap_or(&e);
                                if let Ok(b) = serde_json::from_str::<Value>(raw) {
                                    if let Some(d) = b.get("dialog") {
                                        stopped = Some(d.clone());
                                    }
                                }
                                None
                            }
                        }
                    };
                    if let Some(want) = &until_text {
                        if page_has(want) == Some(true) { hit = true; }
                    }
                    if let Some(gone) = &until_gone {
                        if page_has(gone) == Some(false) { hit = true; }
                    }
                    if let Some(d) = stopped {
                        return Ok(json!({
                            "iterations": iteration,
                            "elapsedMs": started.elapsed().as_millis() as u64,
                            "stoppedOnDialog": true,
                            "dialog": d,
                            "trail": all,
                        }));
                    }
                    if hit { break; }
                }
                if iteration < max_iter {
                    nap(between)?;
                }
            }
            let iterations = all.len();
            // Only the tail matters for the caller; the middle is noise.
            let keep = all.split_off(iterations.saturating_sub(3));
            Ok(json!({
                "iterations": iterations,
                "conditionMet": if until_text.is_some() || until_gone.is_some() { json!(hit) } else { Value::Null },
                "elapsedMs": started.elapsed().as_millis() as u64,
                "trail": keep,
            }))
        }
        "idle" => {
            // Two different limits, and conflating them was a bug: how long to
            // wait at all (`timeoutMs`), and how quiet the page must be before
            // describing it as idle (`maxIdleMs`). A page that keeps ticking
            // must not turn a 10-minute wait into an instant one.
            let timeout = args.get("timeoutMs").and_then(Value::as_u64).unwrap_or(600_000);
            let max_idle = args.get("maxIdleMs").and_then(Value::as_u64).unwrap_or(timeout);
            let until_text = args.get("untilText").and_then(Value::as_str).map(str::to_string);
            // Waiting for a thing to exist is the common case a text search
            // cannot express: a form that has mounted, a dialog that has opened.
            let until_selector = args.get("untilSelector").and_then(Value::as_str).map(str::to_string);
            // One connection for the whole wait, and a retry if it dies: the wait
            // clicks nothing, so starting again is always safe. The deadline is
            // fixed here so a retry spends what is left of it rather than
            // granting itself a fresh ten minutes.
            let started = std::time::Instant::now();
            let deadline = started + std::time::Duration::from_millis(timeout);
            let mut v = with_conn_readonly(&cfg.endpoint, tab, |conn, _| {
                wait_page_settled(
                    conn,
                    until_text.as_deref(),
                    until_selector.as_deref(),
                    started,
                    deadline,
                    max_idle,
                )
            })?;
            // Echo the question: `woke: "selector"` alone does not say which one.
            if let Some(t) = &until_text {
                v["untilText"] = json!(t);
            }
            if let Some(sel) = &until_selector {
                v["untilSelector"] = json!(sel);
            }
            Ok(v)
        }
        "report" => {
            let action = args.get("action").and_then(Value::as_str).unwrap_or("add");
            let file = data_dir().join("reports.jsonl");
            match action {
                "add" => {
                    let summary = args.get("summary").and_then(Value::as_str).ok_or("report needs summary")?;
                    let kind = args.get("kind").and_then(Value::as_str).unwrap_or("issue");
                    if !matches!(kind, "issue" | "idea") {
                        return Err("kind must be issue or idea".into());
                    }
                    // Context that makes a report actionable without the caller
                    // having to write an essay.
                    let mut where_ = Value::Null;
                    if let Some(tb) = args.get("tab").and_then(Value::as_str) {
                        if let Ok(target) = cdp::resolve(&cfg.endpoint, Some(tb)) {
                            where_ = json!({ "url": target.url, "title": target.title });
                        }
                    }
                    let ts = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0);
                    let rec = json!({
                        "id": ts,
                        "ts": ts,
                        "kind": kind,
                        "summary": summary,
                        "details": args.get("details").cloned().unwrap_or(Value::Null),
                        "tool": args.get("tool").cloned().unwrap_or(Value::Null),
                        "profile": cfg.profile.name,
                        "agent": cfg.profile_now().get_str("agent"),
                        "version": VERSION,
                        "where": where_,
                    });
                    if let Some(dir) = file.parent() {
                        std::fs::create_dir_all(dir)
                            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
                    }
                    use std::io::Write as _;
                    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&file)
                        .map_err(|e| format!("cannot open {}: {e}", file.display()))?;
                    writeln!(f, "{rec}").map_err(|e| e.to_string())?;
                    Ok(json!({ "reported": summary, "kind": kind, "id": ts, "path": file.display().to_string() }))
                }
                "list" => {
                    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
                    let body = std::fs::read_to_string(&file).unwrap_or_default();
                    let items: Vec<Value> = body
                        .lines()
                        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                        .collect();
                    let start = items.len().saturating_sub(limit);
                    Ok(json!({ "total": items.len(), "recent": &items[start..], "path": file.display().to_string() }))
                }
                other => Err(format!("unknown report action: {other}")),
            }
        }
        "batch" => {
            // Several tool calls in ONE request. The transport is already ~2 ms;
            // what actually costs time is the model's turn between calls, so the
            // win is doing a whole flow in a single exchange.
            let calls = args.get("calls").and_then(Value::as_array).ok_or("batch needs calls[]")?;
            if calls.len() > 20 {
                return Err("batch takes at most 20 calls".into());
            }
            let mut out = Vec::with_capacity(calls.len());
            for c in calls {
                let name = c.get("tool").and_then(Value::as_str).ok_or("each call needs a tool")?;
                if name == "batch" {
                    return Err("batch cannot nest".into());
                }
                let a = c.get("args").cloned().unwrap_or(json!({}));
                let started = std::time::Instant::now();
                match call_tool(cfg, name, &a) {
                    Ok(v) => out.push(json!({
                        "tool": name,
                        "ok": true,
                        "ms": started.elapsed().as_millis() as u64,
                        "result": v,
                    })),
                    Err(e) => out.push(json!({
                        "tool": name, "ok": false, "ms": started.elapsed().as_millis() as u64, "error": e,
                    })),
                }
            }
            Ok(json!({ "results": out }))
        }
        "status" => {
            let prof = cfg.profile_now();
            let timeout_ms = std::env::var("CDP_MCP_TIMEOUT_MS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(30_000);
            let base = json!({
                "version": VERSION,
                "endpoint": cfg.endpoint,
                "profile": { "name": prof.name, "path": prof.path.display().to_string(),
                             "agent": prof.get_str("agent") },
                "timeoutMs": timeout_ms,
            });
            // A doctor, not just a dump: when it cannot work, say why.
            match cdp::http_json(&cfg.endpoint, "/json/version", "GET") {
                Ok(version) => {
                    let tabs = cdp::targets(&cfg.endpoint).unwrap_or_default();
                    let mut out = base;
                    out["browser"] = version.get("Browser").cloned().unwrap_or(Value::Null);
                    out["protocol"] = version.get("Protocol-Version").cloned().unwrap_or(Value::Null);
                    let pages: Vec<_> = tabs.iter().filter(|t| !t.url.starts_with("chrome-extension://")).collect();
                    out["tabs"] = json!(pages.iter().map(|t| json!({ "tab": t.id, "title": t.title, "url": t.url })).collect::<Vec<_>>());
                    if tabs.is_empty() {
                        out["hint"] = json!("the browser is running but has no page tabs — open one with tabs {action:\"open\"}");
                    }
                    Ok(out)
                }
                Err(e) => Ok(json!({
                    "endpoint": cfg.endpoint,
                    "up": false,
                    "error": e,
                    "hint": "launch the browser with --remote-debugging-port=9222; until then nothing here can work",
                })),
            }
        }
        "tabs" => match args.get("action").and_then(Value::as_str).unwrap_or("list") {
            "list" => {
                let tabs = cdp::targets(&cfg.endpoint)?;
                // Extension pages are targets of type "page" but are not
                // browsing tabs: two of them showed up in every list and had to
                // be filtered by hand. `all: true` asks for them on purpose.
                let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);
                Ok(json!(tabs.iter()
                    .filter(|t| all || !t.url.starts_with("chrome-extension://"))
                    .map(|t| json!({ "tab": t.id, "title": t.title, "url": t.url }))
                    .collect::<Vec<_>>()))
            }
            "open" => {
                let url = args.get("url").and_then(Value::as_str).ok_or("open needs url")?;
                // Background by default: the human may be typing to me in the
                // chat, and activating the tab hands their keyboard to the
                // browser. `foreground: true` when the tab itself is the point.
                let fg = args.get("foreground").and_then(Value::as_bool).unwrap_or(false);
                let id = cdp::new_tab(&cfg.endpoint, url, fg)?;
                // A fresh tab is about:blank and a blank document already reports
                // readyState "complete", so require the URL to have left it.
                let mut ready = String::from("unknown");
                let _ = with_conn(&cfg.endpoint, Some(&id), |conn, _| {
                    let started = std::time::Instant::now();
                    while started.elapsed().as_millis() < 16_000 {
                        // An evaluation during navigation can fail (the old
                        // context is gone); that is "not ready yet", not an error.
                        if let Ok(v) = conn.eval("[document.readyState, location.href]") {
                            ready = v.get(0).and_then(Value::as_str).unwrap_or("unknown").to_string();
                            let href = v.get(1).and_then(Value::as_str).unwrap_or_default();
                            if !href.is_empty() && href != "about:blank" && (ready == "complete" || ready == "interactive") {
                                break;
                            }
                        } else if conn.dead || conn.dialog.is_some() {
                            break;
                        }
                        nap(20)?; // a fresh tab is about:blank for a few ms
                    }
                    Ok(())
                });
                // SPAs reach "interactive" before they paint, so a caller that
                // needs content should ask to settle: wait until the resource
                // count stops moving instead of trusting readyState alone.
                if args.get("idle").and_then(Value::as_bool).unwrap_or(false) {
                    let _ = with_conn(&cfg.endpoint, Some(&id), |conn, _| wait_idle(conn, 20_000, 300));
                }
                let final_url = with_conn(&cfg.endpoint, Some(&id), |conn, _| conn.eval("location.href"))
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default();
                Ok(json!({ "tab": id, "url": final_url, "readyState": ready }))
            }
            "close" => {
                let id = args.get("tab").and_then(Value::as_str).ok_or("close needs tab")?.to_string();
                // The browser confirms the target is gone. A beforeunload guard
                // can refuse, and the tab then keeps its connection and caches.
                let closed = cdp::close_tab(&cfg.endpoint, &id)?;
                if closed {
                    conn_cache().lock().unwrap().remove(&id);
                    scope_cache().lock().unwrap().remove(&id);
                    scan_cache().lock().unwrap().remove(&id);
                }
                Ok(json!({
                    "closed": closed,
                    "tab": id,
                    "note": if closed { Value::Null } else {
                        json!("the browser did not close it: a beforeunload handler is probably asking \
                               whether to leave. The tab is still open and its connection is kept.")
                    },
                }))
            }
            "goto" | "back" | "forward" | "reload" => {
                let name = args.get("action").and_then(Value::as_str).unwrap_or("");
                let idle = args.get("idle").and_then(Value::as_bool).unwrap_or(false);
                let action = match name {
                    "goto" => nav::Action::Goto(
                        args.get("url").and_then(Value::as_str).ok_or("goto needs url")?.to_string(),
                    ),
                    "back" => nav::Action::Back,
                    "forward" => nav::Action::Forward,
                    _ => nav::Action::Reload {
                        ignore_cache: args.get("ignoreCache").and_then(Value::as_bool).unwrap_or(false),
                    },
                };
                with_conn(&cfg.endpoint, tab, |conn, tab_id| {
                    nav::navigate(conn, tab_id, action.clone(), idle)
                })
            }
            other => Err(format!("unknown tabs action: {other}")),
        },
        "emulate" => with_conn(&cfg.endpoint, tab, |conn, tab_id| emulate::run(conn, tab_id, &args)),
        "pdf" => with_conn(&cfg.endpoint, tab, |conn, tab_id| pdf::run(conn, tab_id, &args)),
        "eval" => {
            let expr = args.get("expression").and_then(Value::as_str).ok_or("eval needs expression")?;
            with_conn(&cfg.endpoint, tab, |conn, tab_id| {
                match conn.eval(expr) {
                    Ok(v) => Ok(json!({ "tab": tab_id, "value": v })),
                    Err(e) => {
                        // A dialog can open mid-expression. Reporting it here is
                        // the truth; the caller answers it on this turn.
                        match conn.dialog.clone() {
                            Some(d) => Ok(json!({ "tab": tab_id, "dialog": d })),
                            None => Err(e),
                        }
                    }
                }
            })
        }
        "elements" => {
            let find = args.get("find").and_then(Value::as_str).map(|s| s.to_lowercase());
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(120) as usize;
            let full = args.get("full").and_then(Value::as_bool).unwrap_or(false);
            let mode = args.get("mode").and_then(Value::as_str).unwrap_or("full").to_string();
            let unique = args.get("unique").and_then(Value::as_bool).unwrap_or(false);
            let role_filter = args.get("role").and_then(Value::as_str).map(|s| s.to_lowercase());
            let shot_mode = args.get("shot").and_then(Value::as_str).unwrap_or("none").to_string();
            let shot_fmt = args.get("shotFormat").and_then(Value::as_str).unwrap_or("jpeg").to_string();
            let max_w = args.get("shotMaxWidth").and_then(Value::as_f64).unwrap_or(1024.0);
            let region = parse_region(args.get("region"));

            let within = match args.get("within") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => nonempty_scope(s),
                Some(_) => return Err("within must be a CSS selector; integer within is not implemented yet".into()),
            };
            with_conn(&cfg.endpoint, tab, |conn, tab_id| {
                let mut scan_v = scan_with_frames(conn, within.as_deref())?;
                // Kept with the cache entry, so a delta knows whether its
                // baseline came from the same scope as this scan.
                if let Some(o) = scan_v.as_object_mut() {
                    o.insert("scope".into(), json!(&within));
                }
                scope_cache().lock().unwrap().insert(tab_id.to_string(), within.clone());

                let mut all = scan_v.get("elements").and_then(Value::as_array).cloned().unwrap_or_default();
                if mode == "viewport" {
                    all.retain(|e| e.get("off").and_then(Value::as_bool) != Some(true));
                }

                let previous = {
                    let mut cache = scan_cache().lock().unwrap();
                    let prev = cache.get(tab_id).cloned().unwrap_or(Value::Null);
                    cache.insert(tab_id.to_string(), scan_v.clone());
                    prev
                };
                // A baseline taken under another scope is not a baseline: every
                // element would read as added and every old one as removed.
                let scope_changed = !previous.is_null()
                    && previous.get("scope").and_then(Value::as_str) != within.as_deref();
                let previous = if scope_changed { Value::Null } else { previous };

                if mode == "delta" {
                    let prev_els = previous.get("elements").and_then(Value::as_array).cloned().unwrap_or_default();
                    let old_set: std::collections::HashSet<String> = prev_els.iter().map(fingerprint).collect();
                    let new_set: std::collections::HashSet<String> = all.iter().map(fingerprint).collect();
                    let added: Vec<Value> = all.iter().filter(|e| !old_set.contains(&fingerprint(e))).cloned().collect();
                    let removed: Vec<Value> = prev_els.iter().filter(|e| !new_set.contains(&fingerprint(e))).cloned().collect();
                    let prev_ov = previous.get("overlays").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0) as i64;
                    let now_ov = scan_v.get("overlays").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0) as i64;
                    return Ok(json!({
                        "tab": tab_id,
                        "mode": "delta",
                        // Without these, a scope that never existed reads as
                        // "nothing changed" in the mode callers use to verify.
                        "scope": within.clone(),
                        "scopeChanged": scope_changed,
                        "scopeMissing": scan_v.get("scopeMissing").and_then(Value::as_bool).unwrap_or(false),
                        "baseline": !previous.is_null(),
                        "added": added,
                        "addedCount": added.len(),
                        "removed": removed,
                        "removedCount": removed.len(),
                        "unchanged": all.len().saturating_sub(added.len()),
                        "overlays": scan_v.get("overlays").cloned().unwrap_or(json!([])),
                        "overlayDelta": now_ov - prev_ov,
                        "media": scan_v.get("media").cloned().unwrap_or(json!([])),
                    }));
                }

                let total = all.len();
                let mut seen_labels: std::collections::HashSet<String> = std::collections::HashSet::new();
                let mut shown: Vec<Value> = Vec::new();
                for e in &all {
                    let label = e.get("label").and_then(Value::as_str).unwrap_or("");
                    if let Some(f) = &find {
                        if !label.to_lowercase().contains(f) {
                            continue;
                        }
                    }
                    if let Some(rf) = &role_filter {
                        let role = e.get("role").and_then(Value::as_str).unwrap_or("").to_lowercase();
                        let tag = e.get("tag").and_then(Value::as_str).unwrap_or("").to_lowercase();
                        if role != *rf && tag != *rf {
                            continue;
                        }
                    }
                    if unique && !label.is_empty() && !seen_labels.insert(label.to_lowercase()) {
                        continue;
                    }
                    let mut keep = e.clone();
                    if !full {
                        if let Some(o) = keep.as_object_mut() {
                            o.remove("w");
                            o.remove("h");
                        }
                    }
                    shown.push(keep);
                    if shown.len() >= limit {
                        break;
                    }
                }

                let url = scan_v.get("url").and_then(Value::as_str).unwrap_or("").to_string();
                let url = if url.len() > 300 { format!("{}…", clip_str(&url, 300)) } else { url };
                let scope_missing = scan_v.get("scopeMissing").and_then(Value::as_bool).unwrap_or(false);
                let payload = json!({
                    "tab": tab_id,
                    "url": url,
                    "title": scan_v.get("title").cloned().unwrap_or(Value::Null),
                    // Which scope produced this list, and whether it matched.
                    // Without the echo, a caller cannot tell a scoped scan from
                    // an unscoped one, and a miss from an empty scope.
                    "scope": within.clone(),
                    "scopeMissing": scope_missing,
                    "overlays": scan_v.get("overlays").cloned().unwrap_or(json!([])),
                    "media": scan_v.get("media").cloned().unwrap_or(json!([])),
                    "scroll": scan_v.get("scroll").cloned().unwrap_or(Value::Null),
                    "total": total,
                    "shown": shown.len(),
                    "truncated": shown.len() < total,
                    // How many of these came from inside a cross-origin frame.
                    // Absent would read as "none", which is a lie when it is
                    // non-zero, so it is always present.
                    "framesScanned": scan_v.get("framesScanned").cloned().unwrap_or(json!(0)),
                    "nodes": scan_v.get("nodes").cloned().unwrap_or(json!(0)),
                    // A near-empty scan on a document with almost nothing in it
                    // is usually a page that has not painted yet, not an empty
                    // page. Saying so costs one field and saves a retry. Never
                    // for a scope that missed: that answer is already exact.
                    "suspect": if !scope_missing && suspect_render(
                        total as u64,
                        scan_v.get("nodes").and_then(Value::as_u64).unwrap_or(0),
                    ) {
                        json!("page may still be rendering: few nodes, no elements")
                    } else {
                        Value::Null
                    },
                    "elements": shown,
                });

                if shot_mode == "none" {
                    return Ok(payload);
                }
                let (bytes, clipped, mime) = capture_png(conn, &shot_mode, region, Some(max_w), &shot_fmt, 82)?;
                Ok(json!({ "__content": [
                    { "type": "text", "text": payload.to_string() },
                    { "type": "image", "data": b64encode(&bytes), "mimeType": mime },
                    { "type": "text", "text": format!("(attached {shot_mode} capture, {} bytes{})", bytes.len(), if clipped { ", clipped to 8000px" } else { "" }) }
                ]}))
            })
        }
        "text" => {
            // What the page says, as text. `elements` answers "what can I click";
            // this answers "what does it say", which is the other half of reading
            // a page and had no tool until now - every agent call was writing the
            // same innerText extraction by hand.
            let within = match args.get("within") {
                None | Some(Value::Null) => "body".to_string(),
                Some(Value::String(s)) => nonempty_scope(s).unwrap_or_else(|| "body".to_string()),
                Some(_) => return Err("within must be a CSS selector".into()),
            };
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(4000) as usize;
            with_conn(&cfg.endpoint, tab, |conn, tab_id| {
                let js = format!(
                    "(() => {{ \
                       let r = null; \
                       try {{ r = document.querySelector({sel}); }} catch (e) {{ return {{ err: String(e) }}; }} \
                       if (!r) return {{ missing: true }}; \
                       const t = (r.innerText || '').replace(/\\n{{3,}}/g, '\\n\\n').trim(); \
                       return {{ text: t.slice(0, {lim}) }}; \
                     }})()",
                    sel = serde_json::to_string(&within).unwrap_or_else(|_| "\"body\"".into()),
                    lim = limit
                );
                let v = conn.eval(&js)?;
                if let Some(e) = v.get("err").and_then(Value::as_str) {
                    return Err(format!("text: {e}"));
                }
                // A selector that matches nothing is not the body: saying so
                // beats handing back the whole page as if it were the scope.
                let scope_missing = v.get("missing").and_then(Value::as_bool) == Some(true);
                let text = v.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                Ok(json!({
                    "tab": tab_id,
                    "within": within,
                    "scopeMissing": scope_missing,
                    "chars": text.chars().count(),
                    "truncated": text.chars().count() >= limit,
                    "text": text,
                }))
            })
        }
        "input" => {
            let actions = args.get("actions").and_then(Value::as_array).ok_or("input needs actions[]")?.clone();
            let announce = args.get("announce").and_then(Value::as_bool).unwrap_or(false);
            let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
            let settle_ms = args.get("settleMs").and_then(Value::as_u64).unwrap_or(0).min(5_000);
            // Only a batch that *starts* by answering a dialog may run against a
            // blocked page: anything else would send a click an eval that has to
            // wait out its timeout. A click that opens one is handled inside
            // run_input, where the page was not blocked on entry.
            let answers_dialog = actions.first().and_then(|a| a.get("dialog")).is_some();
            if answers_dialog {
                with_conn_dialog(&cfg.endpoint, tab, |conn, tab_id| {
                    run_batch(conn, tab_id, &actions, announce, force, settle_ms)
                })
            } else {
                with_conn(&cfg.endpoint, tab, |conn, tab_id| {
                    run_batch(conn, tab_id, &actions, announce, force, settle_ms)
                })
            }
        }
        "images" => {
            with_conn(&cfg.endpoint, tab, |conn, tab_id| {
                let v = conn.eval(
                    "(() => [...document.querySelectorAll('img')].filter(i => i.currentSrc || i.src).map(i => ({ \
                       src: i.currentSrc || i.src, alt: (i.alt || '').slice(0, 60), \
                       natW: i.naturalWidth, natH: i.naturalHeight, \
                       w: Math.round(i.getBoundingClientRect().width) })).slice(0, 200))()",
                )?;
                let n = v.as_array().map(|a| a.len()).unwrap_or(0);
                Ok(json!({ "tab": tab_id, "count": n, "images": v }))
            })
        }
        "clip" => {
            // An agent-owned clipboard. The system clipboard is the human's: touching it
            // races with whatever they last copied. This store never leaves a file,
            // so a copy here cannot clobber theirs, and `toSystem` is the explicit
            // opt-in when a real Ctrl+V is the only way to paste somewhere.
            let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
            let prof = cfg.profile_now();
            let path = data_dir().join(format!("clip-{}.jsonl", prof.name));
            let load = || -> Vec<Value> {
                std::fs::read_to_string(&path)
                    .map(|b| b.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).collect())
                    .unwrap_or_default()
            };
            match action {
                "set" | "copy" => {
                    let text = args.get("text").and_then(Value::as_str).ok_or("clip set needs text")?;
                    let (id, count) = clip_append(&prof.name, text, args.get("label").and_then(Value::as_str))?;
                    Ok(json!({ "stored": text, "id": id, "count": count }))
                }
                "get" => {
                    let all = load();
                    let item = match args.get("id").and_then(Value::as_u64) {
                        Some(want) => all.iter().rev().find(|v| v.get("id").and_then(Value::as_u64) == Some(want)).cloned(),
                        None => all.last().cloned(),
                    };
                    Ok(json!({ "item": item }))
                }
                "list" => {
                    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize;
                    let all = load();
                    let start = all.len().saturating_sub(limit);
                    Ok(json!({ "total": all.len(), "recent": &all[start..] }))
                }
                "clear" => {
                    let _ = std::fs::write(&path, "");
                    Ok(json!({ "cleared": true }))
                }
                "toSystem" => {
                    // The one opt-in path: put a stored item on the real clipboard.
                    let text = match args.get("text").and_then(Value::as_str).map(str::to_string) {
                        Some(t) => t,
                        None => {
                            let all = load();
                            all.last().and_then(|v| v.get("text")).and_then(Value::as_str)
                                .ok_or("clipboard store is empty")?.to_string()
                        }
                    };
                    clip_write("text/plain;charset=utf-8", text.as_bytes())?;
                    Ok(json!({ "onSystemClipboard": true, "chars": text.len() }))
                }
                other => Err(format!("unknown clip action: {other}")),
            }
        }
        "link" => {
            // Links, not images: open one in a tab, or copy its URL. Works on any
            // element with an href, so it covers nav links, buttons-as-links and
            // the current page itself.
            let action = args.get("action").and_then(Value::as_str).unwrap_or("open");
            let explicit = args.get("url").and_then(Value::as_str).map(str::to_string);
            let index = args.get("i").and_then(Value::as_u64);
            with_conn(&cfg.endpoint, tab, |conn, tab_id| {
                let url = match (explicit.clone(), index) {
                    (Some(u), _) => u,
                    (None, Some(i)) => {
                        // resolved on the element itself, without scrolling the page
                        let scope = scope_for(tab_id);
                        let at = pick(conn, scope.as_deref(), i, false, None)?;
                        at.get("href").and_then(Value::as_str)
                            .ok_or_else(|| format!("no link at index {i}"))?.to_string()
                    }
                    _ => conn.eval("location.href")?.as_str().unwrap_or_default().to_string(),
                };
                match action {
                    "open" => {
                        let fg = args.get("foreground").and_then(Value::as_bool).unwrap_or(false);
                        let id = cdp::new_tab(&cfg.endpoint, &url, fg)?;
                        Ok(json!({ "action": "open", "tab": id, "url": url, "foreground": fg }))
                    }
                    "copy" => {
                        clip_append(&cfg.profile.name, &url, Some("link"))?;
                        Ok(json!({ "action": "copy", "url": url, "stored": true }))
                    }
                    other => Err(format!("unknown link action: {other}")),
                }
            })
        }
        "image" => {
            let action = args.get("action").and_then(Value::as_str).unwrap_or("open");
            let src = args.get("url").and_then(Value::as_str).map(str::to_string);
            let index = args.get("i").and_then(Value::as_u64);
            with_conn(&cfg.endpoint, tab, |conn, _tab_id| {
                // Resolve which image: an explicit url, or index from `images`.
                let url = match (src.clone(), index) {
                    (Some(u), _) => u,
                    (None, Some(i)) => conn
                        .eval(&format!(
                            "(() => {{ const im = [...document.querySelectorAll('img')].filter(i => i.currentSrc || i.src)[{i}]; return im ? (im.currentSrc || im.src) : null; }})()"
                        ))?
                        .as_str()
                        .ok_or_else(|| format!("no image at index {i}"))?
                        .to_string(),
                    _ => return Err("image needs url or i".into()),
                };
                match action {
                    "open" => {
                        // A tab with the image itself, so a human can look or save
                        // it. In the background by default like every other open:
                        // their keyboard stays where they left it.
                        let fg = args.get("foreground").and_then(Value::as_bool).unwrap_or(false);
                        let id = cdp::new_tab(&cfg.endpoint, &url, fg)?;
                        Ok(json!({ "action": "open", "tab": id, "url": url, "foreground": fg }))
                    }
                    "link" => {
                        // The URL goes to the AGENT clipboard, never the system
                        // one: a copy here must not overwrite what the human
                        // last copied. `clip {action:"toSystem"}` is the opt-in.
                        clip_append(&cfg.profile.name, &url, Some("image link"))?;
                        Ok(json!({ "action": "link", "url": url, "storedInAgentClipboard": true }))
                    }
                    "copy" => {
                        // The bytes, so it can be pasted into an editor (Gemini,
                        // Docs, a chat box) with a real Ctrl+V.
                        let js = format!(
                            r#"(async () => {{
                                const r = await fetch({src}, {{ credentials: 'omit' }});
                                if (!r.ok) return null;
                                const b = new Uint8Array(await r.arrayBuffer());
                                if (b.length > 12_000_000) return null;
                                let s = '';
                                for (let k = 0; k < b.length; k += 0x8000)
                                    s += String.fromCharCode.apply(null, b.subarray(k, k + 0x8000));
                                return btoa(s);
                            }})()"#,
                            src = serde_json::Value::String(url.clone())
                        );
                        let mut bytes = conn.eval(&js).ok().and_then(|v| v.as_str().and_then(|s| b64decode(s).ok())).unwrap_or_default();
                        if bytes.is_empty() {
                            // cross-origin: take it from the page's own cache
                            if let Ok(tree) = conn.call("Page.getFrameTree", json!({})) {
                                if let Some(fid) = tree.pointer("/frameTree/frame/id").and_then(Value::as_str) {
                                    if let Ok(r) = conn.call("Page.getResourceContent",
                                        json!({ "frameId": fid, "url": url })) {
                                        if let Some(c) = r.get("content").and_then(Value::as_str) {
                                            if r.get("base64Encoded").and_then(Value::as_bool).unwrap_or(false) {
                                                bytes = b64decode(c).unwrap_or_default();
                                            } else {
                                                bytes = c.as_bytes().to_vec();
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if bytes.is_empty() {
                            return Err(format!("could not fetch image bytes: {url}"));
                        }
                        let mime = if bytes.starts_with(b"\x89PNG") { "image/png" }
                                   else if bytes.starts_with(b"\xff\xd8") { "image/jpeg" }
                                   else if bytes.starts_with(b"RIFF") { "image/webp" }
                                   else if bytes.starts_with(b"GIF8") { "image/gif" }
                                   else { "image/png" };
                        clip_write(mime, &bytes)?;
                        Ok(json!({
                            "action": "copy",
                            "url": url, "bytes": bytes.len(), "mime": mime,
                            "onSystemClipboard": true,
                            "note": "pixels are on the SYSTEM clipboard so a real Ctrl+V can paste them; the agent clipboard is untouched",
                        }))
                    }
                    other => Err(format!("unknown image action: {other}")),
                }
            })
        }
        "download" => {
            let path = args.get("path").and_then(Value::as_str).ok_or("download needs path")?.to_string();
            let max_bytes = args.get("maxBytes").and_then(Value::as_u64).unwrap_or(10 * 1024 * 1024);
            let explicit = args.get("url").and_then(Value::as_str).map(str::to_string);
            let index = args.get("i").and_then(Value::as_u64);
            with_conn(&cfg.endpoint, tab, |conn, tab_id| {
                // Resolve the URL in-page (so it may come from an <img> index),
                // then fetch it there too: cookies and referer stay correct, which
                // is what makes authenticated images (CDNs, dashboards) work.
                let src = match (explicit.clone(), index) {
                    (Some(u), _) => u,
                    (None, Some(i)) => {
                        let v = conn.eval(
                            "(() => { const im = [...document.querySelectorAll('img')].filter(i => i.currentSrc || i.src)[INDEX]; return im ? (im.currentSrc || im.src) : null; })()"
                                .replace("INDEX", &i.to_string())
                                .as_str(),
                        )?;
                        v.as_str().ok_or_else(|| format!("no image at index {i}"))?.to_string()
                    }
                    _ => return Err("download needs url or i".into()),
                };
                let js = format!(
                    r#"(async () => {{
                        try {{
                            // 'include' keeps cookies (same-origin, authenticated
                            // images); 'omit' is what a public CDN needs, because
                            // a credentialed cross-origin request demands a much
                            // stricter CORS grant than ACAO:*.
                            let r = null;
                            for (const cred of ['include', 'omit']) {{
                                try {{ r = await fetch({src}, {{ credentials: cred }}); }}
                                catch (e) {{ r = null; }}
                                if (r && r.ok) break;
                            }}
                            if (!r) return {{ error: 'network' }};
                            if (!r.ok) return {{ error: 'http ' + r.status }};
                            const buf = new Uint8Array(await (await r.blob()).arrayBuffer());
                            if (buf.length > {max}) return {{ error: 'too large', size: buf.length }};
                            let bin = '';
                            for (let k = 0; k < buf.length; k += 0x8000)
                                bin += String.fromCharCode.apply(null, buf.subarray(k, k + 0x8000));
                            return {{ b64: btoa(bin), size: buf.length }};
                        }} catch (e) {{ return {{ error: String(e).slice(0, 120) }}; }}
                    }})()"#,
                    src = serde_json::Value::String(src.clone()),
                    max = max_bytes
                );
                let v = conn.eval(&js)?;
                // fetch() cannot read a cross-origin body (CORS), and page images
                // very often live on a CDN. Fall back to the resource the page
                // already loaded, which CDP will hand over regardless of origin.
                let bytes = if let Some(err) = v.get("error").and_then(Value::as_str) {
                    // getResourceContent needs the Page domain, which every page
                    // connection enables on first use (with_conn), so this path
                    // no longer has to turn it on itself.
                    let tree = conn.call("Page.getFrameTree", json!({}))?;
                    let frame_id = tree
                        .pointer("/frameTree/frame/id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| format!("download failed: {err}"))?
                        .to_string();
                    let r = conn
                        .call("Page.getResourceContent", json!({ "frameId": frame_id, "url": src }))
                        .map_err(|e| format!("download failed: {err} (and the page cache had it not: {e})"))?;
                    let content = r.get("content").and_then(Value::as_str).ok_or("no content")?;
                    if r.get("base64Encoded").and_then(Value::as_bool).unwrap_or(false) {
                        b64decode(content)?
                    } else {
                        content.as_bytes().to_vec()
                    }
                } else {
                    let b64 = v.get("b64").and_then(Value::as_str).ok_or("no data returned")?;
                    b64decode(b64)?
                };
                std::fs::write(&path, &bytes).map_err(|e| format!("cannot write {path}: {e}"))?;
                Ok(json!({ "tab": tab_id, "path": path, "url": src, "bytes": bytes.len() }))
            })
        }
        "upload" => {
            let path = args.get("path").and_then(Value::as_str).ok_or("upload needs path")?.to_string();
            let selector = args.get("selector").and_then(Value::as_str).unwrap_or("input[type=file]").to_string();
            // Pages often carry several identical file inputs (an avatar and a
            // banner on the same form). Picking the first match silently uploads
            // to the wrong one, so the caller can name which.
            let index = args.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            if !std::path::Path::new(&path).exists() {
                return Err(format!("no such file: {path}"));
            }
            with_conn(&cfg.endpoint, tab, |conn, tab_id| {
                let doc = conn.call("DOM.getDocument", json!({ "depth": 0, "pierce": true }))?;
                let root = doc.pointer("/root/nodeId").and_then(Value::as_u64).ok_or("no document")?;
                let found = conn.call("DOM.querySelectorAll", json!({ "nodeId": root, "selector": selector }));
                let node = found
                    .ok()
                    .and_then(|v| v.get("nodeIds").and_then(Value::as_array).cloned())
                    .and_then(|ids| ids.get(index).and_then(Value::as_u64))
                    .unwrap_or(0);
                if node == 0 {
                    let n = conn.eval("document.querySelectorAll('input[type=file]').length")
                        .ok().and_then(|v| v.as_i64()).unwrap_or(0);
                    return Err(format!("no element matched {selector} at index {index} ({n} file inputs on the page). \
                        If the picker only exists after clicking a button, click it first."));
                }
                conn.call("DOM.setFileInputFiles", json!({ "nodeId": node, "files": [path] }))?;
                Ok(json!({ "tab": tab_id, "uploaded": path, "selector": selector, "index": index, "ok": true }))
            })
        }
        "paste" => {
            let path = args.get("path").and_then(Value::as_str).map(str::to_string);
            let text = args.get("text").and_then(Value::as_str).map(str::to_string);
            let click_i = args.get("i").and_then(Value::as_u64);
            if path.is_none() && text.is_none() {
                return Err("paste needs path (a file) or text".into());
            }
            if text.as_deref() == Some("") {
                return Err("paste text is empty".into());
            }
            let method = args.get("method").and_then(Value::as_str).unwrap_or("page").to_string();
            if method != "page" && method != "system" {
                return Err(format!("unknown paste method {method:?} (page or system)"));
            }
            if method == "system" && !cfg!(feature = "experimental") {
                return Err(
                    "method:system is only in experimental builds; use method:page, or build with \
                     --features experimental"
                        .into(),
                );
            }
            let mime = match path.as_deref() {
                Some(p) => {
                    let ext = std::path::Path::new(p).extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
                    match ext.as_str() {
                        "jpg" | "jpeg" => "image/jpeg",
                        "webp" => "image/webp",
                        "gif" => "image/gif",
                        "png" => "image/png",
                        "svg" => "image/svg+xml",
                        "mp4" => "video/mp4",
                        "webm" => "video/webm",
                        "mov" => "video/quicktime",
                        "pdf" => "application/pdf",
                        "txt" | "md" => "text/plain",
                        _ => "application/octet-stream",
                    }
                }
                None => "text/plain;charset=utf-8",
            };
            let payload = match &path {
                Some(p) => std::fs::read(p).map_err(|e| format!("cannot read {p}: {e}"))?,
                None => text.as_deref().unwrap_or("").as_bytes().to_vec(),
            };
            let pasted = path.clone().or(text.clone().map(|t| format!("{} chars", t.chars().count())));
            let restore = args.get("restore").and_then(Value::as_bool).unwrap_or(true);
            let saved = if method == "system" && restore { clip_save() } else { None };
            with_conn(&cfg.endpoint, tab, |conn, tab_id| {
                // focus the target first, otherwise the paste lands wherever the
                // caret happens to be
                if let Some(i) = click_i {
                    let scope = scope_for(tab_id);
                    click_index(conn, scope.as_deref(), i)?;
                }

                if method == "page" {
                    // Hand the data to the page as a paste event carrying it.
                    // The system clipboard is no use here: on Wayland only the
                    // focused app may read it, and the browser is deliberately
                    // kept unfocused so the human keeps their keyboard
                    // (measured: Ctrl+V into the unfocused browser pasted
                    // nothing, focused it pasted). This never touches the
                    // human's clipboard either.
                    let name = path.as_deref()
                        .and_then(|p| std::path::Path::new(p).file_name())
                        .and_then(|n| n.to_str())
                        .unwrap_or("pasted");
                    let js = format!(
                        r#"(() => {{
                            let el = document.activeElement || document.body;
                            while (el && el.shadowRoot && el.shadowRoot.activeElement) el = el.shadowRoot.activeElement;
                            const dt = new DataTransfer();
                            const text = {text};
                            if (text !== null) dt.setData('text/plain', text);
                            else {{
                                const bin = atob({b64});
                                const u8 = new Uint8Array(bin.length);
                                for (let k = 0; k < bin.length; k++) u8[k] = bin.charCodeAt(k);
                                dt.items.add(new File([u8], {name}, {{ type: {mime} }}));
                            }}
                            const textLike = (n) => {{
                                if (!n) return false;
                                if (n.isContentEditable) return true;
                                if (n.tagName === 'TEXTAREA') return !n.disabled && !n.readOnly;
                                if (n.tagName !== 'INPUT' || n.disabled || n.readOnly) return false;
                                const t = (n.type || 'text').toLowerCase();
                                return ['text','search','url','tel','email','password','number'].includes(t);
                            }};
                            const read = (n) => (n && n.isContentEditable) ? ((n.innerText) || '')
                                : ((n && typeof n.value === 'string') ? n.value : '');
                            const sel = (n) => {{
                                if (n && typeof n.selectionStart === 'number')
                                    return n.selectionEnd > n.selectionStart ? 'range' : 'caret';
                                const s = window.getSelection();
                                return s && !s.isCollapsed ? 'range' : 'caret';
                            }};
                            const before = read(el);
                            const selBefore = sel(el);
                            const editable = textLike(el);
                            const ev = new ClipboardEvent('paste', {{ clipboardData: dt, bubbles: true, cancelable: true, composed: true }});
                            const handled = !el.dispatchEvent(ev);
                            // A page can cancel the paste and commit the DOM a tick
                            // later, so an unchanged read here is not proof it did
                            // nothing. Remember what to compare after a frame, and
                            // treat a collapsed range as a replacement even when
                            // the text came out identical.
                            const changed = read(el) !== before || (selBefore === 'range' && sel(el) === 'caret');
                            if (editable && handled && text !== null && !changed) window.__cdpPaste = {{ text: before, sel: selBefore }};
                            const out = {{ handled, target: el.tagName.toLowerCase(), changed, editable }};
                            // A file nobody took as a paste: offer it as a drop,
                            // which is how most upload areas accept files.
                            if (!handled && text === null) {{
                                const opts = {{ dataTransfer: dt, bubbles: true, cancelable: true, composed: true }};
                                el.dispatchEvent(new DragEvent('dragenter', opts));
                                el.dispatchEvent(new DragEvent('dragover', opts));
                                out.dropped = !el.dispatchEvent(new DragEvent('drop', opts));
                            }}
                            return out;
                        }})()"#,
                        text = text.as_deref().map(|t| Value::String(t.to_string())).unwrap_or(Value::Null),
                        b64 = if text.is_none() { Value::String(b64encode(&payload)) } else { Value::Null },
                        name = Value::String(name.to_string()),
                        mime = Value::String(mime.to_string()),
                    );
                    let r = conn.eval(&js)?;
                    let handled = r.get("handled").and_then(Value::as_bool).unwrap_or(false);
                    let changed = r.get("changed").and_then(Value::as_bool).unwrap_or(false);
                    let editable = r.get("editable").and_then(Value::as_bool).unwrap_or(false);
                    // A handler can accept the paste and insert nothing: Quill
                    // (Gemini's editor) does exactly that, and the tool used to
                    // report success. Trust the editor, not the event.
                    let took = handled && (text.is_none() || changed);
                    let mut how = if took { "paste event" } else { "" };
                    if !took {
                        if let Some(t) = &text {
                            if !editable {
                                return Err(format!(
                                    "the page swallowed the pasted text and the focused element <{}> is not editable",
                                    r.get("target").and_then(Value::as_str).unwrap_or("?")
                                ));
                            }
                            // Nobody inserted it yet. A handler that cancelled
                            // the event may commit on a microtask or a frame, so
                            // give it one before deciding it inserted nothing.
                            let mut landed = false;
                            if handled {
                                nap(50)?;
                                let v = conn.eval_within(PASTE_LANDED_JS, 2_000)?;
                                landed = v.get("changed").and_then(Value::as_bool).unwrap_or(false);
                            }
                            if landed {
                                how = "paste event (delayed)";
                            } else {
                                // Verify the insert: a beforeinput handler can
                                // block it, and reporting success without
                                // checking is how a silent no-op passed before.
                                conn.eval_within(PASTE_SNAP_JS, 1_500)?;
                                conn.call("Input.insertText", json!({ "text": t }))?;
                                let v = conn
                                    .eval_within(PASTE_LANDED_JS, 1_500)
                                    .map_err(|e| format!("the text was inserted, but reading it back failed: {e}"))?;
                                let ok = v.get("changed").and_then(Value::as_bool).unwrap_or(false);
                                if !ok {
                                    return Err("the editor did not take the inserted text; a beforeinput handler may be blocking it".into());
                                }
                                how = "inserted as typed text";
                            }
                        } else if r.get("dropped").and_then(Value::as_bool) == Some(true) {
                            how = "drop event";
                        } else {
                            return Err(format!(
                                "the page ignored the pasted file (focused element: <{}>). For a file \
                                 input use upload; or focus the editor first with i",
                                r.get("target").and_then(Value::as_str).unwrap_or("?")
                            ));
                        }
                    }
                    return Ok(json!({ "tab": tab_id, "pasted": pasted, "mime": mime,
                                      "bytes": payload.len(), "via": how }));
                }

                // method "system": the real clipboard and a real Ctrl+V. Only works while
                // the browser window has keyboard focus.
                clip_write(mime, &payload)?;
                // From here on the human's clipboard is already overwritten, so every exit has
                // to schedule the restore — including a failed keystroke. The result is
                // carried, not returned early.
                let mut outcome: Result<(), String> = Ok(());
                // The writer's child claims the selection after the parent exits, so wait
                // until the clipboard really holds our bytes; pasting sooner pastes whatever
                // was there before.
                let started = std::time::Instant::now();
                while started.elapsed().as_millis() < 1_500 {
                    if clip_read(mime).is_some_and(|b| b.len() == payload.len() && (path.is_some() || b == payload)) {
                        break;
                    }
                    if let Err(e) = nap(30) {
                        outcome = Err(e);
                        break;
                    }
                }
                if outcome.is_ok() {
                    // macOS pastes with Cmd, everyone else with Control: the modifier has to
                    // match or the browser ignores the chord.
                    let mods = if current_os() == Os::Mac { 4 } else { 2 };
                    let key = json!({ "modifiers": mods, "key": "v", "code": "KeyV",
                                      "windowsVirtualKeyCode": 86, "nativeVirtualKeyCode": 86 });
                    let mut down = key.clone();
                    down["type"] = json!("keyDown");
                    // A modifier bitmask alone does not make the browser perform a paste; the
                    // editing command has to be named explicitly.
                    down["commands"] = json!(["paste"]);
                    let mut up = key; up["type"] = json!("keyUp");
                    outcome = conn.call_many(&[("Input.dispatchKeyEvent", down), ("Input.dispatchKeyEvent", up)])
                        .map(|_| ());
                    if outcome.is_ok() {
                        outcome = nap(400);
                    }
                }

                // Put the caller's clipboard back — but out of process and a
                // couple of seconds later. The writer forks, and its child
                // claims the selection asynchronously, so restoring inline
                // loses the race to the paste we just made. A detached shell command wins
                // it, and keeps the tool's reply fast.
                let backend = clip_backend();
                let restored = match (&saved, backend) {
                    (Some((m, b)), Some(bk)) => {
                        let tmp = clip_temp("cdp-mcp-restore");
                        if clip_stage(&tmp, b).is_ok() {
                            let spawned = match clip_restore_script(bk, m, &tmp.to_string_lossy()) {
                                Ok(cmd) => std::process::Command::new("sh")
                                    .arg("-c")
                                    .arg(cmd)
                                    // Detach its stdio completely: a child that
                                    // inherits our stdout keeps the pipe open after
                                    // this process ends, and anything reading the
                                    // MCP output then waits forever.
                                    .stdin(std::process::Stdio::null())
                                    .stdout(std::process::Stdio::null())
                                    .stderr(std::process::Stdio::null())
                                    .spawn()
                                    .is_ok(),
                                Err(_) => false,
                            };
                            if !spawned {
                                // Nothing will read the staged bytes: the script
                                // that would have fed them was never started.
                                let _ = std::fs::remove_file(&tmp);
                            }
                            spawned
                        } else {
                            false
                        }
                    }
                    _ => false,
                };
                outcome?;
                Ok(json!({ "tab": tab_id, "pasted": pasted, "mime": mime, "bytes": payload.len(),
                           "via": "system clipboard + Ctrl+V",
                           "clipboardRestoreScheduled": restored,
                           "clipboardWas": saved.as_ref().map(|(m, b)| format!("{m} ({} bytes)", b.len())) }))
            })
        }
        "profile" => {
            let action = args.get("action").and_then(Value::as_str).unwrap_or("show");
            let mut prof = cfg.profile_now();
            match action {
                "remember" => {
                    let note = args.get("note").and_then(Value::as_str).ok_or("remember needs note")?;
                    let ts = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let mut hist = prof.data.get("history").and_then(Value::as_array).cloned().unwrap_or_default();
                    let mut entry = json!({ "ts": ts, "note": note });
                    if let Some(agent) = prof.get_str("agent") {
                        entry["agent"] = json!(agent);
                    }
                    hist.push(entry);
                    // Small on purpose: a memory, not a log. Keep the tail.
                    if hist.len() > 50 {
                        let cut = hist.len() - 50;
                        hist.drain(0..cut);
                    }
                    prof.set("history", Value::Array(hist.clone()))?;
                    Ok(json!({ "remembered": note, "entries": hist.len(), "path": prof.path.display().to_string() }))
                }
                "history" => {
                    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
                    let hist = prof.data.get("history").and_then(Value::as_array).cloned().unwrap_or_default();
                    let start = hist.len().saturating_sub(limit);
                    Ok(json!({ "name": prof.name, "total": hist.len(), "recent": &hist[start..] }))
                }
                "show" => Ok(json!({
                    "name": prof.name,
                    "path": prof.path.display().to_string(),
                    "values": prof.data,
                    "screenshotDir": prof.shot_dir().display().to_string(),
                })),
                "set" => {
                    let key = args.get("key").and_then(Value::as_str).ok_or("set needs key")?;
                    let value = args.get("value").cloned().ok_or("set needs value")?;
                    prof.set(key, value.clone())?;
                    Ok(json!({ "name": prof.name, "path": prof.path.display().to_string(),
                               "set": { key: value } }))
                }
                other => Err(format!("unknown profile action: {other}")),
            }
        }
        "shot" => {
            let prof = cfg.profile_now();
            let fmt = args
                .get("format")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| prof.get_str("shot_format"))
                .unwrap_or_else(|| "png".into());
            let path = match args.get("path").and_then(Value::as_str) {
                Some(p) => p.to_string(),
                None => {
                    // No path given: use the profile's folder and a unique name,
                    // so an agent sets this once instead of inventing a name
                    // for every capture.
                    let dir = prof.shot_dir();
                    std::fs::create_dir_all(&dir)
                        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
                    let ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0);
                    dir.join(format!("shot-{ms}.{fmt}")).display().to_string()
                }
            };
            let mode = args.get("mode").and_then(Value::as_str).unwrap_or("viewport").to_string();
            let quality = args
                .get("quality")
                .and_then(Value::as_u64)
                .or_else(|| prof.get_u64("shot_quality"))
                .unwrap_or(85) as u32;
            let region = parse_region(args.get("region"));
            let index = args.get("i").and_then(Value::as_u64);
            // An explicit empty `within` means unscoped, as it does for elements;
            // only an absent one falls back to the tab's last scope.
            let within_given = matches!(args.get("within"), Some(v) if !v.is_null());
            let within = match args.get("within") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => nonempty_scope(s),
                Some(_) => return Err("shot: within must be a CSS selector".into()),
            };
            let padding = match args.get("padding") {
                None | Some(Value::Null) => 0.0,
                Some(v) => v
                    .as_f64()
                    .ok_or("shot: padding must be a number of pixels")?
                    .clamp(0.0, 200.0),
            };
            if index.is_some() && region.is_some() {
                return Err("shot: i and region name two different captures; use one".into());
            }
            if index.is_some() && mode == "full" {
                return Err("shot: i cannot be combined with mode \"full\"".into());
            }
            with_conn(&cfg.endpoint, tab, |conn, tab_id| {
                // Resolve an element before anything is woken or borrowed: a
                // missing, detached or zero-size index must not leave a live
                // screencast and focus emulation behind, or spend 800 ms waking
                // the compositor just to fail.
                let (effective_mode, region, element) = match index {
                    Some(i) => {
                        let scope = if within_given { within.clone() } else { scope_for(tab_id) };
                        let at = pick(conn, scope.as_deref(), i, true, None)?;
                        if at.get("connected").and_then(Value::as_bool) == Some(false) {
                            return Err(format!("shot: element {i} is detached; call elements again"));
                        }
                        let rect = at
                            .get("rect")
                            .ok_or_else(|| format!("shot: the page did not report bounds for element {i}"))?;
                        let (x, y, w, h) = (
                            rect.get("x").and_then(Value::as_f64).unwrap_or(0.0),
                            rect.get("y").and_then(Value::as_f64).unwrap_or(0.0),
                            rect.get("w").and_then(Value::as_f64).unwrap_or(0.0),
                            rect.get("h").and_then(Value::as_f64).unwrap_or(0.0),
                        );
                        if w < 1.0 || h < 1.0 {
                            return Err(format!("shot: element {i} has no visible box to capture"));
                        }
                        let (px, py, pw, ph) = padded_box(x, y, w, h, padding);
                        let element = json!({
                            "i": i, "tag": at.get("tag"), "label": at.get("label"),
                            "x": px.round(), "y": py.round(), "w": pw.round(), "h": ph.round(),
                        });
                        ("custom".to_string(), Some((px, py, pw, ph)), Some(element))
                    }
                    None => (mode.clone(), region, None),
                };
                // Someone else's tab: let it render just long enough to be
                // captured, then hand its normal behaviour back.
                let borrowed = !cdp::is_owned(tab_id);
                if borrowed {
                    conn.set_focus_emulation(true).ok();
                }
                // A capture needs a frame from the browser's compositor, and a
                // window that niri is not showing produces none: measured 12 s
                // either way with `fromSurface` true or false, against ~150 ms
                // with the window on the visible workspace. A live screencast
                // forces frames while it runs, so the frame arriving (or not)
                // is also the honest answer to "can this be captured at all".
                if let Err(why) = conn.start_frames() {
                    // Hand the tab back before returning: a borrowed tab must not
                    // keep rendering because a capture failed.
                    if borrowed {
                        let _ = conn.set_focus_emulation(false);
                    }
                    return Err(format!(
                        "no frame: the browser window is not on the workspace you are viewing, \
                         so it is not being rendered. Switch to it and retry, or skip the \
                         screenshot — every other tool works without it. ({why})"
                    ));
                }
                let shot = capture_png(conn, &effective_mode, region, None, &fmt, quality);
                conn.stop_frames();
                if borrowed {
                    let _ = conn.set_focus_emulation(false);
                }
                let (bytes, clipped, _mime) = shot?;
                std::fs::write(&path, &bytes).map_err(|e| format!("cannot write {path}: {e}"))?;
                let mut out = json!({
                    "tab": tab_id, "path": path, "bytes": bytes.len(),
                    // The CDP mode is "custom" for an element; the caller is
                    // told what it actually captured.
                    "mode": if index.is_some() { "element" } else { effective_mode.as_str() },
                    "format": fmt, "clipped": clipped,
                });
                if let Some(el) = element {
                    out["element"] = el;
                }
                Ok(out)
            })
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

/* ------------------------------- encoders -------------------------------- */

fn b64decode(s: &str) -> Result<Vec<u8>, String> {
    let mut table = [255u8; 256];
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for (i, c) in alphabet.iter().enumerate() {
        table[*c as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u8;
    for c in s.bytes() {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        let v = table[c as usize];
        if v == 255 {
            return Err("invalid base64 in screenshot".into());
        }
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Ok(out)
}

/* ------------------------------- transport -------------------------------- */

/// Which request is running, and which were cancelled before they started.
#[derive(Default)]
struct Inflight {
    current: Option<String>,
    cancelled: HashSet<String>,
}

/// One JSON-RPC message per line. Both threads write, so the line is written
/// under the stdout lock and never interleaves with another.
fn send(v: Value) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

fn main() {
    let mut endpoint = DEFAULT_ENDPOINT.to_string();
    let mut profile_name = std::env::var("CDP_PROFILE").unwrap_or_else(|_| "default".to_string());
    let mut allow_remote = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                println!("{}", help());
                return;
            }
            "-V" | "--version" => {
                println!("cdp-mcp {VERSION}");
                return;
            }
            "--cdp" => endpoint = args.next().unwrap_or_else(|| DEFAULT_ENDPOINT.to_string()),
            "--profile" => profile_name = args.next().unwrap_or_else(|| "default".to_string()),
            "--allow-remote" => allow_remote = true,
            other => {
                eprintln!("cdp-mcp: unknown argument {other}\n{}", help());
                std::process::exit(2);
            }
        }
    }
    if allow_remote && !cfg!(feature = "experimental") {
        eprintln!(
            "cdp-mcp: --allow-remote is an experimental build feature.\n\
             CDP has no authentication, so this build refuses non-loopback endpoints.\n\
             Build with `cargo build --features experimental` if you mean it."
        );
        std::process::exit(3);
    }
    if !remote_allowed(is_loopback(&endpoint), allow_remote) {
        eprintln!(
            "cdp-mcp: refusing non-loopback endpoint {endpoint}.\n\
             CDP has no authentication: anything that can reach it controls your browser."
        );
        std::process::exit(3);
    }

    let profile = Profile::load(&profile_name);
    if let Some(ms) = profile.get_u64("call_timeout_ms") {
        std::env::set_var("CDP_MCP_TIMEOUT_MS", ms.to_string());
    }
    let cfg = Cfg { endpoint, profile };

    // Stdin is read on its own thread so a cancellation (or a ping) is seen
    // while a tool is still running; the tools themselves run one at a time on
    // this thread, in arrival order, exactly as before.
    let inflight: std::sync::Arc<Mutex<Inflight>> = Default::default();
    let (tx, rx) = std::sync::mpsc::channel::<Value>();
    {
        let inflight = inflight.clone();
        std::thread::spawn(move || {
            for line in io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let msg: Value = match serde_json::from_str(line) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("cdp-mcp: bad json on stdin: {e}");
                        continue;
                    }
                };
                match msg.get("method").and_then(Value::as_str).unwrap_or("") {
                    "notifications/cancelled" => {
                        let Some(rid) = msg.pointer("/params/requestId") else { continue };
                        let rid = rid.to_string();
                        let mut f = inflight.lock().unwrap();
                        if f.current.as_deref() == Some(rid.as_str()) {
                            CANCELLED.store(true, std::sync::atomic::Ordering::Relaxed);
                        } else {
                            f.cancelled.insert(rid);
                        }
                    }
                    "ping" => send(json!({ "jsonrpc": "2.0", "id": msg.get("id"), "result": {} })),
                    _ => {
                        if tx.send(msg).is_err() {
                            break;
                        }
                    }
                }
            }
            // The client is gone: stop whatever is running instead of finishing
            // a long flow for nobody.
            CANCELLED.store(true, std::sync::atomic::Ordering::Relaxed);
        });
    }

    for msg in rx {
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        match method {
            "initialize" => {
                let proto = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or("2024-11-05");
                send(json!({ "jsonrpc": "2.0", "id": id, "result": {
                    "protocolVersion": proto,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "cdp", "version": VERSION },
                }}));
            }
            m if m.starts_with("notifications/") => {}
            "tools/list" => send(json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tool_defs() } })),
            "tools/call" => {
                let rid = id.to_string();
                {
                    let mut f = inflight.lock().unwrap();
                    if f.cancelled.remove(&rid) {
                        continue; // cancelled before it started: never run it
                    }
                    f.current = Some(rid);
                    CANCELLED.store(false, std::sync::atomic::Ordering::Relaxed);
                }
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let result = call_tool(&cfg, name, &args);
                let was_cancelled = {
                    let mut f = inflight.lock().unwrap();
                    f.current = None;
                    CANCELLED.swap(false, std::sync::atomic::Ordering::Relaxed)
                };
                if was_cancelled {
                    continue; // the client has stopped listening for this reply
                }
                match result {
                    Ok(v) => {
                        let content = match v.get("__content").and_then(Value::as_array) {
                            Some(blocks) => blocks.clone(),
                            None => vec![json!({ "type": "text", "text": v.to_string() })],
                        };
                        send(json!({ "jsonrpc": "2.0", "id": id, "result": { "content": content } }));
                    }
                    Err(e) => {
                        let content = vec![json!({ "type": "text", "text": format!("ERROR: {e}") })];
                        send(json!({ "jsonrpc": "2.0", "id": id, "result": {
                            "content": content,
                            "isError": true
                        }}));
                    }
                }
            }
            other => send(json!({ "jsonrpc": "2.0", "id": id,
                                  "error": { "code": -32601, "message": format!("method not found: {other}") } })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_carry_key_code_and_text() {
        assert_eq!(key_spec("Enter"), ("Enter".into(), "Enter".into(), 13, Some("\r".into())));
        assert_eq!(key_spec("a"), ("a".into(), "KeyA".into(), 65, Some("a".into())));
        assert_eq!(key_spec("7"), ("7".into(), "Digit7".into(), 55, Some("7".into())));
        assert_eq!(key_spec("F5"), ("F5".into(), "F5".into(), 116, None));
        assert_eq!(key_spec("escape").0, "Escape");
        assert_eq!(key_spec("space").1, "Space");
        // punctuation types itself even without a physical code
        assert_eq!(key_spec("/").3, Some("/".into()));
    }

    #[test]
    fn clip_str_never_splits_a_character() {
        let s = "é".repeat(10); // two bytes each
        assert_eq!(clip_str(&s, 5), "éé");
        assert_eq!(clip_str(&s, 6), "ééé");
        assert_eq!(clip_str("short", 300), "short");
    }

    #[test]
    fn only_loopback_endpoints_are_allowed_by_default() {
        assert!(is_loopback("http://127.0.0.1:9222"));
        assert!(is_loopback("http://localhost:9222"));
        assert!(is_loopback("http://[::1]:9222"));
        assert!(!is_loopback("http://192.168.1.4:9222"));
        assert!(!is_loopback("http://example.com:9222"));
    }

    #[test]
    fn irreversible_matches_final_confirmations_only() {
        for l in ["Post", "publish", "Share now", "Send", "Place order", "Delete post"] {
            assert!(irreversible(l), "{l}");
        }
        for l in ["Share", "Postcode", "Sender", "Settings"] {
            assert!(!irreversible(l), "{l}");
        }
    }

    #[test]
    fn a_non_loopback_endpoint_needs_the_experimental_feature() {
        assert!(remote_allowed(true, false));
        assert!(remote_allowed(true, true));
        assert!(!remote_allowed(false, false));
        assert_eq!(remote_allowed(false, true), cfg!(feature = "experimental"));
    }

    #[test]
    fn modifier_names_map_to_cdp_bits() {
        assert_eq!(mod_mask(Some(&json!(["Shift", "ctrl"]))), 8 | 2);
        assert_eq!(mod_mask(Some(&json!(["Alt", "Meta"]))), 1 | 4);
        assert_eq!(mod_mask(None), 0);
    }

    #[test]
    fn a_bare_document_is_suspected_but_a_loaded_one_is_not() {
        // Truly empty document: nothing to scan and nothing to hold it up.
        assert!(suspect_render(0, 2));
        assert!(suspect_render(0, 399));
        // A loaded page with no interactive elements (an article, a scoped scan
        // that matched nothing): many nodes, so this is a real answer.
        assert!(!suspect_render(0, 812));
        assert!(!suspect_render(2, 900));
        // Elements found: never suspect, regardless of node count.
        assert!(!suspect_render(5, 100));
    }

    #[test]
    fn base64_round_trips() {
        for n in 0..10 {
            let bytes: Vec<u8> = (0..n * 7 + 1).map(|b| (b * 37) as u8).collect();
            assert_eq!(b64decode(&b64encode(&bytes)).unwrap(), bytes);
        }
        assert!(b64decode("not*base64").is_err());
    }

    #[test]
    fn scan_placeholders_are_always_substituted() {
        // An unsubstituted placeholder is a ReferenceError in the page.
        for js in [scan_js(None), scan_js(Some("main"))] {
            assert!(!js.contains("__SCOPE__") && !js.contains("__PICK__"));
        }
        assert!(scan_js(Some("main")).contains(r#"const SCOPE = "main";"#));
    }

    #[test]
    fn tool_schemas_are_well_formed() {
        let defs = tool_defs();
        let names: Vec<&str> = defs.as_array().unwrap().iter()
            .map(|t| t["name"].as_str().unwrap()).collect();
        for want in ["status", "tabs", "eval", "elements", "input", "shot", "paste", "flow", "emulate", "pdf"] {
            assert!(names.contains(&want), "{want} missing");
        }
        for t in defs.as_array().unwrap() {
            assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
        }
    }

    #[test]
    fn paths_follow_each_platforms_conventions() {
        use std::path::PathBuf;

        let linux = |k: &str| match k { "HOME" => Some("/home/ben".to_string()), _ => None };
        assert_eq!(config_dir_for(Os::Linux, &linux), PathBuf::from("/home/ben/.config/cdp-mcp"));
        assert_eq!(data_dir_for(Os::Linux, &linux), PathBuf::from("/home/ben/.local/share/cdp-mcp"));

        let linux_xdg = |k: &str| match k {
            "HOME" => Some("/home/ben".to_string()),
            "XDG_CONFIG_HOME" => Some("/x/cfg".to_string()),
            "XDG_DATA_HOME" => Some("/x/data".to_string()),
            _ => None,
        };
        assert_eq!(config_dir_for(Os::Linux, &linux_xdg), PathBuf::from("/x/cfg/cdp-mcp"));
        assert_eq!(data_dir_for(Os::Linux, &linux_xdg), PathBuf::from("/x/data/cdp-mcp"));

        let mac = |k: &str| match k { "HOME" => Some("/Users/ben".to_string()), _ => None };
        assert_eq!(config_dir_for(Os::Mac, &mac),
                   PathBuf::from("/Users/ben/Library/Application Support/cdp-mcp"));
        assert_eq!(data_dir_for(Os::Mac, &mac),
                   PathBuf::from("/Users/ben/Library/Application Support/cdp-mcp"));

        let mac_xdg = |k: &str| match k {
            "HOME" => Some("/Users/ben".to_string()),
            "XDG_CONFIG_HOME" => Some("/x/cfg".to_string()),
            _ => None,
        };
        assert_eq!(config_dir_for(Os::Mac, &mac_xdg), PathBuf::from("/x/cfg/cdp-mcp"));

        let win = |k: &str| match k {
            "USERPROFILE" => Some("C:/Users/ben".to_string()),
            "APPDATA" => Some("C:/Users/ben/AppData/Roaming".to_string()),
            "LOCALAPPDATA" => Some("C:/Users/ben/AppData/Local".to_string()),
            _ => None,
        };
        assert_eq!(config_dir_for(Os::Windows, &win), PathBuf::from("C:/Users/ben/AppData/Roaming/cdp-mcp"));
        assert_eq!(data_dir_for(Os::Windows, &win), PathBuf::from("C:/Users/ben/AppData/Local/cdp-mcp"));

        // Without APPDATA/LOCALAPPDATA the profile directory is the convention.
        let win_fallback = |k: &str| match k { "USERPROFILE" => Some("C:/Users/ben".to_string()), _ => None };
        assert_eq!(config_dir_for(Os::Windows, &win_fallback),
                   PathBuf::from("C:/Users/ben/AppData/Roaming/cdp-mcp"));
        assert_eq!(data_dir_for(Os::Windows, &win_fallback),
                   PathBuf::from("C:/Users/ben/AppData/Local/cdp-mcp"));
    }

    #[test]
    fn an_empty_xdg_value_is_ignored() {
        let get = |k: &str| match k {
            "HOME" => Some("/home/ben".to_string()),
            "XDG_CONFIG_HOME" => Some(String::new()),
            _ => None,
        };
        assert_eq!(config_dir_for(Os::Linux, &get), std::path::PathBuf::from("/home/ben/.config/cdp-mcp"));
    }

    #[test]
    fn a_multiple_selection_compares_as_a_set() {
        let got = json!(["c", "a"]);
        assert!(selection_matches(&["a".into(), "c".into()], &got));
        assert!(selection_matches(&["c".into(), "a".into(), "c".into()], &got));
        assert!(!selection_matches(&["a".into(), "b".into()], &got));
        assert!(!selection_matches(&[], &got));
        // A page that reports a string where an array was asked for is not a match.
        assert!(!selection_matches(&["a".into()], &json!("a")));
    }

    #[test]
    fn select_values_accepts_one_or_many() {
        assert_eq!(select_values(Some(&json!("pro"))).unwrap(), json!("pro"));
        assert_eq!(select_values(Some(&json!(["a", "c"]))).unwrap(), json!(["a", "c"]));
        assert!(select_values(Some(&json!([]))).unwrap_err().contains("at least one"));
        assert!(select_values(Some(&json!(5))).unwrap_err().contains("string or an array"));
        assert!(select_values(Some(&json!([1]))).unwrap_err().contains("must be strings"));
        assert!(select_values(None).unwrap_err().contains("needs value"));
    }

    #[test]
    fn element_padding_widens_without_crossing_the_origin() {
        assert_eq!(padded_box(100.0, 200.0, 160.0, 40.0, 20.0), (80.0, 180.0, 200.0, 80.0));
        assert_eq!(padded_box(10.0, 5.0, 160.0, 40.0, 20.0), (0.0, 0.0, 190.0, 65.0));
        assert_eq!(padded_box(0.0, 0.0, 160.0, 40.0, 0.0), (0.0, 0.0, 160.0, 40.0));
    }

    #[test]
    fn a_screenshot_reports_the_mime_it_really_is() {
        assert_eq!(mime_for("png"), "image/png");
        assert_eq!(mime_for("webp"), "image/webp");
        assert_eq!(mime_for("jpeg"), "image/jpeg");
        assert_eq!(mime_for("jpg"), "image/jpeg");
    }

    
}
