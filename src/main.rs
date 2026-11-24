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
mod cli;
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

/// Where screenshots, pdfs and managed downloads live.
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

    // One preparation for a connection, reused or fresh. Whatever already
    // arrived is read first: a dialog opened between calls must be seen before
    // this call decides anything, and an idle-dead socket fails here, where the
    // retry below handles it.
    let mut attempt = |conn: &mut cdp::Conn, tab_id: &str| -> Result<R, String> {
        conn.drain_arrived()?;
        conn.enable_page()?;
        conn.cleanup_domains()?;
        // A document arrived since the last call: every cached scan index and
        // the `within` scope that produced it point into the page that left.
        // `tabs goto` cleared these; a click that navigated did not.
        if std::mem::take(&mut conn.doc_navigated) {
            scan_cache().lock().unwrap().remove(tab_id);
            scope_cache().lock().unwrap().remove(tab_id);
        }
        if !allow_dialog {
            if let Some(d) = conn.dialog.clone() {
                return Err(dialog_block(&d));
            }
        }
        if cdp::is_owned(tab_id) {
            // A tab we opened lives in the background so the human keeps their
            // keyboard; it still has to render for screenshots and touch. Free
            // after the first call: the flag lives in the Conn. Set after the
            // dialog check, because a blocked renderer would sit on it.
            let _ = conn.set_focus_emulation(true);
        }
        f(conn, tab_id)
    };

    if let Some(k) = cached_key {
        pinned = Some(k.clone());
        let mut cache = conn_cache().lock().unwrap();
        if let Some(conn) = cache.get_mut(&k) {
            let before = conn.replies;
            match attempt(conn, &k) {
                Ok(v) => {
                    // A handler can answer honestly — a beforeunload that closed
                    // the tab, a note about a lost socket — and still leave a
                    // corpse cached, which every later call would pay for.
                    if conn.dead {
                        cache.remove(&k);
                        let mut last = last_tab().lock().unwrap();
                        if last.as_deref() == Some(k.as_str()) {
                            *last = None;
                        }
                    } else {
                        *last_tab().lock().unwrap() = Some(k);
                    }
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
    let out = attempt(&mut conn, &id);
    if conn.dead {
        let mut last = last_tab().lock().unwrap();
        if last.as_deref() == Some(id.as_str()) {
            *last = None;
        }
    } else {
        *last_tab().lock().unwrap() = Some(id.clone());
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

/// The longest a single call may park the tool. Every waiting entry point
/// (`idle`'s timeout, an input action's `wait`/`sleep`) refuses more, so a
/// missing argument cannot turn into an hours-long call.
pub(crate) const MAX_WAIT_MS: u64 = 900_000;

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

/// 2026-07-28 makes `server/discover` mandatory and drops the initialize
/// handshake. The advertised versions are what this build actually answers:
/// the modern revision, and the handshake revisions its clients still use.
/// `ttlMs`/`cacheScope` are required on this result, hence their presence.
/// Short handles for tabs: a call can say `t2` instead of repeating a 32-character
/// target hash. Anthropic's tool guidance measured that resolving opaque ids to a
/// 0-indexed scheme cuts mistakes, so both forms are accepted — the handle is a
/// convenience, never a replacement for the id.
static TAB_HANDLES: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

fn register_tab(id: &str) -> String {
    let mut map = TAB_HANDLES.lock().unwrap();
    if let Some((handle, _)) = map.iter().find(|(_, known)| known == id) {
        return handle.clone();
    }
    let handle = format!("t{}", map.len() + 1);
    map.push((handle.clone(), id.to_string()));
    handle
}

fn forget_tab(id: &str) {
    if let Ok(mut map) = TAB_HANDLES.lock() {
        map.retain(|(_, known)| known != id);
    }
}

fn handle_to_id(handle: &str) -> Option<String> {
    TAB_HANDLES
        .lock()
        .ok()?
        .iter()
        .find(|(h, _)| h == handle)
        .map(|(_, id)| id.clone())
}

/// Rewrite `arguments.tab` from a handle to the target id, once, before any
/// handler sees it. An unknown handle is an error rather than a pass-through:
/// sending `t9` on as an id produces a confusing CDP failure instead of saying
/// what went wrong.
pub(crate) fn normalize_tab(mut args: Value) -> Result<Value, String> {
    let Some(raw) = args.get("tab").and_then(Value::as_str).map(str::to_string) else {
        return Ok(args);
    };
    let Some(rest) = raw.strip_prefix('t') else {
        return Ok(args);
    };
    if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit()) {
        return Ok(args);
    }
    match handle_to_id(&raw) {
        Some(id) => {
            args["tab"] = json!(id);
            Ok(args)
        }
        None => Err(format!("unknown tab handle {raw}; run status to see the handles")),
    }
}

fn discover_result() -> Value {    json!({
        "supportedVersions": ["2026-07-28", "2025-11-25", "2025-06-18"],
        "capabilities": { "tools": {} },
        "resultType": "complete",
        "ttlMs": 600_000,
        "cacheScope": "public",
        "instructions": "Drives the person's own Chromium browser over DevTools: list, read and act on real pages. Tabs open in the background unless consent allows foreground.",
    })
}

/// The first line of a Chromium's DevToolsActivePort file is the port it chose;
/// the launcher publishes the same one-line shape, so both parse the same way.
fn devtools_port(text: &str) -> Option<u16> {
    text.lines().next()?.trim().parse().ok()
}

/// Where the browser really is when the default endpoint has nothing. Two
/// sources, cheapest first: the port `helium-cdp` published for this session,
/// then every Chromium profile that wrote DevToolsActivePort (a browser started
/// with --remote-debugging-port=0 does; T3 Code's own endpoint shows up here).
/// Only an endpoint that answers /json/version is returned.
fn discover_endpoint() -> Option<(String, String)> {
    let mut candidates: Vec<(String, String)> = Vec::new();
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        if let Ok(text) = std::fs::read_to_string(format!("{dir}/helium-cdp.port")) {
            if let Some(port) = devtools_port(&text) {
                candidates.push((format!("http://127.0.0.1:{port}"), "helium-cdp".to_string()));
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        for base in [".config", ".local/share"] {
            let Ok(entries) = std::fs::read_dir(std::path::Path::new(&home).join(base)) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(text) = std::fs::read_to_string(entry.path().join("DevToolsActivePort")) else {
                    continue;
                };
                let Some(port) = devtools_port(&text) else { continue };
                let url = format!("http://127.0.0.1:{port}");
                if !candidates.iter().any(|(known, _)| *known == url) {
                    candidates.push((url, entry.file_name().to_string_lossy().to_string()));
                }
            }
        }
    }
    candidates
        .into_iter()
        .find(|(url, _)| cdp::http(url, "/json/version", "GET").is_ok())
}

/// Consent lives in one small file a person can read and edit, beside the
/// profiles. Starting a browser is a visible action on someone's desktop, so it
/// is opt-in and asked once, in a terminal, where a person is there to answer.
fn consent_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("cdp-mcp").join("consent.json")
}

/// Absent file, unreadable file, or "allowed": false all mean the same thing:
/// no browser may be started. It fails closed because launching is the one thing
/// here a person can see happen without having asked for it.
fn launch_allowed(text: &str) -> Option<String> {
    let v: Value = serde_json::from_str(text).ok()?;
    if v.get("launch")?.get("allowed")?.as_bool()? {
        Some(v["launch"]["browser"].as_str().unwrap_or("helium-browser").to_string())
    } else {
        None
    }
}

/// Only an explicit `tabs.foreground: true` lets a call take the keyboard.
/// Absent file, malformed file, or a false answer all mean no: taking focus away
/// from someone mid-sentence is the failure that no token budget makes up for.
fn allows_foreground(text: &str) -> bool {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v.get("tabs")?.get("foreground")?.as_bool())
        .unwrap_or(false)
}

fn consent_allows_foreground() -> bool {
    std::fs::read_to_string(consent_path())
        .map(|t| allows_foreground(&t))
        .unwrap_or(false)
}

fn on_path(cmd: &str) -> bool {    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(cmd).is_file()))
        .unwrap_or(false)
}

