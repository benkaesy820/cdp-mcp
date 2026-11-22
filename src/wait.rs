// Waiting for a page: the watcher it installs, the decisions it makes, and the
// two text/idle waits built on it. Self-contained — it talks to the page through
// `cdp::Conn` and to the caller through `check_cancel`/`nap`, nothing else.

use serde_json::{json, Value};

use crate::{cdp, check_cancel, nap};

/// Wait in the page for `want` to appear: a MutationObserver wakes it the
/// moment the DOM changes, instead of polling the whole body every 400 ms.
/// The wait is cut into slices of at most a second so a cancellation, or a
/// navigation that destroys the page's context, is noticed promptly.
pub(crate) fn wait_for_text(conn: &mut cdp::Conn, want: &str, ms: u64) -> Result<bool, String> {
    let started = std::time::Instant::now();
    loop {
        check_cancel()?;

        if conn.dialog.is_some() {
            return Err("dialog open: the page is blocked".into());
        }
        let left = (ms as u128).saturating_sub(started.elapsed().as_millis()) as u64;
        let slice = left.min(1_000);
        let js = format!(
            r#"new Promise((done) => {{
                const w = {w};
                const has = () => {{ const b = document.body;
                    return !!b && (b.innerText || '').includes(w); }};
                if (has()) return done(true);
                let queued = false, over = false;
                const fin = (v) => {{ if (over) return; over = true; obs.disconnect(); clearTimeout(t); done(v); }};
                const obs = new MutationObserver(() => {{
                    if (queued) return; queued = true;
                    setTimeout(() => {{ queued = false; if (has()) fin(true); }}, 40);
                }});
                // `attributes` too: revealing text by removing `hidden` or
                // changing a class mutates no node, so without it the wait only
                // noticed at the end of the slice.
                obs.observe(document.documentElement, {{ subtree: true, childList: true, characterData: true, attributes: true }});
                const t = setTimeout(() => fin(has()), {slice});
            }})"#,
            w = Value::String(want.to_string()),
            slice = slice
        );
        match conn.eval_within(&js, slice + 5_000) {
            Ok(Value::Bool(true)) => return Ok(true),
            // a navigation destroyed the context mid-wait: try again in the new one
            Err(e) if conn.dead => return Err(e),
            Err(e) if conn.dialog.is_some() => return Err(e),
            Err(_) => nap(100)?,
            Ok(_) => {}
        }
        if started.elapsed().as_millis() >= ms as u128 {
            return Ok(false);
        }
    }
}

/// Visible size of the page: light-DOM `innerText` plus the text inside shadow
/// roots, which `innerText` never reaches. Used at install and at the quiet
/// decision, so both sides compare the same thing.
const VISIBLE_JS: &str = r#"(() => {
    const b = document.body;
    let s = 0;
    const walk = (r) => {
        const l = r.querySelectorAll('*');
        for (let i = 0; i < l.length; i++) {
            const sr = l[i].shadowRoot;
            if (sr) { s += (sr.textContent || '').length; walk(sr); }
        }
    };
    walk(document);
    return [b ? (b.innerText || '').length : 0, s];
})()"#;

