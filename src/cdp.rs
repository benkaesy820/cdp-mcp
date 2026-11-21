//! Transport: a minimal HTTP client for Chrome's `/json/*` endpoints and a
//! websocket connection for the DevTools protocol itself.
//!
//! Deliberately hand-rolled: the HTTP surface we need is three GET/PUT calls
//! with no body, so pulling an HTTP stack in would cost more than it saves.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use serde_json::{json, Value};
use tungstenite::{client::client_with_config, protocol::WebSocketConfig, Message, WebSocket};

pub type Res<T> = Result<T, String>;

/// Causal events seen while an action settles. Bounded and same-origin: a
/// failing tracker on another host is noise, and an aborted fetch is not a
/// failure. The point is one block that says what the click actually caused, so
/// the agent does not spend two more turns in the console and network panels.
struct Receipts {
    origin: String,
    opener: String,
    entries: Vec<Value>,
    requests: std::collections::HashMap<String, String>,
    opened_tab: Option<Value>,
    downloads: Vec<Value>,
}

/// What a settle window saw beyond the error list: a tab the action opened.
#[derive(Default)]
pub struct ReceiptOutcome {
    pub entries: Vec<Value>,
    pub opened_tab: Option<Value>,
    /// Downloads the action started. Reported as started, never as saved: the
    /// browser says the download began, not where it landed.
    pub downloads: Vec<Value>,
}

fn origin_of(url: &str) -> String {
    let url = url.trim();
    // A blob URL belongs to the origin inside it; about:/data: have none.
    let url = url.strip_prefix("blob:").unwrap_or(url);
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (s.to_ascii_lowercase(), r),
        None => return String::new(),
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("").to_ascii_lowercase();
    if authority.is_empty() {
        return format!("{scheme}://");
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => {
            let bracketed = h.starts_with('[') && h.ends_with(']');
            if bracketed || (!h.contains(':') && p.parse::<u16>().is_ok()) {
                (h.to_string(), Some(p.to_string()))
            } else {
                (authority.clone(), None)
            }
        }
        None => (authority.clone(), None),
    };
    let port = match (scheme.as_str(), port.as_deref()) {
        ("http", Some("80")) | ("https", Some("443")) => None,
        (_, p) => p.map(str::to_string),
    };
    match port {
        Some(p) => format!("{scheme}://{host}:{p}"),
        None => format!("{scheme}://{host}"),
    }
}

fn clip_event(s: &str) -> String {
    s.chars().take(160).collect()
}

impl Receipts {
    fn new(origin: String, opener: String) -> Self {
        Self { origin, opener, entries: Vec::new(), requests: Default::default(), opened_tab: None,
               downloads: Vec::new() }
    }

    fn push(&mut self, v: Value) {
        if self.entries.len() >= 5 || self.entries.iter().any(|e| e == &v) {
            return;
        }
        self.entries.push(v);
    }

    fn same_origin(&self, url: &str) -> bool {
        if self.origin.is_empty() {
            return true;
        }
        let other = origin_of(url);
        if other == self.origin {
            return true;
        }
        // Dev stacks put the app and the API on different loopback ports
        // (Vite on 5173, the API on 3000). Treat loopback as one origin, but
        // match the host exactly: localhost.evil.com is not loopback.
        let loopback = |o: &str| {
            let authority = o.split_once("://").map(|(_, h)| h).unwrap_or("");
            let host = if let Some(rest) = authority.strip_prefix('[') {
                rest.split(']').next().map(|h| format!("[{h}]")).unwrap_or_default()
            } else {
                authority.split(':').next().unwrap_or("").to_string()
            };
            matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]")
        };
        loopback(&self.origin) && loopback(&other)
    }

    fn note(&mut self, v: &Value) {
        let method = v.get("method").and_then(Value::as_str).unwrap_or("");
        let p = v.get("params").unwrap_or(&Value::Null);
        match method {
            "Network.requestWillBeSent" => {
                let Some(id) = p.get("requestId").and_then(Value::as_str) else { return };
                let url = p.pointer("/request/url").and_then(Value::as_str).unwrap_or("");
                let ty = p.get("type").and_then(Value::as_str).unwrap_or("");
                // A Document is tracked whatever its origin: a cross-origin
                // navigation is the action's outcome, and a slow one must not let
                // the quiet window close before it lands. XHR and fetch stay
                // filtered to the page's own origin.
                let wanted = ty == "Document" || self.same_origin(url);
                if wanted && matches!(ty, "XHR" | "Fetch" | "Document") {
                    if self.requests.len() > 2_000 {
                        self.requests.clear();
                    }
                    self.requests.insert(id.to_string(), url.to_string());
                } else {
                    // A redirect reuses the id on another origin: the old
                    // entry must not blame it for this leg's failure.
                    self.requests.remove(id);
                }
            }
            "Network.responseReceived" => {
                let status = p.pointer("/response/status").and_then(Value::as_i64).unwrap_or(0);
                let url = p.pointer("/response/url").and_then(Value::as_str).unwrap_or("");
                let ty = p.get("type").and_then(Value::as_str).unwrap_or("");
                // A failed destination document is the navigation's outcome,
                // whatever the origin was when the action started.
                let wanted = ty == "Document" || self.same_origin(url);
                if status >= 400 && wanted && matches!(ty, "XHR" | "Fetch" | "Document") {
                    self.push(json!({ "kind": "http", "status": status, "url": clip_event(url) }));
                }
            }
            "Network.loadingFinished" => {
                if let Some(id) = p.get("requestId").and_then(Value::as_str) {
                    self.requests.remove(id);
                }
            }
            "Network.loadingFailed" => {
                let Some(id) = p.get("requestId").and_then(Value::as_str) else { return };
                let err = p.get("errorText").and_then(Value::as_str).unwrap_or("");
                let canceled = p.get("canceled").and_then(Value::as_bool).unwrap_or(false);
                let url = self.requests.remove(id);
                if canceled || err.contains("ERR_ABORTED") {
                    return;
                }
                if let Some(url) = url {
                    self.push(json!({ "kind": "net", "error": clip_event(err), "url": clip_event(&url) }));
                }
            }
            "Runtime.exceptionThrown" => {
                let d = p.get("exceptionDetails").unwrap_or(&Value::Null);
                let url = d
                    .get("url")
                    .and_then(Value::as_str)
                    .or_else(|| d.pointer("/stackTrace/callFrames/0/url").and_then(Value::as_str))
                    .unwrap_or("");
                if url.starts_with("chrome-extension://") || (!url.is_empty() && !self.same_origin(url)) {
                    return;
                }
                let text = d
                    .pointer("/exception/description")
                    .and_then(Value::as_str)
                    .or_else(|| d.get("text").and_then(Value::as_str))
                    .unwrap_or("uncaught exception");
                self.push(json!({ "kind": "js", "text": clip_event(text) }));
            }
            "Page.frameNavigated" => {
                let frame = p.get("frame").unwrap_or(&Value::Null);
                // Top frame only: a subframe is not where the action landed.
                if frame.get("parentId").map(|v| !v.is_null()).unwrap_or(false) {
                    return;
                }
                let url = frame.get("url").and_then(Value::as_str).unwrap_or("");
                // A failed or blank commit says nothing about where we are;
                // keeping the old origin is what lets the error be reported.
                if url.is_empty()
                    || url.starts_with("chrome-error://")
                    || url.starts_with("about:")
                    || url.starts_with("data:")
                {
                    return;
                }
                let moved = origin_of(url);
                if !moved.is_empty() {
                    self.origin = moved;
                }
            }
            "Page.downloadWillBegin" => {
                // Never origin-filtered: a download routinely redirects to a
                // CDN or a presigned URL on another origin.
                let url = p.get("url").and_then(Value::as_str).unwrap_or("");
                if url.is_empty() || self.downloads.len() >= 5 {
                    return;
                }
                self.downloads.push(json!({
                    "status": "started",
                    "url": clip_event(url),
                    "filename": p.get("suggestedFilename").and_then(Value::as_str).unwrap_or(""),
                    "guid": p.get("guid").and_then(Value::as_str).unwrap_or(""),
                }));
            }
            "Target.targetCreated" => {
                let info = p.get("targetInfo").unwrap_or(&Value::Null);
                if info.get("type").and_then(Value::as_str) != Some("page") {
                    return;
                }
                if info.get("openerId").and_then(Value::as_str) != Some(self.opener.as_str()) {
                    return;
                }
                if self.opened_tab.is_some() {
                    return;
                }
                let id = info.get("targetId").and_then(Value::as_str).unwrap_or("");
                let url = info.get("url").and_then(Value::as_str).unwrap_or("");
                self.opened_tab = Some(json!({ "tab": id, "url": url }));
            }
            "Target.targetInfoChanged" => {
                let info = p.get("targetInfo").unwrap_or(&Value::Null);
                let Some(id) = info.get("targetId").and_then(Value::as_str) else { return };
                let Some(cur) = self.opened_tab.as_mut() else { return };
                if cur.get("tab").and_then(Value::as_str) != Some(id) {
                    return;
                }
                if let Some(url) = info.get("url").and_then(Value::as_str) {
                    if !url.is_empty() && url != "about:blank" {
                        cur["url"] = json!(url);
                    }
                }
            }
            "Runtime.consoleAPICalled" => {
                if p.get("type").and_then(Value::as_str) != Some("error") {
                    return;
                }
                let url = p
                    .pointer("/stackTrace/callFrames/0/url")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if url.starts_with("chrome-extension://") || (!url.is_empty() && !self.same_origin(url)) {
                    return;
                }
                let text = p
                    .get("args")
                    .and_then(Value::as_array)
                    .map(|args| {
                        args.iter()
                            .map(|a| {
                                a.get("value")
                                    .and_then(Value::as_str)
                                    .map(str::to_string)
                                    .or_else(|| a.get("description").and_then(Value::as_str).map(str::to_string))
                                    .or_else(|| a.get("value").map(|v| v.to_string()))
                                    .unwrap_or_default()
                            })
                            .filter(|s| !s.is_empty())
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                self.push(json!({ "kind": "console", "text": clip_event(&text) }));
            }
            _ => {}
        }
    }
}

/// Split `http://host:port` into (`host:port`, "").
fn split_endpoint(endpoint: &str) -> Res<(String, String)> {
    let rest = endpoint
        .strip_prefix("http://")
        .ok_or_else(|| format!("endpoint must start with http:// (got {endpoint})"))?;
    let (authority, base) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return Err(format!("endpoint has no host (got {endpoint})"));
    }
    Ok((authority.to_string(), base.to_string()))
}