fn ask(prompt: &str, default_yes: bool) -> bool {
    // No Enter-to-accept: an empty line is not an answer, it is a question that
    // was not read. Only y/yes/n/no decide; EOF (a piped, empty stdin) takes the
    // default so a scripted run cannot hang.
    loop {
        println!("{prompt} [y/n]");
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => return default_yes, // nobody there to answer
            Ok(_) => {}
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return true,
            "n" | "no" => return false,
            "" => println!("  nothing typed: answer y or n"),
            other => println!("  {other:?} is not an answer: type y or n"),
        }
    }
}

fn setup() {
    const KNOWN: [&str; 6] = [
        "helium-browser", "chromium", "google-chrome-stable", "google-chrome",
        "brave", "microsoft-edge-stable",
    ];
    let found: Vec<&str> = KNOWN.iter().copied().filter(|c| on_path(c)).collect();
    println!("cdp-mcp setup — what this machine may do on its own\n");
    println!(
        "Browsers on PATH: {}",
        if found.is_empty() { "none".to_string() } else { found.join(", ") }
    );
    let launch = ask(
        "\nMay cdp-mcp start a browser when none is publishing a DevTools endpoint?",
        false,
    );
    let browser = if launch && !found.is_empty() {
        println!("\nWhich one?");
        for (n, b) in found.iter().enumerate() {
            println!("  {}. {b}", n + 1);
        }
        println!("type a number from 1 to {} ({} is first)", found.len(), found[0]);
        loop {
            let mut line = String::new();
            match std::io::stdin().read_line(&mut line) {
                Ok(0) | Err(_) => break found[0].to_string(),
                Ok(_) => {}
            }
            let typed = line.trim();
            if typed.is_empty() {
                println!("  nothing typed: type a number from 1 to {}", found.len());
                continue;
            }
            match typed.parse::<usize>() {
                Ok(n) if n >= 1 && n <= found.len() => break found[n - 1].to_string(),
                _ => println!("  {typed:?} is not one of 1 to {}", found.len()),
            }
        }
    } else {
        "helium-browser".to_string()
    };
    // The tab policy is not a question: one tab, navigated in place, opened in the
    // background only when a task truly needs a second page. Asking a person to
    // decide that invites a worse answer than the rule.
    let body = json!({
        "launch": { "allowed": launch, "browser": browser },
        "tabs": { "background": true, "foreground": false },
        "written": "one tab, navigated in place; edit tabs.foreground only if you mean it",
    });
    let path = consent_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = serde_json::to_string_pretty(&body).unwrap_or_default();
    match std::fs::write(&path, text + "\n") {
        Ok(()) => {
            println!("\nwrote {}", path.display());
            println!(
                "launch: {}   tabs: one, background, never focused",
                if launch { format!("allowed ({browser})") } else { "not allowed".to_string() }
            );
        }
        Err(e) => {
            eprintln!("cannot write {}: {e}", path.display());
            std::process::exit(1);
        }
    }
}