/// The page-side half of `idle`: watch the document and call a binding when
/// something happens. Nothing here is polled from outside. `MutationObserver`
/// pushes, and the server only ever reads a socket.
///
/// Returns `{r: ...}` because a hit at install time has to hand back a snapshot
/// in the same reply: a binding call made here would arrive before the command's
/// own answer and be discarded by `call`, and a report saying "selector" with
/// nothing else is exactly the case where naming the element is most useful.
///
/// One install per wait, and again after a navigation, because the observer dies
/// with the document while the binding itself survives it (both measured). The
/// binding is checked rather than assumed, so a browser that drops it is caught
/// here instead of turning every later wait into a silent timeout.
fn watch_js(name: &str, until_text: Option<&str>, until_selector: Option<&str>, install: bool) -> String {
    let want = until_text.map(|t| Value::String(t.to_string())).unwrap_or(Value::Bool(false));
    let sel = until_selector.map(|s| Value::String(s.to_string())).unwrap_or(Value::Bool(false));
    format!(
        r#"(() => {{
            const NAME = {name}, WANT = {want}, SEL = {sel};
            const root = document.documentElement;
            if (!root) return {{r:'not-ready'}};
            const SKIP = new Set(['script','style','noscript','template','link','meta','title']);
            // Bounded on purpose: `textContent` builds the whole subtree first,
            // and a route swap can add a container with half the page in it.
            // This stops after 90 characters, which is all `slice(0, 70)` keeps.
            const words = (el) => {{
                let out = '', left = 90, space = false;
                const w = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
                while (left > 0 && w.nextNode()) {{
                    const n = w.currentNode;
                    const p = n.parentElement;
                    if (p && p.closest('script,style,noscript,template')) continue;
                    // Normalise, but keep where the source had a space: inline
                    // runs like <b>Ben</b>k must not become "Ben k", and blank
                    // nodes must not eat the budget before the label starts.
                    const raw = (n.nodeValue || '').replace(/\s+/g, ' ');
                    const t = raw.trim();
                    if (!t) {{ space = true; continue; }}
                    if (out && (space || raw.charAt(0) === ' ')) {{ out += ' '; left -= 1; }}
                    out += t.slice(0, Math.max(0, left));
                    left -= t.length;
                    space = raw.charAt(raw.length - 1) === ' ';
                }}
                return out.slice(0, 70).trim();
            }};
            const dialogs = () => document.querySelectorAll('[role="dialog"],[role="alertdialog"],[aria-modal="true"],dialog[open]').length;
            const describe = (el) => {{
                if (!el || el.nodeType !== 1) return null;
                const tag = el.tagName.toLowerCase();
                if (SKIP.has(tag)) return null;
                const o = {{ tag: tag }};
                const role = el.getAttribute('role');
                if (role) o.role = role;
                const label = el.getAttribute('aria-label') || el.getAttribute('placeholder') || el.getAttribute('name');
                if (label) o.label = label.slice(0, 40);
                const text = words(el).slice(0, 70);
                if (text) o.text = text;
                const fields = [];
                for (const f of el.querySelectorAll('input,select,textarea')) {{
                    const name = f.getAttribute('name') || f.getAttribute('placeholder') || f.getAttribute('type');
                    if (name && fields.indexOf(name) < 0) fields.push(name);
                    if (fields.length >= 4) break;
                }}
                if (fields.length) o.fields = fields;
                return o;
            }};
            const hitSel = () => {{
                if (SEL === false) return false;
                try {{ return !!document.querySelector(SEL); }} catch (e) {{ return false; }}
            }};
            const hitText = () => {{
                if (WANT === false) return false;
                const b = document.body;
                // `innerText`, not `textContent`: textContent includes inline
                // <script> source, so a search for a word the page's own code
                // mentions matched before the page ever showed it.
                return !!b && (b.innerText || '').indexOf(WANT) >= 0;
            }};
            let stopped = false, added = [], addedCount = 0, removed = 0, obs = null;
            // Negative, not zero: `performance.now()` restarts at 0 on a
            // navigation, so a first check compared against 0 could be
            // suppressed for the first 80 ms of the new document.
            let lastSent = -1e9, lastText = -1e9, textTimer = 0, sendTimer = 0, dirty = false;
            const send = (o) => {{ try {{ window[NAME](JSON.stringify(o)); }} catch (e) {{}} }};
            const stop = () => {{
                stopped = true;
                try {{ if (obs) obs.disconnect(); }} catch (e) {{}}
                try {{ if (textTimer) clearTimeout(textTimer); }} catch (e) {{}}
                try {{ if (sendTimer) clearTimeout(sendTimer); }} catch (e) {{}}
                window.__cdpWatch = null;
            }};
            const snapshot = (t, extra) => {{
                const b = document.body;
                const list = extra || added;
                return {{ t: t, url: location.href, title: document.title,
                          chars: b ? (b.innerText || '').length : 0,
                          added: list, addedCount: extra ? list.length : addedCount,
                          removed: removed, dialogs: dialogs() }};
            }};
            // A hit is a change too: it carries what appeared with it, so a
            // caller that only wanted to know "a dialog opened" is already done.
            const finish = (kind) => {{
                stop();
                const o = snapshot(kind);
                // A hit should name what matched even when unrelated additions
                // filled the slots collected for this window.
                if (kind === 'selector') {{
                    try {{
                        const el = describe(document.querySelector(SEL));
                        if (el) o.added = [el].concat(o.added || []).slice(0, 8);
                    }} catch (e) {{}}
                }}
                send(o);
            }};
            // A hit must be immediate. A plain "the page moved" report is worth
            // at most one message per 80 ms: a streaming page produces a batch
            // every frame, and each one used to cost a forced layout plus a CDP
            // message. The quiet clock is still accurate to 80 ms, and nothing
            // that matched is delayed by this.
            const flush = (attrOnly) => {{
                if (stopped) return;
                if (hitSel()) return finish('selector');
                const now = window.performance ? performance.now() : Date.now();
                if (WANT !== false) {{
                    const wait = 50 - (now - lastText);
                    if (wait <= 0) {{
                        lastText = now;
                        if (hitText()) return finish('text');
                    }} else if (!textTimer) {{
                        textTimer = setTimeout(() => {{
                            textTimer = 0;
                            if (stopped) return;
                            lastText = window.performance ? performance.now() : Date.now();
                            if (hitText()) finish('text');
                        }}, Math.max(1, wait));
                    }}
                }}
                // Attribute churn is not a change. Reporting it would keep a
                // page that only toggles classes from ever reaching maxIdleMs.
                if (attrOnly) return;
                const left = 80 - (now - lastSent);
                if (left > 0) {{
                    dirty = true;
                    if (!sendTimer) {{
                        sendTimer = setTimeout(() => {{
                            sendTimer = 0;
                            if (stopped || !dirty) return;
                            dirty = false;
                            lastSent = window.performance ? performance.now() : Date.now();
                            send(snapshot('change'));
                            added = [];
                            addedCount = 0;
                            removed = 0;
                        }}, Math.max(1, left));
                    }}
                    return;
                }}
                if (sendTimer) {{ clearTimeout(sendTimer); sendTimer = 0; }}
                dirty = false;
                lastSent = now;
                send(snapshot('change'));
                // The payload carries this window's additions, and the server
                // sums them; leaving them in place would count them twice.
                added = [];
                addedCount = 0;
                removed = 0;
            }};
            // A previous wait that ended badly can leave its observer running.
            // Stop it before the hit checks, which return early and would
            // otherwise never reach the install that replaces it. Probe mode
            // must not run this: it would kill the watcher it is checking for,
            // and a miss returns without installing a new one.
            if ({install}) {{
                const prev = window.__cdpWatch;
                if (prev) {{ try {{ prev.stop(); }} catch (e) {{}} window.__cdpWatch = null; }}
            }}
            // Already there: answer now, and name the thing that matched.
            if (hitSel()) {{
                let el = null;
                try {{ el = document.querySelector(SEL); }} catch (e) {{}}
                const d = describe(el);
                return {{ r: 'hit:selector', snap: snapshot('selector', d ? [d] : []) }};
            }}
            if (hitText()) return {{ r: 'hit:text', snap: snapshot('text', []) }};
            // Without the binding nothing could ever wake this wait, and the only
            // symptom would be a timeout that looks like a quiet page.
            if (typeof window[NAME] !== 'function') return {{ r: 'nobinding' }};
            // Probe mode: answer the hit question and touch nothing. The
            // boundary look uses it, because stopping or installing there
            // would discard the watcher's buffered window.
            if (!{install}) return {{ r: 'miss' }};
            obs = new MutationObserver((records) => {{
                let moved = false;
                for (const r of records) {{
                    if (r.type === 'attributes') continue;
                    const t = r.target && r.target.nodeType === 1 ? r.target : (r.target ? r.target.parentElement : null);
                    if (t && t.closest && t.closest('script,style,noscript,template')) continue;
                    if (r.type === 'characterData') {{
                        // A price that changes in place has no added node; the
                        // parent's current text is the delta the caller wants.
                        moved = true;
                        const p = r.target.parentElement;
                        const d = p ? describe(p) : null;
                        if (d && added.length < 6) added.push(d);
                        continue;
                    }}
                    moved = true;
                    addedCount += r.addedNodes.length;
                    removed += r.removedNodes.length;
                    for (const n of r.addedNodes) {{
                        if (added.length >= 6) break;
                        // Replacing textContent adds a text node; its parent is
                        // the element whose text changed. Comments and
                        // whitespace are anchors, not changes.
                        const d = describe(
                            n.nodeType === 1 ? n
                            : (n.nodeType === 3 && (n.nodeValue || '').trim() ? n.parentElement : null)
                        );
                        if (d) added.push(d);
                    }}
                }}
                // An attribute change can be what makes SEL or WANT start
                // matching, so it is only ignored when nothing is being watched
                // for.
                if (!moved && SEL === false && WANT === false) return;
                // Flushed straight from the microtask, not on a timer: a
                // MutationObserver callback is a microtask, while `setTimeout` in
                // a tab nobody is looking at is throttled (a 200 ms timer measured
                // 345 ms in a background tab), which would eat the whole win.
                flush(!moved);
            }});
            obs.observe(root, {{ subtree: true, childList: true, characterData: true, attributes: true }});
            window.__cdpWatch = {{ text: WANT, sel: SEL, stop: stop }};
            const vis = {vis};
            return {{ r: 'installed', chars: vis[0], shadow: vis[1] }};
        }})()"#,
        name = Value::String(name.to_string()),
        want = want,
        sel = sel,
        vis = VISIBLE_JS,
        install = install,
    )
}

