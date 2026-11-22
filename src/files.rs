// File-shaped tools: images, links, downloads, uploads and pasted pixels.

use serde_json::{json, Value};

use crate::cdp;
use crate::clip::{clip_append, clip_backend, clip_read, clip_restore_script, clip_save, clip_stage, clip_temp, clip_write};
use crate::input::click_index;
use crate::scan::pick;
use crate::{b64decode, b64encode, current_os, nap, scope_for, with_conn, Cfg, Os};

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

pub(crate) fn images(cfg: &Cfg, args: &Value) -> Result<Value, String> {
    let tab = args.get("tab").and_then(Value::as_str);
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

pub(crate) fn link(cfg: &Cfg, args: &Value) -> Result<Value, String> {
    let tab = args.get("tab").and_then(Value::as_str);
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

pub(crate) fn image(cfg: &Cfg, args: &Value) -> Result<Value, String> {
    let tab = args.get("tab").and_then(Value::as_str);
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

pub(crate) fn upload(cfg: &Cfg, args: &Value) -> Result<Value, String> {
    let tab = args.get("tab").and_then(Value::as_str);
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