fn help() -> String {
    format!(
        "cdp-mcp {VERSION}\n\
         A tiny MCP server that drives your own Chromium browser over DevTools.\n\n\
         USAGE:\n  cdp-mcp [--cdp URL] [--profile NAME] [--allow-remote]\n  cdp-mcp setup\n\n\
         OPTIONS:\n\
         \x20 --cdp URL        DevTools endpoint (default {DEFAULT_ENDPOINT})\n\
         \x20 --profile NAME   settings file to read (default \"default\")\n\
         \x20 --allow-remote   permit a non-loopback endpoint (experimental builds only; CDP has no auth)\n\
         \x20 -h, --help       print this\n\
         \x20 -V, --version    print the version\n\n\
         The browser must be started with --remote-debugging-port. Use 9222 for a\n\
         fixed port, or 0 to let it choose: the port is then read from\n\
         <user-data-dir>/DevToolsActivePort, and found automatically when no\n\
         --cdp endpoint is given.\n"
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

/// The endpoint's host, with the port and any userinfo removed. A bracketed
/// IPv6 literal is unwrapped: `[::1]:9222` is host `::1`.
fn endpoint_host(endpoint: &str) -> &str {
    let authority = endpoint
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    match authority.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    }
}

fn is_loopback(endpoint: &str) -> bool {
    // `rsplit_once(':')` read a portless `http://127.0.0.1` as no host at all and
    // split `[::1]` inside the brackets; both were refused as non-loopback.
    matches!(endpoint_host(endpoint), "127.0.0.1" | "localhost" | "::1")
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
            // model turn. This is the token lever — not the transport — and the
            // repeat-until loop is what heavy work needs (click "more" until the
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
            if between > MAX_WAIT_MS {
                // `max` iterations times an unbounded delay is an hours-long call.
                return Err(format!("flow: repeat.delayMs must be at most {MAX_WAIT_MS}"));
            }
            let stop_on_error = args.get("stopOnError").and_then(Value::as_bool).unwrap_or(true);

            // Only the tail is ever returned; keeping every iteration held up to
            // 200 full tool results (600-element scans included) in RAM.
            let mut trail: Vec<Value> = Vec::new();
            let started = std::time::Instant::now();
            let mut hit = false;
            let mut done = 0u64;
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
                                trail.push(json!({ "iteration": iteration, "log": iteration_log }));
                                if trail.len() > 3 { trail.remove(0); }
                                return Ok(json!({
                                    "iterations": iteration,
                                    "elapsedMs": started.elapsed().as_millis() as u64,
                                    "stoppedOnDialog": true,
                                    "dialog": d,
                                    "trail": trail,
                                }));
                            }
                        }
                        Err(e) => {
                            iteration_log.push(json!({ "tool": name, "ok": false, "error": e }));
                            if stop_on_error {
                                trail.push(json!({ "iteration": iteration, "log": iteration_log }));
                                if trail.len() > 3 { trail.remove(0); }
                                return Ok(json!({ "iterations": iteration, "elapsedMs": started.elapsed().as_millis() as u64,
                                                  "stoppedOnError": true, "trail": trail }));
                            }
                        }
                    }
                }
                done = iteration;
                trail.push(json!({ "iteration": iteration, "log": iteration_log }));
                if trail.len() > 3 {
                    trail.remove(0);
                }

                // Did we reach the state we were waiting for?
                if until_text.is_some() || until_gone.is_some() {
                    // Asked in the page: only booleans cross the socket.
                    // A dialog is not "the text is absent": it blocks every later
                    // step, so the flow stops here instead of sleeping and asking
                    // a renderer that cannot answer.
                    let mut stopped: Option<Value> = None;
                    let mut failure: Option<String> = None;
                    let mut page_has = |want: &str| -> Option<bool> {
                        match with_conn(&cfg.endpoint, tab, |conn, _| conn.eval(&has_text_js(want))) {
                            Ok(v) => Some(v.as_bool().unwrap_or(false)),
                            Err(e) => {
                                // The refusal for a blocked page is the dialog
                                // itself; anything else is a real failure.
                                match serde_json::from_str::<Value>(&e) {
                                    Ok(b) if b.get("dialog").is_some() => {
                                        stopped = b.get("dialog").cloned();
                                    }
                                    _ => failure = Some(e),
                                }
                                None
                            }
                        }
                    };
                    if let Some(want) = &until_text {
                        if page_has(want) == Some(true) { hit = true; }
                    }
                    // Asking for the text that just appeared would be a second
                    // round trip for an answer already known.
                    if !hit {
                        if let Some(gone) = &until_gone {
                            if page_has(gone) == Some(false) { hit = true; }
                        }
                    }
                    if let Some(e) = failure {
                        // A closed tab or a dead browser must end the loop, not
                        // be retried `max` times and reported as "not met".
                        return Err(format!("flow: the stop condition could not be checked: {e}"));
                    }
                    if let Some(d) = stopped {
                        return Ok(json!({
                            "iterations": iteration,
                            "elapsedMs": started.elapsed().as_millis() as u64,
                            "stoppedOnDialog": true,
                            "dialog": d,
                            "trail": trail,
                        }));
                    }
                    if hit { break; }
                }
                if iteration < max_iter {
                    nap(between)?;
                }
            }
            Ok(json!({
                "iterations": done,
                "conditionMet": if until_text.is_some() || until_gone.is_some() { json!(hit) } else { Value::Null },
                "elapsedMs": started.elapsed().as_millis() as u64,
                "trail": trail,
            }))
        }
        "idle" => {
            // Two different limits, and conflating them was a bug: how long to
            // wait at all (`timeoutMs`), and how quiet the page must be before
            // describing it as idle (`maxIdleMs`). A page that keeps ticking
            // must not turn a 10-minute wait into an instant one.
            let timeout = args.get("timeoutMs").and_then(Value::as_u64).unwrap_or(600_000);
            if timeout > MAX_WAIT_MS {
                return Err(format!("idle: timeoutMs must be at most {MAX_WAIT_MS}"));
            }
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
                    out["tabs"] = json!(pages.iter().map(|t| json!({ "tab": t.id, "handle": register_tab(&t.id), "title": t.title, "url": t.url, "owned": cdp::is_owned(&t.id) })).collect::<Vec<_>>());
                    // Opt-in, because it costs one evaluation per tab: "a window
                    // is in front and a page has input focus" is the only portable
                    // signal. There is none for "the human is typing right now",
                    // so this is a proxy and is named as one.
                    if args.get("inUse").and_then(Value::as_bool) == Some(true) {
                        let expr = "({visible: document.visibilityState, focused: document.hasFocus()})";
                        let mut in_use = false;
                        let mut front = Value::Null;
                        for t in &pages {
                            // A tab this server opened reports focus right after
                            // creation even though the window title, and the e2e
                            // focus check, say the human's tab never moved. So only
                            // pages we did not open count as evidence of use.
                            if cdp::is_owned(&t.id) {
                                continue;
                            }
                            if let Ok(v) = with_conn(&cfg.endpoint, Some(&t.id), |conn, _| conn.eval(expr)) {
                                if v.get("focused").and_then(Value::as_bool) == Some(true) {
                                    in_use = true;
                                    front = json!({ "tab": t.id, "title": t.title });
                                    break;
                                }
                            }
                        }
                        out["inUse"] = json!(in_use);
                        out["inUseSignal"] = json!("a page this server did not open reports focus; not proof of human input");
                        if in_use {
                            out["inFront"] = front;
                        }
                    }
                    if tabs.is_empty() {
                        out["hint"] = json!("the browser is running but has no page tabs — open one with tabs {action:\"open\"}");
                    }
                    Ok(out)
                }
                Err(e) => {
                    // What an agent can actually do about it: the consent file
                    // decides whether a browser may be started here, and the hint
                    // names the command. "Nothing works" is not actionable.
                    let consent = std::fs::read_to_string(consent_path()).unwrap_or_default();
                    let launch = launch_allowed(&consent);
                    let hint = match &launch {
                        Some(browser) => format!(
                            "no browser on {}. Starting one is allowed ({browser}): run `helium-cdp`, or `{browser} --remote-debugging-port`, then retry.",
                            cfg.endpoint
                        ),
                        None => "no browser on this endpoint, and starting one is not permitted. Run `cdp-mcp setup` to allow it, or start a browser with --remote-debugging-port.".to_string(),
                    };
                    Ok(json!({
                        "endpoint": cfg.endpoint,
                        "up": false,
                        "error": e,
                        "launchAllowed": launch.is_some(),
                        "canLaunch": launch.unwrap_or_default(),
                        "hint": hint,
                    }))
                }
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
                    .map(|t| json!({ "tab": t.id, "handle": register_tab(&t.id), "title": t.title, "url": t.url, "owned": cdp::is_owned(&t.id) }))
                    .collect::<Vec<_>>()))
            }
            "open" => {
                let url = args.get("url").and_then(Value::as_str).ok_or("open needs url")?;
                // Background by default: the human may be typing to me in the
                // chat, and activating the tab hands their keyboard to the
                // browser. `foreground: true` when the tab itself is the point,
                // and only if the person allowed that at setup — research work
                // wants a tab the user never notices, not one that steals focus.
                let fg = args.get("foreground").and_then(Value::as_bool).unwrap_or(false);
                if fg && !consent_allows_foreground() {
                    return Err("bringing a tab to the front is not permitted \
                                (consent.json says tabs.foreground=false). Open it in the \
                                background, or run `cdp-mcp setup` if the user wants that."
                        .into());
                }
                let id = cdp::new_tab(&cfg.endpoint, url, fg)?;
                // A fresh tab is about:blank and a blank document already reports
                // readyState "complete", so a real navigation is waited for by
                // requiring the URL to have left it — unless about:blank is what
                // was asked for, where the blank document is the answer.
                let blank_wanted = url == "about:blank";
                let mut ready = String::from("unknown");
                let mut dialog = Value::Null;
                let mut note: Option<String> = None;
                let waited = with_conn(&cfg.endpoint, Some(&id), |conn, _| {
                    let started = std::time::Instant::now();
                    while started.elapsed().as_millis() < 16_000 {
                        // An evaluation during navigation can fail (the old
                        // context is gone); that is "not ready yet", not an error.
                        if let Ok(v) = conn.eval("[document.readyState, location.href]") {
                            ready = v.get(0).and_then(Value::as_str).unwrap_or("unknown").to_string();
                            let href = v.get(1).and_then(Value::as_str).unwrap_or_default();
                            if (ready == "complete" || ready == "interactive")
                                && (blank_wanted || (!href.is_empty() && href != "about:blank"))
                            {
                                break;
                            }
                        } else if conn.dead {
                            break;
                        }
                        if conn.dialog.is_some() {
                            break;
                        }
                        nap(20)?; // a fresh tab is about:blank for a few ms
                    }
                    // A dialog opened during the load blocks the renderer: the
                    // caller has to be told, and it is the reason there is no url.
                    dialog = conn.dialog.clone().unwrap_or(Value::Null);
                    Ok(())
                });
                if waited.is_err() {
                    // A client stop is not an outcome to report as success. Any
                    // other failure leaves the tab open with an unknown document,
                    // which the note says.
                    check_cancel()?;
                    if let Err(e) = waited {
                        note = Some(format!("the tab opened, but its document could not be read: {e}"));
                    }
                }
                // SPAs reach "interactive" before they paint, so a caller that
                // needs content should ask to settle: wait until the resource
                // count stops moving instead of trusting readyState alone.
                let idle = if args.get("idle").and_then(Value::as_bool).unwrap_or(false) {
                    match with_conn(&cfg.endpoint, Some(&id), |conn, _| wait_idle(conn, 20_000, 300)) {
                        Ok(n) => Some(n),
                        Err(e) => {
                            check_cancel()?;
                            note.get_or_insert(format!("the idle wait ended early: {e}"));
                            None
                        }
                    }
                } else {
                    None
                };
                let final_url = with_conn(&cfg.endpoint, Some(&id), |conn, _| conn.eval("location.href"))
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default();
                if dialog.is_null() && final_url.starts_with("chrome-error://") {
                    // The browser committed its own error page: the open failed,
                    // and reporting a tab and a url would hide that.
                    return Err(format!(
                        "the tab opened but the navigation failed: the browser showed its own error page ({final_url})"
                    ));
                }
                let mut out = json!({ "tab": id, "url": final_url, "readyState": ready });
                if let Some(n) = idle {
                    out["idle"] = json!(true);
                    out["resources"] = json!(n);
                }
                if !dialog.is_null() {
                    out["dialog"] = dialog;
                    out["note"] = json!("the tab opened; a load-time dialog is blocking the page, so its url and ready state are not known yet");
                } else if let Some(n) = note {
                    out["note"] = json!(n);
                }
                Ok(out)
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
                    // A handle must not outlive its tab: reusing the number for a
                    // later tab would silently point at a different page.
                    forget_tab(&id);
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
            return Err("invalid base64 in a binary payload".into());
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
    /// The client closed stdin. Anything still queued is for a reader that is
    /// gone, so it must not start: a queued ten-minute wait would run to the end
    /// for nobody.
    eof: bool,
}