/// What the wait noticed, accumulated across every pushed change so the caller
/// does not have to observe the page again afterwards.
#[derive(Default)]
struct Settled {
    changes: u64,
    appeared: Vec<Value>,
    seen: std::collections::HashSet<String>,
    added_count: i64,
    removed: i64,
    dialogs: i64,
    url: String,
    title: String,
    chars: i64,
    /// `None` until the first payload: a baseline of 0 is a real value, and could
    /// not be told apart from "not measured yet" when this was a plain i64.
    first_chars: Option<i64>,
    /// Characters inside shadow roots, which `innerText` cannot see.
    shadow_chars: i64,
    navigated: bool,
}

impl Settled {
    /// Fold one payload from the page into the running report.
    fn absorb(&mut self, p: &Value) {
        self.changes += 1;
        if let Some(u) = p.get("url").and_then(Value::as_str) {
            if !self.url.is_empty() && self.url != u {
                self.navigated = true;
            }
            if !u.is_empty() {
                self.url = u.to_string();
            }
        }
        if let Some(t) = p.get("title").and_then(Value::as_str) {
            self.title = t.to_string();
        }
        if let Some(c) = p.get("chars").and_then(Value::as_i64) {
            self.chars = c;
            if self.first_chars.is_none() {
                self.first_chars = Some(c);
            }
        }
        self.added_count += p.get("addedCount").and_then(Value::as_i64).unwrap_or(0);
        self.removed += p.get("removed").and_then(Value::as_i64).unwrap_or(0);
        self.dialogs = self.dialogs.max(p.get("dialogs").and_then(Value::as_i64).unwrap_or(0));
        if let Some(arr) = p.get("added").and_then(Value::as_array) {
            for a in arr {
                // Eight named elements is the token budget for this report; past
                // that the caller wants a scan, not a list, and `addedCount`
                // still tells it how many actually arrived.
                if self.appeared.len() >= 8 {
                    break;
                }
                let key = format!(
                    "{}|{}|{}",
                    a.get("tag").and_then(Value::as_str).unwrap_or(""),
                    a.get("label").and_then(Value::as_str).unwrap_or(""),
                    a.get("text").and_then(Value::as_str).unwrap_or("")
                );
                if self.seen.insert(key) {
                    self.appeared.push(a.clone());
                }
            }
        }
    }

