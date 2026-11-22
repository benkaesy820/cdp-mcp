// File-shaped tools: downloads, uploads and page-targeted paste.

use serde_json::{json, Value};

use crate::cdp;
use crate::input::{click_at, click_index};
use crate::scan::pick;
use crate::{b64decode, b64encode, nap, nonempty_scope, scope_for, with_conn, Cfg};

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

pub(crate) fn download(cfg: &Cfg, args: &Value) -> Result<Value, String> {
    let tab = args.get("tab").and_then(Value::as_str);
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
        // The limit is the caller's, so name it here instead of letting a page
        // that reported `too large` fall through to the cache, which would only
        // hand back the same oversized body.
        if v.get("error").and_then(Value::as_str) == Some("too large") {
            let size = v.get("size").and_then(Value::as_u64).unwrap_or(0);
            return Err(format!(
                "download refused: {size} bytes is over the {max_bytes}-byte limit; raise \
                 maxBytes if that is intended"
            ));
        }
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
        // The page-side fetch refuses an oversized body, but its reply only
        // sends the fallback down the page-cache path, which hands back the
        // whole resource. Enforce the limit on the bytes that are about to be
        // written, whatever path produced them.
        if bytes.len() as u64 > max_bytes {
            return Err(format!(
                "download refused: {} bytes is over the {max_bytes}-byte limit; raise maxBytes \
                 if that is intended",
                bytes.len()
            ));
        }
        std::fs::write(&path, &bytes).map_err(|e| format!("cannot write {path}: {e}"))?;
        Ok(json!({ "tab": tab_id, "path": path, "url": src, "bytes": bytes.len() }))
    })
}

/// A validated upload request. Pure, so the argument rules are unit-tested
/// instead of discovered after interception is armed on someone's tab.
#[derive(Debug, PartialEq)]
struct Upload {
    paths: Vec<String>,
    /// Click this element and intercept the chooser it opens. None is the
    /// DOM-selector path.
    index: Option<u64>,
    selector: String,
    dom_index: usize,
    timeout_ms: u64,
}

fn plan(args: &Value) -> Result<Upload, String> {
    let paths = match (args.get("path"), args.get("paths")) {
        (Some(Value::String(p)), None) => vec![p.clone()],
        (None, Some(Value::Array(a))) => {
            if a.is_empty() {
                return Err("paths needs at least one file".into());
            }
            let mut out = Vec::with_capacity(a.len());
            for x in a {
                out.push(x.as_str().ok_or("paths must be strings")?.to_string());
            }
            out
        }
        (Some(_), Some(_)) => return Err("upload: give path or paths, not both".into()),
        (Some(_), None) => return Err("path must be a string".into()),
        (None, Some(_)) => return Err("paths must be an array of strings".into()),
        (None, None) => return Err("upload needs path or paths".into()),
    };
    // Present and non-null: an explicit `null` means absent, while a string or a
    // negative number is a mistake that must not silently fall through to the
    // selector path.
    let index = match args.get("i") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().ok_or("upload: i must be a whole number")?),
    };
    let has_selector = matches!(args.get("selector"), Some(v) if !v.is_null());
    let has_index = matches!(args.get("index"), Some(v) if !v.is_null());
    if index.is_some() && (has_selector || has_index) {
        return Err(
            "upload: i clicks a chooser open, so it cannot be combined with selector or index"
                .into(),
        );
    }
    let selector = match args.get("selector") {
        None | Some(Value::Null) => "input[type=file]".to_string(),
        Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
        Some(Value::String(_)) => return Err("upload: selector must not be empty".into()),
        Some(_) => return Err("upload: selector must be a string".into()),
    };
    let dom_index = match args.get("index") {
        None | Some(Value::Null) => 0usize,
        Some(v) => v.as_u64().ok_or("upload: index must be a whole number")? as usize,
    };
    Ok(Upload {
        paths,
        index,
        selector,
        dom_index,
        timeout_ms: args
            .get("timeoutMs")
            .and_then(Value::as_u64)
            .unwrap_or(5_000)
            .clamp(500, 15_000),
    })
}