/// One request with no body, returning the response body as text.
pub fn http(endpoint: &str, path: &str, method: &str) -> Res<String> {
    let (authority, base) = split_endpoint(endpoint)?;
    let mut stream = TcpStream::connect(&authority)
        .map_err(|e| format!("cannot reach {authority}: {e}. Is the browser running with --remote-debugging-port?"))?;
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(10))).ok();

    let req = format!("{method} {base}{path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).map_err(|e| e.to_string())?;

    // Read the headers, then exactly Content-Length bytes. Reading "until close"
    // is tempting but breaks as soon as a read timeout is set (EAGAIN), and it
    // depends on the peer closing promptly.
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        let n = stream.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err(format!("connection closed before headers from {path}"));
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let status = head.lines().next().unwrap_or("").to_string();
    if !status.contains(" 200") && !status.contains(" 204") {
        return Err(format!("{method} {path} -> {status}"));
    }
    let content_length = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            if k.eq_ignore_ascii_case("content-length") {
                v.trim().parse::<usize>().ok()
            } else {
                None
            }
        });

    let body = match content_length {
        Some(len) => {
            while buf.len() < header_end + len {
                let n = stream.read(&mut chunk).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let end = (header_end + len).min(buf.len());
            buf[header_end..end].to_vec()
        }
        None => {
            // no length: drain what is already buffered plus anything readable
            let mut rest = buf[header_end..].to_vec();
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => rest.extend_from_slice(&chunk[..n]),
                    Err(_) => break,
                }
            }
            rest
        }
    };

    Ok(String::from_utf8_lossy(&body).to_string())
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

pub fn http_json(endpoint: &str, path: &str, method: &str) -> Res<Value> {
    let body = http(endpoint, path, method)?;
    if body.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&body).map_err(|e| format!("bad JSON from {path}: {e}"))
}

/// A page target as reported by `/json/list`.
#[derive(Debug, Clone)]
pub struct Target {
    pub id: String,
    pub title: String,
    pub url: String,
    pub ws_url: String,
}