    /// Forget the old document. A report about a page that has been navigated
    /// away from is worse than no report.
    fn reset_document(&mut self) {
        self.appeared.clear();
        self.seen.clear();
        self.added_count = 0;
        self.removed = 0;
        self.dialogs = 0;
        self.url.clear();
        self.title.clear();
        self.chars = 0;
        self.first_chars = None;
        self.shadow_chars = 0;
    }

    /// A wake caused by a dialog: the same report, plus the dialog itself.
    fn dialog_report(&self, d: Value, started: &std::time::Instant, last: &std::time::Instant) -> Value {
        let mut o = self.report("dialog", started, last);
        o["dialog"] = d;
        o
    }

    fn report(&self, woke: &str, started: &std::time::Instant, last: &std::time::Instant) -> Value {
        let mut o = json!({
            "woke": woke,
            "elapsedMs": started.elapsed().as_millis() as u64,
            "quietMs": last.elapsed().as_millis() as u64,
            "changes": self.changes,
        });
        if self.navigated {
            o["navigated"] = json!(true);
        }
        if !self.url.is_empty() {
            o["url"] = json!(self.url);
        }
        if !self.title.is_empty() {
            o["title"] = json!(self.title);
        }
        if self.chars > 0 {
            o["textChars"] = json!(self.chars);
        }
        // A page that started empty and then filled in is exactly the delta a
        // caller wants; `first > 0` used to hide it.
        if let Some(first) = self.first_chars {
            if first != self.chars {
                o["textDelta"] = json!(self.chars - first);
            }
        }
        if self.dialogs > 0 {
            o["dialogs"] = json!(self.dialogs);
        }
        if !self.appeared.is_empty() {
            o["appeared"] = json!(self.appeared);
        }
        if self.added_count > self.appeared.len() as i64 {
            o["addedCount"] = json!(self.added_count);
        }
        if self.removed > 0 {
            o["removed"] = json!(self.removed);
        }
        o
    }
}

/// Stop the page-side watcher. Bounded: a cancelled wait usually means the page
/// is wedged, and a default 30 s eval would make "stop" slower than the wait it
/// is stopping. The two literals used to be duplicated here and in the wrapper.
const STOP_WATCH_JS: &str = "try{window.__cdpWatch&&window.__cdpWatch.stop()}catch(e){}0";