pub(crate) fn upload(cfg: &Cfg, args: &Value) -> Result<Value, String> {
    let plan = plan(args)?;
    // A missing file is the caller's mistake; discovering it after the chooser is
    // open and interception is armed on their tab is not acceptable. Chromium
    // resolves a path on its own side, so a relative one is made absolute here.
    let mut paths = Vec::with_capacity(plan.paths.len());
    for p in &plan.paths {
        match std::fs::metadata(p) {
            Ok(m) if m.is_file() => {}
            Ok(_) => return Err(format!("not a file: {p}")),
            Err(_) => return Err(format!("no such file: {p}")),
        }
        paths.push(
            std::fs::canonicalize(p)
                .map_err(|e| format!("cannot resolve {p}: {e}"))?
                .display()
                .to_string(),
        );
    }
    let plan = Upload { paths, ..plan };
    let tab = args.get("tab").and_then(Value::as_str);
    match plan.index {
        None => with_conn(&cfg.endpoint, tab, |conn, tab_id| dom_upload(conn, tab_id, &plan)),
        Some(i) => {
            let within_given = matches!(args.get("within"), Some(v) if !v.is_null());
            let within = match args.get("within") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => nonempty_scope(s),
                Some(_) => return Err("upload: within must be a CSS selector".into()),
            };
            with_conn(&cfg.endpoint, tab, |conn, tab_id| {
                let scope = if within_given { within.clone() } else { scope_for(tab_id) };
                conn.begin_file_chooser()?;
                let out = click_upload(conn, tab_id, &plan, i, scope.as_deref());
                conn.end_file_chooser();
                out
            })
        }
    }
}

fn uploaded(paths: &[String]) -> Value {
    if paths.len() == 1 {
        json!(paths[0])
    } else {
        json!(paths)
    }
}

fn dom_upload(conn: &mut cdp::Conn, tab_id: &str, plan: &Upload) -> Result<Value, String> {
    let doc = conn.call("DOM.getDocument", json!({ "depth": 0, "pierce": true }))?;
    let root = doc.pointer("/root/nodeId").and_then(Value::as_u64).ok_or("no document")?;
    let found = conn.call("DOM.querySelectorAll", json!({ "nodeId": root, "selector": plan.selector }));
    let node = found
        .ok()
        .and_then(|v| v.get("nodeIds").and_then(Value::as_array).cloned())
        .and_then(|ids| ids.get(plan.dom_index).and_then(Value::as_u64))
        .unwrap_or(0);
    if node == 0 {
        let n = conn.eval("document.querySelectorAll('input[type=file]').length")
            .ok().and_then(|v| v.as_i64()).unwrap_or(0);
        return Err(format!(
            "no element matched {} at index {} ({} file inputs on the page). \
             If the picker only exists after clicking a button, upload with `i` instead.",
            plan.selector, plan.dom_index, n
        ));
    }
    conn.call("DOM.setFileInputFiles", json!({ "nodeId": node, "files": plan.paths }))?;
    Ok(json!({ "tab": tab_id, "uploaded": uploaded(&plan.paths),
               "selector": plan.selector, "index": plan.dom_index, "ok": true }))
}

/// Click an element and fulfil the chooser it opens. Interception is armed by the
/// caller and disarmed on every exit path.
fn click_upload(
    conn: &mut cdp::Conn,
    tab_id: &str,
    plan: &Upload,
    i: u64,
    scope: Option<&str>,
) -> Result<Value, String> {
    let at = pick(conn, scope, i, true, None)?;
    if at.get("connected").and_then(Value::as_bool) == Some(false) {
        return Err(format!("upload: element {i} is detached; call elements again"));
    }
    if at.get("zero").and_then(Value::as_bool) == Some(true) {
        return Err(format!("upload: element {i} has no visible box to click"));
    }
    let x = at.get("x").and_then(Value::as_f64).unwrap_or(0.0);
    let y = at.get("y").and_then(Value::as_f64).unwrap_or(0.0);
    if let Err(e) = click_at(conn, x, y) {
        if let Some(d) = conn.dialog.clone() {
            return Ok(json!({ "tab": tab_id, "clicked": i, "files": plan.paths, "dialog": d }));
        }
        return Err(e);
    }
    let Some(ev) = conn.wait_file_chooser(plan.timeout_ms)? else {
        if let Some(d) = conn.dialog.clone() {
            return Ok(json!({ "tab": tab_id, "clicked": i, "files": plan.paths, "dialog": d }));
        }
        return Err(format!(
            "no file chooser opened within {} ms: element {i} did not open one \
             (interception is off again)",
            plan.timeout_ms
        ));
    };
    let mode = ev.get("mode").and_then(Value::as_str).unwrap_or("").to_string();
    if mode == "selectSingle" && plan.paths.len() > 1 {
        return Err(format!(
            "this chooser takes one file, but {} were given: {}",
            plan.paths.len(),
            plan.paths.join(", ")
        ));
    }
    let node = ev.get("backendNodeId").and_then(Value::as_u64).ok_or_else(|| {
        "the file chooser has no input node: showOpenFilePicker-style pickers cannot be \
         fulfilled over CDP"
            .to_string()
    })?;
    conn.call("DOM.setFileInputFiles", json!({ "backendNodeId": node, "files": plan.paths }))?;
    Ok(json!({ "tab": tab_id, "clicked": i, "mode": mode,
               "files": plan.paths, "ok": true }))
}

