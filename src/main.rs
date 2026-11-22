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
mod emulate;
mod files;
mod input;
mod nav;
mod pdf;
mod scan;
mod schema;
mod scroll;
mod shot;
mod wait;

use wait::*;

use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::io::{self, BufRead, Write};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// An empty or whitespace-only selector is no selector: it must not read as a
/// scope that matched everything, and it must not be passed to querySelector.
pub(crate) fn nonempty_scope(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() { None } else { Some(t.to_string()) }
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

/// Where screenshots and managed downloads live.
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

pub(crate) fn data_dir() -> std::path::PathBuf {
    data_dir_for(current_os(), &|k| std::env::var(k).ok())
}

/// Last scan per tab, so `mode:"delta"` can say what changed instead of
/// re-sending the whole page after every click. Keyed by tab id.
pub(crate) fn scan_cache() -> &'static Mutex<HashMap<String, Value>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Value>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn conn_cache() -> &'static Mutex<HashMap<String, cdp::Conn>> {
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

pub(crate) fn with_conn<R>(
    endpoint: &str,
    tab: Option<&str>,
    f: impl FnMut(&mut cdp::Conn, &str) -> Result<R, String>,
) -> Result<R, String> {
    with_conn_retry(endpoint, tab, Retry::IfUntouched, false, f)
}

/// `with_conn` for the one action that may run while a dialog is open: answering
/// it. Everything else fails fast, so a blocked page is reported, not waited on.
pub(crate) fn with_conn_dialog<R>(
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
pub(crate) fn with_conn_readonly<R>(
    endpoint: &str,
    tab: Option<&str>,
    f: impl FnMut(&mut cdp::Conn, &str) -> Result<R, String>,
) -> Result<R, String> {
    with_conn_retry(endpoint, tab, Retry::Always, false, f)
}

pub(crate) fn with_conn_retry<R>(
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

pub(crate) fn check_cancel() -> Result<(), String> {
    if CANCELLED.load(std::sync::atomic::Ordering::Relaxed) {
        Err("cancelled by the client".into())
    } else {
        Ok(())
    }
}

/// Sleep that a cancellation cuts short.
pub(crate) fn nap(ms: u64) -> Result<(), String> {
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

/// Emulation state per tab. See the "emulate" arm: it is page state, not
/// connection state, so it must not be lost when a socket dies.
fn emu_cache() -> &'static Mutex<HashMap<String, emulate::Emulated>> {
    static EMU: OnceLock<Mutex<HashMap<String, emulate::Emulated>>> = OnceLock::new();
    EMU.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The scope (`within`) of the last `elements` scan per tab. An index is only
/// meaningful in the scan that produced it, so a click must resolve it in the
/// same scope or it lands on a different element.
fn scope_cache() -> &'static Mutex<HashMap<String, Option<String>>> {
    static SCOPES: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
    SCOPES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn scope_for(tab_id: &str) -> Option<String> {
    scope_cache().lock().unwrap().get(tab_id).cloned().flatten()
}

/// Cut a string to at most `max` bytes on a character boundary. Slicing at a
/// byte offset panics inside a multi-byte character, and a panic ends the
/// server (panic = "abort").
pub(crate) fn clip_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
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
pub(crate) struct Profile {
    pub(crate) name: String,
    path: std::path::PathBuf,
    data: Value,
}

impl Cfg {
    /// The settings loaded at startup. Nothing in the server writes the file, so
    /// re-reading it per call would only pick up a hand edit made behind a
    /// running process.
    pub(crate) fn profile_now(&self) -> Profile {
        self.profile.clone()
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
    pub(crate) fn get_str(&self, key: &str) -> Option<String> {
        self.data.get(key).and_then(Value::as_str).map(str::to_string)
    }
    pub(crate) fn get_u64(&self, key: &str) -> Option<u64> {
        self.data.get(key).and_then(Value::as_u64)
    }
    /// Where a screenshot goes when the caller gave no path.
    pub(crate) fn shot_dir(&self) -> std::path::PathBuf {
        self.get_str("screenshot_dir").map(std::path::PathBuf::from)
            .unwrap_or_else(|| data_dir().join("shots"))
    }
}

#[derive(Clone)]
pub(crate) struct Cfg {
    pub(crate) endpoint: String,
    pub(crate) profile: Profile,
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

pub(crate) fn b64encode(bytes: &[u8]) -> String {
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

/// JS: does the page's text contain `want`? Evaluated in the page, so only a
/// boolean crosses the socket instead of the whole body text.
pub(crate) fn has_text_js(want: &str) -> String {
    // `innerText` only: `textContent` includes inline <script> source, so a wait
    // for a word the page's own code mentions matched before it was ever shown.
    format!(
        "(() => {{ const w = {w}; const b = document.body; \
         return !!b && (b.innerText || '').includes(w); }})()",
        w = Value::String(want.to_string())
    )
}

/// A step inherits the caller's tab unless it names one: `flow {tab}` used to
/// leave its steps on whatever tab the last call happened to touch.
pub(crate) fn inherit_tab(mut a: Value, tab: Option<&str>) -> Value {
    if let (Some(t), Some(o)) = (tab, a.as_object_mut()) {
        if !o.contains_key("tab") {
            o.insert("tab".into(), json!(t));
        }
    }
    a
}

pub(crate) fn call_tool(cfg: &Cfg, name: &str, args: &Value) -> Result<Value, String> {
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
                    out["tabs"] = json!(pages.iter().map(|t| json!({ "tab": t.id, "title": t.title, "url": t.url, "owned": cdp::is_owned(&t.id) })).collect::<Vec<_>>());
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
                    .map(|t| json!({ "tab": t.id, "title": t.title, "url": t.url, "owned": cdp::is_owned(&t.id) }))
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
                    emu_cache().lock().unwrap().remove(&id);
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
        "emulate" => with_conn(&cfg.endpoint, tab, |conn, tab_id| {
            // The record is keyed by tab, not held on the connection: the
            // overrides live on the target, so they survive a socket that died,
            // and a merge from an empty record would wipe a feature the page
            // still has. Read, run unlocked, write back.
            //
            // Written back even on failure: `run` advances the record only
            // after the browser acknowledged that step, so after an error it is
            // exactly what was applied. Discarding it would make the next call
            // merge into a state the page no longer has.
            let mut state = emu_cache().lock().unwrap().get(tab_id).cloned().unwrap_or_default();
            let out = emulate::run(conn, tab_id, &args, &mut state);
            emu_cache().lock().unwrap().insert(tab_id.to_string(), state);
            out
        }),
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
        "elements" => scan::elements(cfg, args),
        "text" => scan::text(cfg, args),
        "input" => {
            let actions = args.get("actions").and_then(Value::as_array).ok_or("input needs actions[]")?.clone();
            let announce = args.get("announce").and_then(Value::as_bool).unwrap_or(false);
            let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
              // Managed downloads: opt-in, so a human's own downloads are never
              // redirected by default. `0` from the caller means "default", and with a
              // managed folder that default is a real settle window.
              let save = args.get("saveDownloads").and_then(Value::as_bool).unwrap_or(false);
              let dir = match args.get("downloadDir") {
                  None | Some(Value::Null) => None,
                  Some(Value::String(s)) if !s.trim().is_empty() => Some(std::path::PathBuf::from(s)),
                  Some(_) => return Err("input: downloadDir must be a path".into()),
              };
              let mut managed = dir.or_else(|| save.then(|| data_dir().join("downloads")));
              if let Some(d) = managed.as_mut() {
                  std::fs::create_dir_all(&*d)
                    .map_err(|e| format!("cannot create {}: {e}", d.display()))?;
                  // Chromium resolves the folder on its own side; a relative one would diverge.
                  *d = std::fs::canonicalize(&*d)
                    .map_err(|e| format!("cannot resolve {}: {e}", d.display()))?;
              }
              let settle_arg = args.get("settleMs").and_then(Value::as_u64).unwrap_or(0);
              let settle_ms = if settle_arg == 0 && managed.is_some() { 5_000 } else { settle_arg.min(5_000) };
            // Only a batch that *starts* by answering a dialog may run against a
            // blocked page: anything else would send a click an eval that has to
            // wait out its timeout. A click that opens one is handled inside
            // run_input, where the page was not blocked on entry.
            let answers_dialog = actions.first().and_then(|a| a.get("dialog")).is_some();
            if answers_dialog {
                with_conn_dialog(&cfg.endpoint, tab, |conn, tab_id| {
                    input::run_batch(conn, tab_id, &actions, announce, force, settle_ms, managed.as_deref())
                })
            } else {
                with_conn(&cfg.endpoint, tab, |conn, tab_id| {
                    input::run_batch(conn, tab_id, &actions, announce, force, settle_ms, managed.as_deref())
                })
            }
        }
        "download" => files::download(cfg, args),
        "upload" => files::upload(cfg, args),
        "paste" => files::paste(cfg, args),
        "shot" => shot::run(cfg, args),
        other => Err(format!("unknown tool: {other}")),
    }
}

/* ------------------------------- encoders -------------------------------- */

pub(crate) fn b64decode(s: &str) -> Result<Vec<u8>, String> {
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
            "tools/list" => send(json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": schema::tool_defs() } })),
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
    fn a_non_loopback_endpoint_needs_the_experimental_feature() {
        assert!(remote_allowed(true, false));
        assert!(remote_allowed(true, true));
        assert!(!remote_allowed(false, false));
        assert_eq!(remote_allowed(false, true), cfg!(feature = "experimental"));
    }



    /// The emulation record has to follow the tab, not the socket: a reconnect
    /// drops the Conn, while the browser keeps the override applied.
    #[test]
    fn emulation_state_is_kept_per_tab() {
        let mut st = emulate::Emulated::default();
        st.media.features.push(("prefers-color-scheme".into(), "dark".into()));
        emu_cache().lock().unwrap().insert("tab-a".into(), st);
        let other = emu_cache().lock().unwrap().get("tab-b").cloned().unwrap_or_default();
        assert!(other.media.features.is_empty(), "a new tab starts with nothing");
        let same = emu_cache().lock().unwrap().get("tab-a").cloned().unwrap_or_default();
        assert_eq!(same.media.features, vec![("prefers-color-scheme".to_string(), "dark".to_string())]);
        // Closing a tab drops its record, as it drops the scan and scope caches.
        emu_cache().lock().unwrap().remove("tab-a");
        assert!(emu_cache().lock().unwrap().get("tab-a").is_none());
    }

    #[test]
    fn base64_round_trips() {        for n in 0..10 {
            let bytes: Vec<u8> = (0..n * 7 + 1).map(|b| (b * 37) as u8).collect();
            assert_eq!(b64decode(&b64encode(&bytes)).unwrap(), bytes);
        }
        assert!(b64decode("not*base64").is_err());
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





    
}