/// Cold-path diagnostics for a timeout: the near miss, not a new snapshot.
/// Only runs when a wait actually expires, behind a 400 ms eval cap.
fn diagnose_js(sel: Option<&str>, text: Option<&str>) -> String {
    let sel = sel.map(|s| Value::String(s.to_string())).unwrap_or(Value::Bool(false));
    let want = text.map(|s| Value::String(s.to_string())).unwrap_or(Value::Bool(false));
    format!(
        r#"(() => {{
            const SEL = {sel}, WANT = {want};
            const out = {{ selector: null, selectorMatches: [], dialogs: [], textNear: "" }};
            const describe = (el) => {{
                if (!el || el.nodeType !== 1) return null;
                const o = {{ tag: el.tagName.toLowerCase() }};
                const role = el.getAttribute('role');
                if (role) o.role = role;
                const label = el.getAttribute('aria-label') || el.getAttribute('placeholder') || el.getAttribute('name');
                if (label) o.label = label.slice(0, 60);
                const t = (el.innerText || el.textContent || '').replace(/\s+/g, ' ').trim().slice(0, 120);
                if (t) o.text = t;
                o.visible = !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
                return o;
            }};
            const roots = [document];
            let nodes = 4000;
            try {{
                for (const el of document.querySelectorAll('*')) {{
                    if (--nodes <= 0 || roots.length >= 40) break;
                    if (el.shadowRoot) roots.push(el.shadowRoot);
                }}
            }} catch (e) {{}}
            const seen = new Set();
            const add = (arr, el) => {{
                const d = describe(el);
                if (!d) return;
                const k = d.tag + '|' + (d.label || '') + '|' + (d.text || '');
                if (seen.has(k)) return;
                seen.add(k);
                arr.push(d);
            }};
            try {{
                for (const root of roots) {{
                    for (const el of root.querySelectorAll('[aria-modal="true"],dialog[open],[role="dialog"],[role="alertdialog"]')) {{
                        if (out.dialogs.length >= 3) break;
                        add(out.dialogs, el);
                    }}
                    if (out.dialogs.length >= 3) break;
                }}
            }} catch (e) {{}}
            if (SEL !== false) {{
                for (const root of roots) {{
                    try {{
                        const el = root.querySelector(SEL);
                        if (el) {{ out.selector = describe(el); break; }}
                    }} catch (e) {{}}
                }}
                if (!out.selector) {{
                    // Strip pseudo-classes and pseudo-elements, but only when
                    // that actually leaves a selector to try.
                    let loose = SEL.replace(/::[\w-]+/g, '').replace(/:([\w-]+)(\([^)]*\))?/g, '');
                    loose = loose.trim();
                    if (loose && loose !== SEL.trim()) {{
                        for (const root of roots) {{
                            try {{
                                for (const el of root.querySelectorAll(loose)) {{
                                    if (out.selectorMatches.length >= 3) break;
                                    add(out.selectorMatches, el);
                                }}
                            }} catch (e) {{}}
                            if (out.selectorMatches.length >= 3) break;
                        }}
                    }}
                }}
            }}
            if (WANT !== false && WANT.length >= 3) {{
                const words = WANT.split(/\s+/).filter(w => w.length >= 4).sort((a, b) => b.length - a.length);
                const candidates = words.length ? words : [WANT];
                let text = '', budget = 100000, nodes2 = 8000;
                const collect = (root) => {{
                    try {{
                        const w = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
                        let n;
                        while (budget > 0 && (n = w.nextNode())) {{
                            const parent = n.parentElement;
                            if (parent && /^(SCRIPT|STYLE|NOSCRIPT|TEMPLATE)$/.test(parent.tagName)) continue;
                            const s = n.nodeValue || '';
                            text += s + ' ';
                            budget -= s.length;
                        }}
                        for (const el of root.querySelectorAll('*')) {{
                            if (budget <= 0 || --nodes2 <= 0) break;
                            if (el.shadowRoot) collect(el.shadowRoot);
                        }}
                    }} catch (e) {{}}
                }};
                collect(document);
                let hit = -1;
                const lower = text.toLowerCase();
                for (const w of candidates) {{
                    const tries = w.length > 4 ? [w.toLowerCase(), w.slice(0, 4).toLowerCase()] : [w.toLowerCase()];
                    for (const needle of tries) {{
                        const i = lower.indexOf(needle);
                        if (i >= 0) {{ hit = i; break; }}
                    }}
                    if (hit >= 0) break;
                }}
                if (hit >= 0) out.textNear = text.slice(Math.max(0, hit - 50), hit + 100).replace(/\s+/g, ' ').trim();
            }}
            return out;
        }})()"#,
        sel = sel,
        want = want,
    )
}