/// One JSON-RPC message per line. Both threads write, so the line is written
/// under the stdout lock and never interleaves with another.
fn send(v: Value) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

fn main() {
    // `cdp <verb>`: the CLI path, decided before flag parsing so a verb is not
    // mistaken for an unknown argument.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let cli_mode = argv.first().map(|a| cli::is_verb(a)).unwrap_or(false);
    if argv.first().map(String::as_str) == Some("setup") {
        setup();
        return;
    }
    let mut endpoint = DEFAULT_ENDPOINT.to_string();
    let mut endpoint_set = false;
    let mut profile_name = std::env::var("CDP_PROFILE").unwrap_or_else(|_| "default".to_string());
    let mut allow_remote = false;
    let mut args = argv.clone().into_iter().skip(if cli_mode { usize::MAX } else { 0 });
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
            "--cdp" => {
                endpoint = args.next().unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
                endpoint_set = true;
            }
            "--profile" => profile_name = args.next().unwrap_or_else(|| "default".to_string()),
            "--allow-remote" => allow_remote = true,
            other => {
                eprintln!("cdp-mcp: unknown argument {other}\n{}", help());
                std::process::exit(2);
            }
        }
    }
    // No endpoint was named, so the browser may not be on the default port: a
    // Playwright run can be holding 9222, and a browser started with
    // --remote-debugging-port=0 gets one of its own choosing. Ask the launcher
    // and the browsers' own DevToolsActivePort files before giving up, so an
    // agent is never told "not running" while a browser is up.
    if !endpoint_set && cdp::http(&endpoint, "/json/version", "GET").is_err() {
        if let Some((url, who)) = discover_endpoint() {
            eprintln!("cdp-mcp: {DEFAULT_ENDPOINT} has no browser; using {url} ({who})");
            endpoint = url;
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
    if cli_mode {
        std::process::exit(cli::run(&cfg, &argv));
    }

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
            // a long flow for nobody, and refuse anything still queued.
            CANCELLED.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Ok(mut f) = inflight.lock() {
                f.eof = true;
            }
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
            "server/discover" => send(json!({ "jsonrpc": "2.0", "id": id, "result": discover_result() })),
            "tools/list" => send(json!({ "jsonrpc": "2.0", "id": id, "result": {
                "tools": schema::tool_defs(),
                // Required by 2026-07-28 on list results: a client may cache the
                // catalog. The order is static, so the cache key is stable.
                "resultType": "complete",
                "ttlMs": 60_000,
                "cacheScope": "public",
            }})),
            "tools/call" => {
                let rid = id.to_string();
                {
                    let mut f = inflight.lock().unwrap();
                    if f.eof {
                        break; // the client is gone; queued work is for nobody
                    }
                    if f.cancelled.remove(&rid) {
                        continue; // cancelled before it started: never run it
                    }
                    f.current = Some(rid);
                    CANCELLED.store(false, std::sync::atomic::Ordering::Relaxed);
                }
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let result = match normalize_tab(args) {
                    Ok(args) => call_tool(&cfg, name, &args),
                    Err(e) => Err(e),
                };
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
                        // 2026-07-28 requires resultType on results; older clients
                        // ignore the field, so this is additive for them.
                        send(json!({ "jsonrpc": "2.0", "id": id, "result": {
                            "content": content,
                            "resultType": "complete"
                        }}));
                    }
                    Err(e) => {
                        let content = vec![json!({ "type": "text", "text": format!("ERROR: {e}") })];
                        send(json!({ "jsonrpc": "2.0", "id": id, "result": {
                            "content": content,
                            "isError": true,
                            "resultType": "complete"
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
    fn a_devtools_active_port_file_is_read_from_its_first_line() {
        // What a browser writes with --remote-debugging-port=0.
        assert_eq!(devtools_port("33331\n/devtools/browser/13ed-\n"), Some(33331));
        // What the launcher publishes: the port alone.
        assert_eq!(devtools_port("9222\n"), Some(9222));
        assert_eq!(devtools_port(""), None);
        assert_eq!(devtools_port("not a port\n"), None);
    }

    #[test]
    fn consent_fails_closed() {
        // Granted: the browser to launch.
        assert_eq!(
            launch_allowed(r#"{"launch":{"allowed":true,"browser":"chromium"}}"#),
            Some("chromium".to_string())
        );
        // Refused, missing, malformed, or older shape: no launcher.
        assert_eq!(launch_allowed(r#"{"launch":{"allowed":false}}"#), None);
        assert_eq!(launch_allowed("{}"), None);
        assert_eq!(launch_allowed("not json"), None);
    }

    #[test]
    fn foreground_needs_an_explicit_yes() {
        // Research work must never take the keyboard: only a literal true allows it.
        assert!(allows_foreground(r#"{"tabs":{"foreground":true}}"#));
        assert!(!allows_foreground(r#"{"tabs":{"foreground":false}}"#));
        assert!(!allows_foreground(r#"{"tabs":{"background":true}}"#));
        assert!(!allows_foreground("{}"));
        assert!(!allows_foreground(""));
    }

    #[test]
    fn a_tab_handle_resolves_and_an_unknown_one_fails() {
        let id = format!("TESTTARGET-{}", std::process::id());
        let handle = register_tab(&id);
        assert!(handle.starts_with('t') && handle.len() <= 5, "handle looks wrong: {handle}");
        assert_eq!(normalize_tab(json!({ "tab": handle })).unwrap()["tab"], id.as_str());
        // A real target id passes through untouched.
        assert_eq!(normalize_tab(json!({ "tab": "ABC123" })).unwrap()["tab"], "ABC123");
        // An unknown handle is an error that names the fix, not a later CDP failure.
        assert!(normalize_tab(json!({ "tab": "t999999" })).is_err());
    }

    #[test]
    fn discover_advertises_what_this_build_answers() {
        let d = discover_result();
        let versions: Vec<&str> = d["supportedVersions"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        // Modern clients must find the stateless revision; legacy ones must still
        // find theirs, because the handshake they use is still answered here.
        assert!(versions.contains(&"2026-07-28"), "missing the modern revision");
        assert!(versions.contains(&"2025-06-18"), "missing the legacy handshake revision");
        assert_eq!(d["resultType"], "complete");
        assert!(d["ttlMs"].is_u64() && d["cacheScope"].is_string());
        assert!(d["capabilities"]["tools"].is_object());
    }

    #[test]
    fn only_loopback_endpoints_are_allowed_by_default() {
        assert!(is_loopback("http://127.0.0.1:9222"));
        assert!(is_loopback("http://localhost:9222"));
        assert!(is_loopback("http://[::1]:9222"));
        assert!(!is_loopback("http://192.168.1.4:9222"));
        assert!(!is_loopback("http://example.com:9222"));
        // A portless loopback endpoint is still loopback, and the bracketed form
        // must not be split inside the brackets.
        assert!(is_loopback("http://127.0.0.1"));
        assert!(is_loopback("http://[::1]"));
        assert!(is_loopback("http://localhost/base"));
        assert!(!is_loopback("http://[::2]:9222"));
        assert!(!is_loopback("http://127.0.0.1.evil.example:9222"));
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
    fn base64_round_trips() {
        for n in 0..10 {
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