pub fn targets(endpoint: &str) -> Res<Vec<Target>> {
    let v = http_json(endpoint, "/json/list", "GET")?;
    let arr = v.as_array().cloned().unwrap_or_default();
    Ok(arr
        .iter()
        .filter(|t| t.get("type").and_then(Value::as_str) == Some("page"))
        .map(|t| Target {
            id: t.get("id").and_then(Value::as_str).unwrap_or_default().to_string(),
            title: t.get("title").and_then(Value::as_str).unwrap_or_default().to_string(),
            url: t.get("url").and_then(Value::as_str).unwrap_or_default().to_string(),
            ws_url: t
                .get("webSocketDebuggerUrl")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
        .collect())
}

/// Resolve a tab id (or prefix). `None` picks the first page tab.
pub fn resolve(endpoint: &str, tab: Option<&str>) -> Res<Target> {
    let all = targets(endpoint)?;
    if all.is_empty() {
        return Err("no page tabs open; use tabs(action=open, url=...) first".into());
    }
    match tab {
        None => Ok(all[0].clone()),
        Some(t) => all
            .iter()
            .find(|x| x.id == t)
            .or_else(|| all.iter().find(|x| x.id.starts_with(t)))
            .cloned()
            .ok_or_else(|| {
                let ids: Vec<&str> = all.iter().map(|x| x.id.as_str()).collect();
                format!("tab {t} not found. open tabs: {}", ids.join(", "))
            }),
    }
}

/// Tabs this server created. They are allowed to keep rendering in the
/// background for their whole life; a tab the human opened is only nudged for
/// the duration of a capture, because focus emulation makes a page believe it
/// is focused (presence statuses, "you are active" timers) and that is their
/// page's truth, not ours to change.
static OWNED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

pub fn is_owned(id: &str) -> bool {
    OWNED.lock().map(|s| s.contains(id)).unwrap_or(false)
}

/// One browser-level connection, kept for the life of the process: opening a
/// tab used to cost an HTTP lookup and a fresh websocket handshake each time.
static BROWSER: std::sync::Mutex<Option<Conn>> = std::sync::Mutex::new(None);

/// Run one browser-level command (Target.*) on the shared connection,
/// reconnecting once if the browser restarted. A retry happens only when the
/// old socket died before the browser answered, so nothing runs twice.
fn browser_call(endpoint: &str, method: &str, params: Value) -> Res<Value> {
    let mut slot = BROWSER.lock().map_err(|_| "browser connection lock poisoned")?;
    if let Some(conn) = slot.as_mut() {
        let before = conn.replies;
        match conn.call(method, params.clone()) {
            Ok(v) => return Ok(v),
            Err(e) if !conn.dead => return Err(e),
            Err(e) if conn.replies != before => {
                *slot = None;
                return Err(e);
            }
            Err(_) => *slot = None,
        }
    }
    let version = http_json(endpoint, "/json/version", "GET")?;
    let ws_url = version
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .ok_or("the browser exposed no DevTools socket")?
        .to_string();
    let mut conn = Conn::open(&ws_url)?;
    let out = conn.call(method, params);
    if !conn.dead {
        *slot = Some(conn);
    }
    out
}

/// The DevTools socket of a page target. Chromium serves every target at
/// `/devtools/page/<id>`, so a tab we just created is reachable at once,
/// without waiting for `/json/list` to catch up and report it.
pub fn page_ws_url(endpoint: &str, id: &str) -> Res<String> {
    let (authority, base) = split_endpoint(endpoint)?;
    Ok(format!("ws://{authority}{base}/devtools/page/{id}"))
}

/// Create a page tab without taking the keyboard away from the human.
///
/// `/json/new` activates the tab it creates, which raises the browser window:
/// measured here, focus went from the chat window to Helium on every open, so a
/// human typing mid-run lost their keystrokes. `Target.createTarget` with
/// `background: true` creates the tab and leaves the focused window alone.
/// `foreground: true` is the deliberate exception (show this to the human).
pub fn new_tab(endpoint: &str, url: &str, foreground: bool) -> Res<String> {
    let res = browser_call(endpoint, "Target.createTarget", json!({ "url": url, "background": !foreground }))?;
    let id = res
        .get("targetId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if id.is_empty() {
        return Err(format!("the browser created no target for {url}"));
    }
    if let Ok(mut owned) = OWNED.lock() {
        owned.insert(id.clone());
    }
    Ok(id)
}

/// Close a page tab without activating the browser window.
///
/// `GET /json/close/<id>` goes through Chrome's tab-strip close, which picks the
/// next tab and activates the window: measured, focus left the chat window for
/// Helium on every close. `Target.closeTarget` tears the target down instead.
pub fn close_tab(endpoint: &str, id: &str) -> Res<bool> {
    // `Target.closeTarget` means "asked", not "gone"; the target list lags the
    // close (measured: a plain close was still listed), so the destroy event is
    // the confirmation. Discovery is on for the check and off after it: the
    // answer is exact and no event stream is left behind.
    browser_call(endpoint, "Target.setDiscoverTargets", json!({ "discover": true }))?;
    let closed = match browser_call(endpoint, "Target.closeTarget", json!({ "targetId": id })) {
        Ok(_) => {
            let mut slot = BROWSER.lock().map_err(|_| "browser connection lock poisoned")?;
            match slot.as_mut() {
                Some(conn) => conn.await_destroyed(id, 1500),
                None => false,
            }
        }
        Err(e) => {
            let _ = browser_call(endpoint, "Target.setDiscoverTargets", json!({ "discover": false }));
            return Err(e);
        }
    };
    let _ = browser_call(endpoint, "Target.setDiscoverTargets", json!({ "discover": false }));
    if closed {
        if let Ok(mut owned) = OWNED.lock() {
            owned.remove(id);
        }
    }
    Ok(closed)
}

/// An open DevTools connection to one page.
pub struct Conn {
    ws: WebSocket<TcpStream>,
    next: i64,
    focus_on: bool,
    timeout_ms: u64,
    /// The socket itself failed (closed, reset). A timeout does not set this:
    /// a busy page is still connected and its late reply is skipped by id.
    pub dead: bool,
    /// Replies received. `with_conn` compares it across a failure to know
    /// whether anything reached the page before the socket died, which is the
    /// only case where running the caller's actions again is safe.
    pub replies: u64,
    /// Out-of-process iframes of this page, as (target id, session id, url),
    /// kept current from attach/detach events once `frames()` turned
    /// auto-attach on. Cross-origin frames run in another renderer, so the
    /// page's own script cannot see into them; each needs its own session.
    oopifs: Vec<(String, String, String)>,
    auto_attach: bool,
    /// `Runtime.enable` has been called on this connection.
    runtime_on: bool,
    /// The binding this connection has installed, so it is added once.
    pub binding: Option<String>,
    /// Set by `next_msg` when a read hit its deadline rather than failing. A
    /// timeout and a broken socket must not be told apart by a string.
    pub timed_out: bool,
    /// Set when the page announced that its execution contexts are gone
    /// (`Runtime.executionContextsCleared`, which is what a navigation sends).
    /// Everything installed in the document died with it.
    pub contexts_cleared: bool,
    /// Causal events for an action that asked for a settle window.
    receipts: Option<Receipts>,
    /// `Page.enable` has been sent on this connection. One round trip per tab,
    /// then free; kept out of `Conn::open` so the browser-level socket never
    /// sends it.
    page_on: bool,
    /// The JavaScript dialog the page is showing, if any: `type`, `message`,
    /// `defaultPrompt`, `url`. While this is set the renderer is blocked, so
    /// every other call would wait out its timeout.
    pub dialog: Option<Value>,
    /// Bumped for every dialog that opens. A wait records it on entry and
    /// breaks only when it moves, which lets an already-open dialog be answered
    /// without breaking the very call that answers it.
    pub dialog_seq: u64,
    /// `Network.enable` is on. A settle window that had to skip its teardown
    /// because a dialog blocked the page leaves this set, so the next call can
    /// turn it off once the dialog is gone.
    network_on: bool,
    /// Targets this connection has seen destroyed. A close asks for one of
    /// these, and the event is recorded in the funnel every message passes, so
    /// it still counts when it arrives before the close's own reply.
    destroyed: Vec<String>,
}

/// Events that mean the page is still doing something. `Page.*` is deliberately
/// absent: frame lifecycle chatter would hold a settle window open to its cap,
/// and the wait is about network quiet.
fn settle_activity(v: &Value) -> bool {
    match v.get("method").and_then(Value::as_str) {
        Some(m) => {
            m.starts_with("Network.")
                || m == "Target.targetCreated"
                || m == "Target.targetInfoChanged"
                || m == "Runtime.exceptionThrown"
                || m == "Runtime.consoleAPICalled"
                // A commit and a started download both mean the action is still
                // unfolding; without them the quiet window can close first.
                || m == "Page.frameNavigated"
                || m == "Page.downloadWillBegin"
        }
        None => false, // a reply is not page activity
    }
}

fn timeout_from_env() -> u64 {
    std::env::var("CDP_MCP_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(30_000)
}

impl Conn {
    pub fn open(ws_url: &str) -> Res<Self> {
        // Connect by hand rather than `tungstenite::connect` so every step is
        // bounded: a wedged browser must fail the call, not hang the server.
        let authority = ws_url
            .strip_prefix("ws://")
            .and_then(|r| r.split('/').next())
            .ok_or_else(|| format!("unsupported DevTools socket {ws_url}"))?;
        let addr = authority
            .to_socket_addrs()
            .map_err(|e| format!("cannot resolve {authority}: {e}"))?
            .next()
            .ok_or_else(|| format!("cannot resolve {authority}"))?;
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
            .map_err(|e| format!("cannot reach {authority}: {e}"))?;
        stream.set_nodelay(true).ok();
        stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
        stream.set_write_timeout(Some(Duration::from_secs(10))).ok();
        // Chromium sends a whole screenshot as one frame; the default 16 MiB
        // frame cap rejects a tall full-page capture.
        let config = WebSocketConfig::default()
            .max_message_size(Some(512 << 20))
            .max_frame_size(Some(512 << 20));
        let (ws, _resp) = client_with_config(ws_url, stream, Some(config))
            .map_err(|e| format!("websocket handshake failed: {e}"))?;
        // Bounded so a stuck page cannot hang an agent that was left alone.
        // Configurable per profile with `call_timeout_ms`.
        let ms = timeout_from_env();
        let mut conn = Conn { ws, next: 0, focus_on: false, timeout_ms: ms, dead: false, replies: 0,
                              oopifs: Vec::new(), auto_attach: false, runtime_on: false,
                              binding: None, timed_out: false, contexts_cleared: false,
                              receipts: None, page_on: false, dialog: None, dialog_seq: 0, network_on: false,
                              destroyed: Vec::new() };
        conn.set_read_timeout(ms);
        // No domain is enabled on connect: each enable costs a round trip, and
        // most commands do not need one. `enable_runtime` turns on the Runtime
        // domain the first time a wait needs its events, and only then.
        Ok(conn)
    }

    fn set_read_timeout(&mut self, ms: u64) {
        self.ws.get_mut().set_read_timeout(Some(Duration::from_millis(ms.max(1)))).ok();
    }

    /// The next protocol message, answering pings on the way. `waiting` names
    /// what we are waiting for, so a timeout says which call stalled.
    fn next_msg(&mut self, waiting: &str, ms: u64) -> Res<Value> {
        self.timed_out = false;
        loop {
            let m = match self.ws.read() {
                Ok(m) => m,
                Err(tungstenite::Error::Io(e))
                    if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
                {
                    self.timed_out = true;
                    return Err(format!("{waiting}: no reply within {ms} ms (the page is busy or frozen)"));
                }
                Err(e) => {
                    self.dead = true;
                    return Err(format!("connection to the page lost: {e}"));
                }
            };
            let v = match m {
                Message::Text(t) => serde_json::from_str::<Value>(t.as_str()),
                Message::Binary(b) => serde_json::from_slice::<Value>(&b),
                Message::Ping(p) => {
                    let _ = self.ws.send(Message::Pong(p));
                    continue;
                }
                Message::Close(_) => {
                    self.dead = true;
                    return Err("the page closed the connection".into());
                }
                _ => continue,
            };
            if let Ok(v) = v {
                self.track_frames(&v);
                if let Some(r) = self.receipts.as_mut() {
                    r.note(&v);
                }
                return Ok(v);
            }
        }
    }

    /// Read whatever has already arrived, without blocking and without counting
    /// it as a reply.
    ///
    /// A dialog can open between two tool calls; the event then sits in the
    /// socket while nothing reads it, and a command written before it is seen
    /// would run the moment the dialog is dismissed. A dialog the human
    /// dismissed while the server was idle has to be seen for the same reason.
    ///
    /// `replies` is deliberately not touched, so `with_conn_retry`'s
    /// "nothing reached the page" test keeps meaning what it meant.
    pub fn drain_arrived(&mut self) -> Res<usize> {
        if self.dead {
            return Ok(0);
        }
        if let Err(e) = self.ws.get_mut().set_nonblocking(true) {
            return Err(format!("cannot poll the page socket: {e}"));
        }
        let mut seen: Vec<Value> = Vec::new();
        let mut pings = Vec::new();
        let mut out: Res<()> = Ok(());
        loop {
            if seen.len() >= 256 {
                break; // bounded; the rest waits for the next call
            }
            match self.ws.read() {
                Ok(Message::Text(t)) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&t) {
                        seen.push(v);
                    }
                }
                Ok(Message::Binary(b)) => {
                    if let Ok(v) = serde_json::from_slice::<Value>(&b) {
                        seen.push(v);
                    }
                }
                Ok(Message::Ping(p)) => pings.push(p),
                Ok(Message::Close(_)) => {
                    self.dead = true;
                    out = Err("the page closed the connection".into());
                    break;
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
                {
                    break;
                }
                Err(e) => {
                    self.dead = true;
                    out = Err(format!("connection to the page lost: {e}"));
                    break;
                }
            }
        }
        // Back to blocking before anything is sent: a partial write on a
        // non-blocking socket is a bug waiting to happen.
        let _ = self.ws.get_mut().set_nonblocking(false);
        self.timed_out = false;
        for v in &seen {
            self.track_frames(v);
            if let Some(r) = self.receipts.as_mut() {
                r.note(v);
            }
        }
        for p in pings {
            let _ = self.ws.send(Message::Pong(p));
        }
        out.map(|_| seen.len())
    }

    /// Keep the out-of-process iframe list current. Every message passes
    /// through here, so an event is never missed because a call skipped it.
    fn track_frames(&mut self, v: &Value) {
        match v.get("method").and_then(Value::as_str) {
            Some("Target.attachedToTarget") => {
                let p = &v["params"];
                let info = &p["targetInfo"];
                if info["type"] == "iframe" {
                    if let (Some(t), Some(sid)) = (info["targetId"].as_str(), p["sessionId"].as_str()) {
                        let url = info["url"].as_str().unwrap_or_default().to_string();
                        self.oopifs.retain(|f| f.0 != t);
                        self.oopifs.push((t.to_string(), sid.to_string(), url));
                    }
                }
            }
            Some("Target.detachedFromTarget") => {
                if let Some(sid) = v.pointer("/params/sessionId").and_then(Value::as_str) {
                    self.oopifs.retain(|f| f.1 != sid);
                }
            }
            Some("Target.targetInfoChanged") => {
                let info = &v["params"]["targetInfo"];
                if let (Some(t), Some(url)) = (info["targetId"].as_str(), info["url"].as_str()) {
                    if let Some(f) = self.oopifs.iter_mut().find(|f| f.0 == t) {
                        f.2 = url.to_string();
                    }
                }
            }
            // A dialog blocks the renderer until it is answered: input and
            // evaluations stop replying, so every wait has to know about it.
            Some("Page.javascriptDialogOpening") => {
                let p = &v["params"];
                self.dialog_seq += 1;
                self.dialog = Some(json!({
                    "type": p.get("type").cloned().unwrap_or(Value::Null),
                    "message": p.get("message").cloned().unwrap_or(Value::Null),
                    "defaultPrompt": p.get("defaultPrompt").cloned().unwrap_or(Value::Null),
                    "url": p.get("url").cloned().unwrap_or(Value::Null),
                }));
            }
            Some("Page.javascriptDialogClosed") => self.dialog = None,
            Some("Target.targetDestroyed") => {
                if let Some(id) = v.pointer("/params/targetId").and_then(Value::as_str) {
                    if !self.destroyed.iter().any(|d| d == id) {
                        self.destroyed.push(id.to_string());
                    }
                    if self.destroyed.len() > 32 {
                        self.destroyed.remove(0);
                    }
                }
            }
            // A navigation, or a document swap. Everything the page was holding
            // for us is gone, including any observer. The binding itself
            // survives (measured), so only the installer has to run again.
            Some("Runtime.executionContextsCleared") => self.contexts_cleared = true,
            _ => {}
        }
    }

    /// The page's out-of-process iframes, as (target id, session id, url),
    /// in a stable order. The first call turns auto-attach on; Chromium
    /// reports every existing frame before it replies, so the list is
    /// complete when this returns.
    pub fn frames(&mut self) -> Res<Vec<(String, String, String)>> {
        if !self.auto_attach {
            self.call("Target.setAutoAttach",
                      json!({ "autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true }))?;
            self.auto_attach = true;
        }
        let mut f = self.oopifs.clone();
        f.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(f)
    }

    fn send_cmd(&mut self, method: &str, params: &Value) -> Res<i64> {
        self.send_to(None, method, params)
    }

    fn send_to(&mut self, session: Option<&str>, method: &str, params: &Value) -> Res<i64> {
        self.next += 1;
        let id = self.next;
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(sid) = session {
            msg["sessionId"] = json!(sid);
        }
        let msg = msg.to_string();
        if let Err(e) = self.ws.send(Message::text(msg)) {
            self.dead = true;
            return Err(format!("connection to the page lost: {e}"));
        }
        Ok(id)
    }

    /// Read until a named protocol event arrives, skipping everything else.
    ///
    /// Only safe when no other call is in flight: replies are skipped, not
    /// queued, so this is for a deliberate one-event wait.
    pub fn wait_event(&mut self, method: &str, ms: u64) -> Res<Value> {
        let deadline = std::time::Instant::now() + Duration::from_millis(ms);
        let out = loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break Err(format!("no {method} within {ms} ms"));
            }
            self.set_read_timeout(left.as_millis() as u64);
            match self.next_msg(method, ms) {
                Ok(v) if v.get("method").and_then(Value::as_str) == Some(method) => {
                    break Ok(v.get("params").cloned().unwrap_or(Value::Null));
                }
                Ok(_) => continue,
                Err(e) => break Err(e),
            }
        };
        let restore = self.timeout_ms;
        self.set_read_timeout(restore);
        out
    }

    /// Start a tiny screencast so the compositor keeps producing frames.
    ///
    /// A window that is not on the visible workspace, or a tab nobody is looking
    /// at, produces no frames at all, and `Page.captureScreenshot` waits for one
    /// that never arrives: measured 298 ms with the window on screen, 10.4 s
    /// with it on another workspace, and a run of six captures degraded
    /// 98 -> 19 235 ms. Stopping the stream *before* the capture does not help,
    /// because the compositor goes idle again. A live screencast forces frame
    /// production throughout, which is the workaround Playwright's maintainers
    /// documented for headed Chrome; one 64 px frame a second costs nothing.
    /// Pair with `stop_frames`.
    pub fn start_frames(&mut self) -> Res<()> {
        self.call(
            "Page.startScreencast",
            json!({ "format": "jpeg", "quality": 10, "maxWidth": 64, "maxHeight": 64,
                    "everyNthFrame": 1, "maxFramesInFlight": 4 }),
        )?;
        // Three frames, not one: the first frame after a window stops being
        // visible proves the pipeline wakes up, and the capture that follows
        // still waited 12 s for a *compositor* frame. Waiting for the stream to
        // settle makes the capture cheap on the first try.
        for _ in 0..3 {
            let Ok(p) = self.wait_event("Page.screencastFrame", 1_500) else { break };
            if let Some(sid) = p.get("sessionId").and_then(Value::as_i64) {
                let _ = self.call("Page.screencastFrameAck", json!({ "sessionId": sid }));
            }
        }
        Ok(())
    }

    pub fn stop_frames(&mut self) {
        let _ = self.call("Page.stopScreencast", json!({}));
    }

    /// Turn the Runtime domain on once per connection.
    ///
    /// Enabling costs a round trip (measured 16 ms on first call) and then makes
    /// the page's own events flow: `Runtime.bindingCalled` is how a wait is
    /// woken without asking anything, and `executionContextsCleared` is how a
    /// navigation announces that the page's state is gone.
    pub fn enable_runtime(&mut self) -> Res<()> {
        if !self.runtime_on {
            self.call("Runtime.enable", json!({}))?;
            self.runtime_on = true;
        }
        Ok(())
    }

    /// Turn Network off when a settle window had to leave it on because the page
    /// was blocked. Called before the next use of the connection, so a dialog
    /// that has since been answered does not leave an event stream on.
    pub fn cleanup_domains(&mut self) -> Res<()> {
        if self.network_on && self.dialog.is_none() {
            self.call_within("Network.disable", json!({}), 400)?;
            self.network_on = false;
        }
        Ok(())
    }

    /// Enable the Page domain once per connection. It is the only way to hear
    /// about a JavaScript dialog before the 30 s timeout, and about frame
    /// navigations and downloads the caller may want reported.
    ///
    /// Deliberately not in `Conn::open`: the browser-level socket is a `Conn`
    /// too, and `Page.enable` there is a protocol error.
    pub fn enable_page(&mut self) -> Res<()> {
        if !self.page_on {
            self.call("Page.enable", json!({}))?;
            self.page_on = true;
        }
        Ok(())
    }

    /// Answer the dialog the page is showing, and report the one that replaced
    /// it, if any: `confirm()` followed by `alert()` opens a second dialog as
    /// the first closes. One answer per call keeps that deterministic.
    pub fn handle_dialog(&mut self, accept: bool, text: Option<&str>) -> Res<Option<Value>> {
        let seq = self.dialog_seq;
        let beforeunload = self.dialog.as_ref()
            .and_then(|d| d.get("type")).and_then(Value::as_str) == Some("beforeunload");
        let mut params = json!({ "accept": accept });
        if let Some(t) = text {
            params["promptText"] = json!(t);
        }
        match self.call("Page.handleJavaScriptDialog", params) {
            Ok(_) => {
                // The Closed event normally clears it; this covers the ordering
                // where the reply arrives first. A newer dialog is left alone.
                if self.dialog_seq == seq {
                    self.dialog = None;
                }
                // A chained dialog opens as the renderer resumes, which can land
                // just after this reply: wait briefly for it so the caller is told
                // now instead of discovering it on the next call.
                if self.dialog.is_none() {
                    let _ = self.wait_event("Page.javascriptDialogOpening", 150);
                    self.timed_out = false;
                }
                let out = self.dialog.clone();
                // A settle window that ran into this dialog left Network on; with
                // the page unblocked it can be turned off now rather than later.
                let _ = self.cleanup_domains();
                Ok(out)
            }
            Err(e) => {
                // Accepting "leave" tears the tab down: the socket can close
                // before the reply arrives, and that is the expected end.
                if beforeunload && accept && self.dead {
                    self.dialog = None;
                    return Ok(None);
                }
                if self.dialog_seq != seq && self.dialog.is_some() {
                    Ok(self.dialog.clone())
                } else {
                    Err(e)
                }
            }
        }
    }

    /// The page's current URL from the browser process, which keeps answering
    /// while a dialog blocks the renderer. Used where `location.href` cannot.
    pub fn target_url(&mut self) -> Option<String> {
        self.call("Target.getTargetInfo", json!({}))
            .ok()
            .and_then(|v| v.pointer("/targetInfo/url").and_then(Value::as_str).map(str::to_string))
    }

    /// Install a page-side binding the page can call to wake this server.
    ///
    /// Added once and never removed: the probe showed it survives navigation
    /// (`typeof window.__name === 'function'` in the new document), so a rename
    /// is the only reason to pay the 8 ms again.
    pub fn add_binding(&mut self, name: &str) -> Res<()> {
        if self.binding.as_deref() == Some(name) {
            return Ok(());
        }
        self.call("Runtime.addBinding", json!({ "name": name }))?;
        self.binding = Some(name.to_string());
        Ok(())
    }

    /// Start collecting causal events for an action that asked for a settle
    /// window. Off by default: enabling Network on every click would cost
    /// latency and fill the socket for actions nobody is debugging.
    pub fn begin_receipts(&mut self, page_url: &str, opener: &str) -> Res<()> {
        // Enable first: Runtime.enable replays buffered console entries, and a
        // receipt armed before the call would blame them on this action.
        self.enable_runtime()?;
        self.call("Network.enable", json!({}))?;
        self.network_on = true;
        if let Err(e) = self.call("Target.setDiscoverTargets", json!({ "discover": true })) {
            if self.call("Network.disable", json!({})).is_ok() {
                self.network_on = false;
            }
            return Err(e);
        }
        self.receipts = Some(Receipts::new(origin_of(page_url), opener.to_string()));
        Ok(())
    }

    /// Has `Target.targetDestroyed` arrived for this target? Reads until it has
    /// or `ms` elapses. Recorded in `track_frames`, so one that arrived before
    /// the close reply is still counted.
    pub fn await_destroyed(&mut self, id: &str, ms: u64) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_millis(ms);
        let seen = |c: &Self| c.destroyed.iter().any(|d| d == id);
        let out = loop {
            if seen(self) {
                self.destroyed.retain(|d| d != id);
                break true;
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break false;
            }
            self.set_read_timeout(left.as_millis().max(1) as u64);
            if self.next_msg("target close", ms).is_err() {
                break seen(self);
            }
        };
        self.set_read_timeout(self.timeout_ms);
        out
    }


    /// Read until `quiet_ms` passes with no message while no same-origin
    /// request is in flight, or `max_ms` elapses, then turn the domains off.
    /// This is a socket read, not a poll: an in-flight API call keeps the
    /// window open, and a still page costs one timeout.
    pub fn settle_receipts(&mut self, quiet_ms: u64, max_ms: u64) -> ReceiptOutcome {
        let deadline = std::time::Instant::now() + Duration::from_millis(max_ms);
        // Quiet is measured from the last event that means the page is working.
        // Page.* events are seen (a dialog must end the wait) but do not count:
        // frame lifecycle chatter would hold every settle window to its cap.
        let mut last_activity = std::time::Instant::now();
        loop {
            // Blocked before we even read: no event can arrive and no teardown
            // command would be answered, so leave now instead of waiting out
            // slices against a renderer that cannot run.
            if self.dialog.is_some() {
                break;
            }
            let cap_left = deadline.saturating_duration_since(std::time::Instant::now());
            if cap_left.is_zero() {
                break;
            }
            let quiet_left = quiet_ms.saturating_sub(last_activity.elapsed().as_millis() as u64);
            let wait = if quiet_left == 0 {
                // Quiet long enough. Leave unless a same-origin request is still
                // open; a short slice keeps that landing responsive instead of
                // spinning on 1 ms reads.
                let idle = self.receipts.as_ref().map(|r| r.requests.is_empty()).unwrap_or(true);
                if idle {
                    break;
                }
                Duration::from_millis(quiet_ms.clamp(50, 250))
            } else {
                Duration::from_millis(quiet_left)
            };
            let slice = wait.min(cap_left).as_millis().max(1) as u64;
            self.set_read_timeout(slice);
            match self.next_msg("action settle", slice) {
                Ok(v) => {
                    // The renderer is blocked; nothing more will settle.
                    if self.dialog.is_some() {
                        break;
                    }
                    if settle_activity(&v) {
                        last_activity = std::time::Instant::now();
                    }
                }
                Err(_) if self.timed_out => {
                    self.timed_out = false;
                }
                Err(_) => break,
            }
        }
        // A quiet exit leaves timed_out set; clear it so the next call does not
        // mistake a healthy page for a stalled one.
        self.timed_out = false;
        let restore = self.timeout_ms;
        self.set_read_timeout(restore);
        if !self.dead {
            // Bounded: if a dialog is up the renderer is blocked, and a teardown
            // command must not turn a settled action into a 30 s wait.
            let _ = self.call_within("Target.setDiscoverTargets", json!({ "discover": false }), 400);
            // A blocked renderer would sit on this command; the next call turns
            // Network off once the dialog is answered.
            if self.dialog.is_none() && self.call_within("Network.disable", json!({}), 400).is_ok() {
                self.network_on = false;
            }
        }
        match self.receipts.take() {
            Some(r) => ReceiptOutcome { entries: r.entries, opened_tab: r.opened_tab,
                                        downloads: r.downloads },
            None => ReceiptOutcome::default(),
        }
    }

    /// Block until the page calls `name`, with nothing sent to the page.
    ///
    /// `Ok(None)` means the wait elapsed. That return value is the clock the
    /// caller uses for "the page has gone quiet", so the page never has to run a
    /// timer of its own and a page that changes nothing costs zero traffic: this
    /// is a local read timeout on a socket, not a question.
    pub fn wait_binding(&mut self, name: &str, ms: u64) -> Res<Option<Value>> {
        let deadline = std::time::Instant::now() + Duration::from_millis(ms.max(1));
        let out = loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break Ok(None);
            }
            let left = left.as_millis().max(1) as u64;
            self.set_read_timeout(left);
            match self.next_msg(name, left) {
                Ok(v) => {
                    if v.get("method").and_then(Value::as_str) == Some("Runtime.bindingCalled")
                        && v.pointer("/params/name").and_then(Value::as_str) == Some(name)
                    {
                        break Ok(Some(v.pointer("/params/payload").cloned().unwrap_or(Value::Null)));
                    }
                    // A document swap means whatever the caller installed is
                    // gone. Returning now lets it reinstall immediately instead
                    // of sitting out the whole read timeout first.
                    if self.contexts_cleared {
                        break Ok(None);
                    }
                    // Any other event: keep waiting for ours.
                    // A dialog blocks the renderer, so the wake this waits for cannot
                    // arrive. Return at once, or the caller waits out its timeout
                    // and every retry after that.
                    if self.dialog.is_some() {
                        break Ok(None);
                    }
                }
                // A read timeout is the clock, not a failure. Any other error is
                // a real one: mistaking it for "the wait elapsed" would report
                // success where the connection had actually gone.
                Err(_) if self.timed_out => break Ok(None),
                Err(e) => break Err(e),
            }
        };
        let restore = self.timeout_ms;
        self.set_read_timeout(restore);
        out
    }

    /// Tell Chromium to keep this tab rendering while it is not the one on
    /// screen.
    ///
    /// A background or occluded tab has its compositor frozen, and two things
    /// measured on this machine depend on it: `Page.captureScreenshot` waits
    /// forever for a frame that never arrives, and `Input.dispatchTouchEvent`
    /// is never acked (30 s read timeout, then a dead connection). Playwright
    /// enables the same switch on every page it drives (`crPage.ts`,
    /// `_initialize`), which is why Playwright can screenshot a background tab
    /// and a naive CDP client cannot.
    pub fn set_focus_emulation(&mut self, on: bool) -> Res<()> {
        if self.focus_on == on {
            return Ok(());
        }
        self.call("Emulation.setFocusEmulationEnabled", json!({ "enabled": on }))?;
        self.focus_on = on;
        Ok(())
    }

    /// Send one command and wait for its reply, skipping protocol events.
    pub fn call(&mut self, method: &str, params: Value) -> Res<Value> {
        self.call_in(None, method, params)
    }

    /// `call`, addressed to an attached child session (an iframe) when given.
    pub fn call_in(&mut self, session: Option<&str>, method: &str, params: Value) -> Res<Value> {
        let id = self.send_to(session, method, &params)?;
        let total = self.timeout_ms;
        let replies = self.await_replies(&[id], method, total);
        self.set_read_timeout(total);
        let v = replies?.into_iter().next().unwrap_or(Value::Null);
        if let Some(err) = v.get("error") {
            let m = err.get("message").and_then(Value::as_str).unwrap_or("unknown error");
            return Err(format!("{method}: {m}"));
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }

    /// One reply per id, within `total` milliseconds overall.
    ///
    /// The per-read timeout alone cannot bound a call: every protocol event
    /// resets it, so a page emitting events faster than the timeout can keep a
    /// stalled reply waiting forever. The deadline is fixed here instead, and
    /// each read gets only what is left of it.
    fn await_replies(&mut self, ids: &[i64], waiting: &str, total: u64) -> Res<Vec<Value>> {
        let deadline = std::time::Instant::now() + Duration::from_millis(total);
        let start_seq = self.dialog_seq;
        let mut got: Vec<Option<Value>> = vec![None; ids.len()];
        let mut left = ids.len();
        while left > 0 {
            let remaining = deadline
                .saturating_duration_since(std::time::Instant::now())
                .as_millis() as u64;
            if remaining == 0 {
                self.timed_out = true;
                return Err(format!("{waiting}: no reply within {total} ms (the page is busy or frozen)"));
            }
            self.set_read_timeout(remaining);
            // `total` is for the message: a timeout after events, or after a
            // dialog, reports the call's deadline rather than the last slice.
            let v = self.next_msg(waiting, total)?;
            // A dialog that opened during this wait blocks the renderer; one
            // that was already open must not, or answering it would break the
            // very call that answers it.
            if self.dialog_seq != start_seq {
                if let Some(d) = self.dialog.as_ref() {
                    let ty = d.get("type").and_then(Value::as_str).unwrap_or("dialog");
                    return Err(format!("dialog open: {ty}"));
                }
            }
            let Some(id) = v.get("id").and_then(Value::as_i64) else { continue };
            let Some(slot) = ids.iter().position(|i| *i == id) else { continue };
            if got[slot].is_some() {
                continue;
            }
            self.replies += 1;
            got[slot] = Some(v);
            left -= 1;
        }
        Ok(got.into_iter().map(|v| v.unwrap_or(Value::Null)).collect())
    }

    /// `call` with its own reply deadline, for a command that legitimately
    /// takes longer than the default (an in-page wait).
    pub fn call_within(&mut self, method: &str, params: Value, ms: u64) -> Res<Value> {
        let saved = self.timeout_ms;
        self.timeout_ms = ms;
        self.set_read_timeout(ms);
        let out = self.call(method, params);
        self.timeout_ms = saved;
        self.set_read_timeout(saved);
        out
    }

    /// Send several commands at once and collect their replies in order.
    /// One round trip instead of one per command: a click is three events, and
    /// waiting for each reply cost more than the events themselves.
    pub fn call_many(&mut self, cmds: &[(&str, Value)]) -> Res<Vec<Value>> {
        let mut ids = Vec::with_capacity(cmds.len());
        for (method, params) in cmds {
            ids.push(self.send_cmd(method, params)?);
        }
        let label = cmds.first().map(|c| c.0).unwrap_or("batched call");
        let total = self.timeout_ms;
        let replies = self.await_replies(&ids, label, total);
        self.set_read_timeout(total);
        let replies = replies?;
        let mut first_err: Option<String> = None;
        let mut out = Vec::with_capacity(replies.len());
        // Every reply is collected before failing, so none is left on the socket
        // to be mistaken for the answer to a later call.
        for v in replies {
            if let Some(err) = v.get("error") {
                let m = err.get("message").and_then(Value::as_str).unwrap_or("unknown error");
                first_err.get_or_insert_with(|| format!("{label}: {m}"));
            }
            out.push(v.get("result").cloned().unwrap_or(Value::Null));
        }
        if let Some(e) = first_err {
            return Err(e);
        }
        Ok(out)
    }

    /// Evaluate JS in the page and return the value as JSON.
    pub fn eval(&mut self, expression: &str) -> Res<Value> {
        let ms = self.timeout_ms;
        self.eval_within(expression, ms)
    }

    /// `eval` inside an attached iframe session.
    pub fn eval_in(&mut self, session: &str, expression: &str) -> Res<Value> {
        let r = self.call_in(Some(session), "Runtime.evaluate", json!({
            "expression": expression, "returnByValue": true, "awaitPromise": true, "userGesture": true,
        }))?;
        if let Some(details) = r.get("exceptionDetails") {
            let text = details.pointer("/exception/description").and_then(Value::as_str)
                .or_else(|| details.get("text").and_then(Value::as_str))
                .unwrap_or("evaluation threw");
            return Err(text.to_string());
        }
        Ok(r.pointer("/result/value").cloned().unwrap_or(Value::Null))
    }

    /// `eval` for an expression that awaits something in the page, with a
    /// reply deadline to match.
    pub fn eval_within(&mut self, expression: &str, ms: u64) -> Res<Value> {
        let params = json!({
            "expression": expression,
            "returnByValue": true,
            "awaitPromise": true,
            "userGesture": true,
        });
        let r = if ms == self.timeout_ms {
            self.call("Runtime.evaluate", params)?
        } else {
            self.call_within("Runtime.evaluate", params, ms)?
        };
        if let Some(details) = r.get("exceptionDetails") {
            let text = details
                .get("exception")
                .and_then(|e| e.get("description"))
                .and_then(Value::as_str)
                .or_else(|| details.get("text").and_then(Value::as_str))
                .unwrap_or("evaluation threw");
            return Err(text.to_string());
        }
        Ok(r.pointer("/result/value").cloned().unwrap_or(Value::Null))
    }
}

#[cfg(test)]
mod receipt_tests {
    use super::*;

    #[test]
    fn origins_normalise_ports_case_and_blob() {
        assert_eq!(origin_of("https://Example.COM:443/x"), "https://example.com");
        assert_eq!(origin_of("http://example.com:80/"), "http://example.com");
        assert_eq!(origin_of("http://example.com:8080/x"), "http://example.com:8080");
        assert_eq!(origin_of("blob:https://Example.com/uuid"), "https://example.com");
    }

    #[test]
    fn loopback_is_one_origin_across_ports() {
        let dev = Receipts::new(origin_of("http://127.0.0.1:5173/"), "T".into());
        assert!(dev.same_origin("http://127.0.0.1:3000/api"));
        assert!(dev.same_origin("http://localhost:3000/api"));
        assert!(!dev.same_origin("https://tracker.invalid/x"));
    }

    #[test]
    fn lookalike_loopback_hosts_are_not_loopback() {
        let dev = Receipts::new(origin_of("http://localhost:5173/"), "T".into());
        assert!(!dev.same_origin("https://localhost.evil.com/x"));
        assert!(!dev.same_origin("http://127.0.0.1.nip.io/x"));
    }

    #[test]
    fn an_opened_page_is_attributed_to_its_opener() {
        let mut r = Receipts::new("https://app.example.com".into(), "T1".into());
        r.note(&json!({"method":"Target.targetCreated","params":{"targetInfo":{
            "targetId":"T2","type":"page","openerId":"T1","url":"about:blank"}}}));
        assert_eq!(r.opened_tab.as_ref().unwrap()["tab"], "T2");
        r.note(&json!({"method":"Target.targetInfoChanged","params":{"targetInfo":{
            "targetId":"T2","url":"https://app.example.com/report"}}}));
        assert_eq!(r.opened_tab.as_ref().unwrap()["url"], "https://app.example.com/report");
        let before = r.opened_tab.clone();
        r.note(&json!({"method":"Target.targetCreated","params":{"targetInfo":{
            "targetId":"T3","type":"page","openerId":"OTHER","url":"https://x.example/"}}}));
        assert_eq!(r.opened_tab, before);
    }

    #[test]
    fn a_cross_origin_document_is_tracked_and_its_failure_reported() {
        let mut r = Receipts::new(origin_of("http://app.test/"), "T".into());
        r.note(&json!({"method":"Network.requestWillBeSent","params":{
            "requestId":"d1","type":"Document","request":{"url":"http://other.test/page"}}}));
        r.note(&json!({"method":"Network.responseReceived","params":{
            "requestId":"d1","type":"Document","response":{"status":500,"url":"http://other.test/page"}}}));
        assert!(r.entries.iter().any(|e| e["kind"] == "http" && e["status"] == 500),
                "{:?}", r.entries);
    }

    #[test]
    fn a_top_frame_commit_moves_the_origin_but_an_error_page_does_not() {
        let mut r = Receipts::new(origin_of("http://app.test/"), "T".into());
        r.note(&json!({"method":"Page.frameNavigated","params":{"frame":{"url":"http://other.test/x"}}}));
        r.note(&json!({"method":"Network.responseReceived","params":{
            "type":"Fetch","response":{"status":503,"url":"http://other.test/api"}}}));
        assert!(r.entries.iter().any(|e| e["status"] == 503),
                "the new origin must count: {:?}", r.entries);

        let mut r2 = Receipts::new(origin_of("http://app.test/"), "T".into());
        r2.note(&json!({"method":"Page.frameNavigated","params":{"frame":{"url":"chrome-error://chromewebdata/"}}}));
        r2.note(&json!({"method":"Network.responseReceived","params":{
            "type":"Fetch","response":{"status":503,"url":"http://other.test/api"}}}));
        assert!(!r2.entries.iter().any(|e| e["status"] == 503),
                "a failed commit must not move the origin: {:?}", r2.entries);
        assert_eq!(r2.origin, "http://app.test");
    }

    #[test]
    fn a_subframe_commit_does_not_move_the_origin() {
        let mut r = Receipts::new(origin_of("http://app.test/"), "T".into());
        r.note(&json!({"method":"Page.frameNavigated","params":{"frame":{
            "parentId":"p","url":"http://other.test/frame"}}}));
        assert_eq!(r.origin, "http://app.test");
    }

    #[test]
    fn a_download_is_reported_as_started_whatever_its_origin() {
        let mut r = Receipts::new(origin_of("http://app.test/"), "T".into());
        r.note(&json!({"method":"Page.downloadWillBegin","params":{
            "url":"https://cdn.test/file.zip?token=x","suggestedFilename":"file.zip","guid":"g1"}}));
        assert_eq!(r.downloads.len(), 1);
        assert_eq!(r.downloads[0]["status"], "started");
        assert_eq!(r.downloads[0]["filename"], "file.zip");
        assert_eq!(r.downloads[0]["guid"], "g1");
        assert!(r.entries.is_empty());
    }

    #[test]
    fn non_loopback_origins_are_strict() {
        let app = Receipts::new(origin_of("https://app.example.com/"), "T".into());
        assert!(app.same_origin("https://app.example.com:443/api"));
        assert!(!app.same_origin("https://api.example.com/x"));
        assert!(!app.same_origin("http://app.example.com/x"));
    }
}