/// Wait for a page to move, woken by the page itself and never asked.
///
/// The page installs one `MutationObserver` and calls a CDP binding when
/// anything appears; this loop just reads the socket. Three consequences:
/// a page that changes nothing costs zero messages, the wake lands within a few
/// milliseconds of the mutation (measured 1.4 to 4.8 ms for the event itself),
/// and every change arrives with what it added, so a caller that only wants to
/// know "what appeared" never has to scan the page again.
///
/// The quiet clock lives here, not in the page: a read timeout is the cheapest
/// timer there is, and Chromium throttles timers in tabs nobody is looking at.
///
/// `started` and `deadline` come from the caller, so a retry after a dropped
/// socket spends what is left of the wait instead of granting a fresh one.
pub(crate) fn wait_page_settled(
    conn: &mut cdp::Conn,
    until_text: Option<&str>,
    until_selector: Option<&str>,
    started: std::time::Instant,
    deadline: std::time::Instant,
    max_idle_ms: u64,
) -> Result<Value, String> {
    let out = wait_page_settled_inner(conn, until_text, until_selector, started, deadline, max_idle_ms);
    if out.is_err() && conn.dialog.is_none() {
        // Every early return used to leave the page's observer running: a
        // cancellation, or a page that refused the binding. It then read
        // `innerText` on every mutation and pushed into a socket nobody was
        // reading, for the rest of that tab's life. A dead socket cannot be
        // cleaned this way; the next wait's `prev.stop()` covers that one.
        let _ = conn.eval_within(STOP_WATCH_JS, 500);
    }
    out
}