pub(crate) fn paste(cfg: &Cfg, args: &Value) -> Result<Value, String> {
    let tab = args.get("tab").and_then(Value::as_str);
    let path = args.get("path").and_then(Value::as_str).map(str::to_string);
    let text = args.get("text").and_then(Value::as_str).map(str::to_string);
    let click_i = args.get("i").and_then(Value::as_u64);
    if path.is_none() && text.is_none() {
        return Err("paste needs path (a file) or text".into());
    }
    if text.as_deref() == Some("") {
        return Err("paste text is empty".into());
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
    with_conn(&cfg.endpoint, tab, |conn, tab_id| {
        // focus the target first, otherwise the paste lands wherever the
        // caret happens to be
        if let Some(i) = click_i {
            let scope = scope_for(tab_id);
            click_index(conn, scope.as_deref(), i)?;
        }

        // Hand the data to the page as a paste event carrying it. The system
        // clipboard is not an option: on Wayland only the focused app may read
        // it, and the browser is deliberately kept unfocused so the human keeps
        // their keyboard (measured: Ctrl+V into the unfocused browser pasted
        // nothing, focused it pasted). So this method needs no clipboard at all.
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
        Ok(json!({ "tab": tab_id, "pasted": pasted, "mime": mime,
                   "bytes": payload.len(), "via": how }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_path_or_many_is_a_list() {
        let one = plan(&json!({ "path": "/tmp/a" })).unwrap();
        assert_eq!(one.paths, vec!["/tmp/a"]);
        assert_eq!(
            (one.index, one.selector.as_str(), one.dom_index, one.timeout_ms),
            (None, "input[type=file]", 0, 5000)
        );

        let two = plan(&json!({ "paths": ["/tmp/a", "/tmp/b"] })).unwrap();
        assert_eq!(two.paths, vec!["/tmp/a", "/tmp/b"]);
    }

    #[test]
    fn conflicting_or_empty_arguments_are_named() {
        let err = |v: Value| plan(&v).unwrap_err();
        assert!(err(json!({ "path": "/tmp/a", "paths": ["/tmp/b"] })).contains("not both"));
        assert!(err(json!({})).contains("needs path or paths"));
        assert!(err(json!({ "paths": [] })).contains("at least one"));
        assert!(err(json!({ "paths": [1] })).contains("must be strings"));
        assert!(err(json!({ "path": "/tmp/a", "i": 3, "selector": "#x" }))
            .contains("cannot be combined"));
        assert!(err(json!({ "path": "/tmp/a", "i": 3, "index": 1 }))
            .contains("cannot be combined"));
        // A present-but-wrong i must not silently become the selector path.
        assert!(err(json!({ "path": "/tmp/a", "i": "2" })).contains("whole number"));
        assert!(err(json!({ "path": "/tmp/a", "i": -1 })).contains("whole number"));
        assert!(err(json!({ "path": "/tmp/a", "i": 2, "selector": 5 })).contains("cannot be combined"));
        assert!(err(json!({ "path": "/tmp/a", "selector": 5 })).contains("must be a string"));
        assert!(err(json!({ "path": "/tmp/a", "selector": "  " })).contains("must not be empty"));
        // Explicit nulls read as absent, as they do everywhere else.
        assert_eq!(plan(&json!({ "path": "/tmp/a", "i": null, "selector": null })).unwrap().index, None);
        assert_eq!(plan(&json!({ "path": "/tmp/a", "i": 2, "selector": null })).unwrap().index, Some(2));
    }

    #[test]
    fn the_wait_is_bounded() {
        assert_eq!(plan(&json!({ "path": "/tmp/a", "timeoutMs": 100 })).unwrap().timeout_ms, 500);
        assert_eq!(
            plan(&json!({ "path": "/tmp/a", "timeoutMs": 99999 })).unwrap().timeout_ms,
            15000
        );
        assert_eq!(plan(&json!({ "path": "/tmp/a", "i": 2, "within": "main" })).unwrap().index, Some(2));
    }
}