fn wait_page_settled_inner(
    conn: &mut cdp::Conn,
    until_text: Option<&str>,
    until_selector: Option<&str>,
    started: std::time::Instant,
    deadline: std::time::Instant,
    max_idle_ms: u64,
) -> Result<Value, String> {
    const NAME: &str = "__cdpMCP";
    // A navigation that happened before this wait says nothing about it.
    conn.contexts_cleared = false;
    conn.enable_runtime()?;

    let mut settled = Settled::default();
    // One direct look at each decision point, reusing the installer: on a hit it
    // names the element and installs nothing, otherwise it replaces the watcher
    // (which the exit below stops). A wake can be missed when a deferred check
    // was the last one, or when a selector matches through a property such as
    // `checked` or `disabled`, which fires no mutation at all.
    // What a boundary probe saw. `NoBinding` is separate from a miss: the page
    // cannot wake us at all, so the wait has to reinstall instead of idling.
    enum Look {
        Hit(String, Value),
        Miss,
        NoBinding,
    }
    // Probe only: `watch_js(.., false)` checks without stopping the watcher or
    // installing a new one, so the buffered window survives the quiet check.
    let last_look = |conn: &mut cdp::Conn| -> Look {
        if until_text.is_none() && until_selector.is_none() {
            return Look::Miss;
        }
        let v = match conn.eval_within(&watch_js(NAME, until_text, until_selector, false), 2_000) {
            Ok(v) => v,
            Err(_) => return Look::Miss,
        };
        match v.get("r").and_then(Value::as_str) {
            Some("nobinding") => Look::NoBinding,
            Some(kind @ ("hit:text" | "hit:selector")) => Look::Hit(
                kind[4..].to_string(),
                v.get("snap").cloned().unwrap_or(Value::Null),
            ),
            _ => Look::Miss,
        }
    };
    let mut installed = false;
    let mut first_install = true;
    let mut failing_since: Option<std::time::Instant> = None;
    let mut last_event = started;

    // The watcher must not outlive the wait: a live observer keeps pushing into
    // a socket nobody is reading, and the next tool on this tab pays for it.
    let stop_watch = |conn: &mut cdp::Conn| {
        if conn.dialog.is_some() {
            // Blocked: the observer can run nothing, and the next wait
            // replaces it. Waiting 2 s for the stop would be the hang.
            return;
        }
        let _ = conn.eval_within(STOP_WATCH_JS, 2_000);
    };

    loop {
        check_cancel()?;

        // A dialog blocks the renderer: nothing can settle, and every page
        // read from here would wait out its own timeout.
        if let Some(d) = conn.dialog.clone() {
            return Ok(settled.dialog_report(d, &started, &last_event));
        }

        // Install on the first pass, and again after a document swap: the
        // observer died with the old document, the binding did not.
        if !installed || conn.contexts_cleared {
            if conn.contexts_cleared {
                conn.contexts_cleared = false;
                settled.navigated = true;
                settled.reset_document();
                last_event = std::time::Instant::now();
            }
            // Cached, so this is free unless the page lost it.
            conn.add_binding(NAME)?;
            match conn.eval(&watch_js(NAME, until_text, until_selector, true)) {
                Ok(v) => match v.get("r").and_then(Value::as_str) {
                    Some("installed") => {
                        installed = true;
                        failing_since = None;
                        // A recovery starts a new quiet clock once the watcher
                        // is armed. The first arm keeps the clock from the
                        // wait's start, or a few milliseconds of install time
                        // would stop `maxIdleMs == timeoutMs` ever being met.
                        if first_install {
                            first_install = false;
                        } else {
                            last_event = std::time::Instant::now();
                        }
                        // The size at install is the baseline for textDelta and
                        // for the last-look before calling the page quiet.
                        if let Some(c) = v.get("chars").and_then(Value::as_i64) {
                            settled.chars = c;
                            if settled.first_chars.is_none() {
                                settled.first_chars = Some(c);
                            }
                        }
                        settled.shadow_chars = v.get("shadow").and_then(Value::as_i64).unwrap_or(0);
                    }
                    Some(kind @ ("hit:text" | "hit:selector")) => {
                        let kind = kind.to_string();
                        if let Some(snap) = v.get("snap") {
                            settled.absorb(snap);
                            // It was already there when the wait began; that is
                            // not a change that happened during it.
                            settled.changes = 0;
                        }
                        return Ok(settled.report(&kind[4..], &started, &last_event));
                    }
                    Some("nobinding") => {
                        // The connection believes it installed one; the page says
                        // otherwise. Re-add on the next pass.
                        conn.binding = None;
                        installed = false;
                        let since = *failing_since.get_or_insert_with(std::time::Instant::now);
                        if since.elapsed().as_millis() > 5_000 {
                            return Err("the page would not accept a wake-up binding".into());
                        }
                        nap(20)?;
                        continue;
                    }
                    _ => {
                        // `not-ready`: the new document has not parsed yet.
                        installed = false;
                        let since = *failing_since.get_or_insert_with(std::time::Instant::now);
                        if since.elapsed().as_millis() > 5_000 {
                            return Err("the page never became ready to watch".into());
                        }
                        nap(20)?;
                        continue;
                    }
                },
                Err(e) if conn.dead => return Err(e),
                Err(_) if conn.dialog.is_some() => {
                    // "Not ready" here would nap and retry into the same blocked
                    // renderer, once per pass, for the whole timeout.
                    let d = conn.dialog.clone().unwrap_or(Value::Null);
                    return Ok(settled.dialog_report(d, &started, &last_event));
                }
                Err(_) => {
                    installed = false;
                    let since = *failing_since.get_or_insert_with(std::time::Instant::now);
                    if since.elapsed().as_millis() > 5_000 {
                        return Err("the page never became ready to watch".into());
                    }
                    nap(20)?;
                    continue;
                }
            }
        }

        let now = std::time::Instant::now();
        let quiet = last_event.elapsed().as_millis() as u64;
        // The quiet check comes first on purpose: `maxIdleMs` defaults to
        // `timeoutMs`, so on the last pass both are true, and "the page was
        // quiet" is the truthful answer. The deadline used to win, which made
        // every quiet default wait report `woke: "timeout"` and claim the page
        // never matched anything.
        if quiet >= max_idle_ms {
            // One direct look before calling it quiet: a deferred check whose
            // page then went silent, or a selector matched through a property
            // change, would otherwise be reported as a still page.
            match last_look(conn) {
                Look::Hit(kind, snap) => {
                    if snap.is_object() {
                        settled.absorb(&snap);
                    }
                    stop_watch(conn);
                    return Ok(settled.report(&kind, &started, &last_event));
                }
                Look::NoBinding => {
                    // The page cannot wake us any more. Reinstall rather than
                    // run the rest of the wait blind, but not after the
                    // deadline: that would spend a round trip on a wait that is
                    // already over. The quiet clock restarts when the watcher is
                    // actually armed again, not here, so a slow reinstall is not
                    // counted as quiet time.
                    if now < deadline {
                        conn.binding = None;
                        installed = false;
                        continue;
                    }
                }
                Look::Miss => {}
            }
            // The look is where a dialog can appear: do not walk shadow roots
            // or call the page idle with a blocked renderer.
            if let Some(d) = conn.dialog.clone() {
                return Ok(settled.dialog_report(d, &started, &last_event));
            }
            // Last look before calling it quiet: a shadow root or an attribute
            // toggle can move the visible text without a light-DOM record, and
            // reporting idle mid-render is worse than one extra read.
            // `innerText` never reaches a shadow root, so a component painting
            // on its own would look like a still page. Walk them too, once, at
            // the decision point only.
            if let Some(a) = conn.eval_within(VISIBLE_JS, 1_500).ok().filter(|v| v.is_array()) {
                let len = a.get(0).and_then(Value::as_i64).unwrap_or(settled.chars);
                let shadow = a.get(1).and_then(Value::as_i64).unwrap_or(settled.shadow_chars);
                if (len > 0 && len != settled.chars) || shadow != settled.shadow_chars {
                    settled.chars = len;
                    settled.shadow_chars = shadow;
                    if settled.first_chars.is_none() {
                        settled.first_chars = Some(len);
                    }
                    last_event = std::time::Instant::now();
                    continue;
                }
            }
            // A dialog opened around the look: report it instead of
            // calling the page idle or timed out.
            if let Some(d) = conn.dialog.clone() {
                return Ok(settled.dialog_report(d, &started, &last_event));
            }

            stop_watch(conn);
            let mut o = settled.report("idleLimit", &started, &last_event);
            o["note"] = json!("nothing changed for maxIdleMs");
            return Ok(o);
        }
        if now >= deadline {
            // The same look at the deadline: a missed wake must not be
            // reported as "the page never matched".
            if let Look::Hit(kind, snap) = last_look(conn) {
                if snap.is_object() {
                    settled.absorb(&snap);
                }
                stop_watch(conn);
                return Ok(settled.report(&kind, &started, &last_event));
            }
            // A dialog opened around the look: report it instead of
            // calling the page idle or timed out.
            if let Some(d) = conn.dialog.clone() {
                return Ok(settled.dialog_report(d, &started, &last_event));
            }

            stop_watch(conn);
            let mut o = settled.report("timeout", &started, &last_event);
            // Cold path only: one bounded look at the near miss, so the next
            // turn can correct the selector instead of rescanning the page.
            if until_selector.is_some() || until_text.is_some() {
                if let Ok(near) = conn.eval_within(&diagnose_js(until_selector, until_text), 400) {
                    let has = |k: &str| match near.get(k) {
                        Some(Value::Array(a)) => !a.is_empty(),
                        Some(Value::String(s)) => !s.is_empty(),
                        Some(Value::Null) | None => false,
                        Some(_) => true,
                    };
                    if has("selector") || has("selectorMatches") || has("dialogs") || has("textNear") {
                        o["near"] = near;
                    }
                }
            }
            o["note"] = json!("waited the full timeout; the page never matched what was asked for");
            return Ok(o);
        }

        // A local read timeout is the cancellation check and the quiet timer at
        // once, and it sends nothing: 250 ms with no traffic on a still page.
        let slice = std::time::Duration::from_millis(250)
            .min(deadline.saturating_duration_since(now))
            .min(std::time::Duration::from_millis(max_idle_ms.saturating_sub(quiet)))
            .as_millis()
            .max(1) as u64;
        if let Some(payload) = conn.wait_binding(NAME, slice)? {
            let p: Value = match payload {
                Value::String(s) => serde_json::from_str(&s).unwrap_or(Value::Null),
                other => other,
            };
            match p.get("t").and_then(Value::as_str) {
                Some("text") => {
                    settled.absorb(&p);
                    return Ok(settled.report("text", &started, &last_event));
                }
                Some("selector") => {
                    settled.absorb(&p);
                    return Ok(settled.report("selector", &started, &last_event));
                }
                Some("change") => {
                    settled.absorb(&p);
                    last_event = std::time::Instant::now();
                }
                // An unknown payload is still evidence the page moved.
                _ => last_event = std::time::Instant::now(),
            }
        }
    }
}

/// Wait until loading settles: the Performance API's resource count stops
/// moving. Network idle without enabling the Network domain.
pub(crate) fn wait_idle(conn: &mut cdp::Conn, ms: u64, step: u64) -> Result<i64, String> {
    // An open dialog blocks the renderer: the eval below would wait out the full
    // call timeout rather than fail.
    if conn.dialog.is_some() {
        return Err("dialog open: the page is blocked".into());
    }
    let started = std::time::Instant::now();
    let mut last = -1i64;
    let mut stable_ms = 0u64;
    loop {
        let n = conn
            .eval("performance.getEntriesByType('resource').length")
            .ok()
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        // The eval is where a dialog shows up: `.ok()` would read it as zero
        // resources and call the page idle.
        if conn.dialog.is_some() {
            return Err("dialog open: the page is blocked".into());
        }
        if n == last {
            stable_ms += step;
        } else {
            stable_ms = 0;
            last = n;
        }
        if stable_ms >= 600 || started.elapsed().as_millis() >= ms as u128 {
            return Ok(last);
        }
        nap(step)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_page_that_fills_in_reports_its_delta() {
        let mut s = Settled::default();
        s.absorb(&json!({ "chars": 0 }));
        s.absorb(&json!({ "chars": 42 }));
        let now = std::time::Instant::now();
        let r = s.report("text", &now, &now);
        assert_eq!(r["textChars"], 42);
        assert_eq!(r["textDelta"], 42);
    }

    #[test]
    fn a_page_that_clears_reports_a_negative_delta() {
        let mut s = Settled::default();
        s.absorb(&json!({ "chars": 10 }));
        s.absorb(&json!({ "chars": 0 }));
        let now = std::time::Instant::now();
        let r = s.report("change", &now, &now);
        assert_eq!(r["textDelta"], -10);
        assert!(r.get("textChars").is_none(), "an empty page has no char count");
    }
}
