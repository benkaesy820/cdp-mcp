//! Transport: a minimal HTTP client for Chrome's `/json/*` endpoints and a
//! websocket connection for the DevTools protocol itself.
//!
//! Deliberately hand-rolled: the HTTP surface we need is three GET/PUT calls
//! with no body, so pulling an HTTP stack in would cost more than it saves.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde_json::{json, Value};
use tungstenite::{connect, stream::MaybeTlsStream, Message, WebSocket};

pub type Res<T> = Result<T, String>;

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

/// Create a page tab without taking the keyboard away from the human.
///
/// `/json/new` activates the tab it creates, which raises the browser window:
/// measured here, focus went from the chat window to Helium on every open, so a
/// human typing mid-run lost their keystrokes. `Target.createTarget` with
/// `background: true` creates the tab and leaves the focused window alone.
/// `foreground: true` is the deliberate exception (show this to the human).
pub fn new_tab(endpoint: &str, url: &str, foreground: bool) -> Res<String> {
    let version = http_json(endpoint, "/json/version", "GET")?;
    let ws_url = version
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .ok_or("the browser exposed no DevTools socket")?
        .to_string();
    let mut conn = Conn::open(&ws_url)?;
    let res = conn.call(
        "Target.createTarget",
        json!({ "url": url, "background": !foreground }),
    )?;
    let id = res
        .get("targetId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if id.is_empty() {
        return Err(format!("the browser created no target for {url}"));
    }
    // The target exists before `/json/list` reports it, and the very next call
    // resolves it by id: wait the few milliseconds instead of failing with
    // "tab not found" on a tab the browser has already opened.
    for _ in 0..50 {
        if targets(endpoint)?.iter().any(|t| t.id == id) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
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
pub fn close_tab(endpoint: &str, id: &str) -> Res<()> {
    let version = http_json(endpoint, "/json/version", "GET")?;
    let ws_url = version
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .ok_or("the browser exposed no DevTools socket")?
        .to_string();
    let mut conn = Conn::open(&ws_url)?;
    conn.call("Target.closeTarget", json!({ "targetId": id }))?;
    if let Ok(mut owned) = OWNED.lock() {
        owned.remove(id);
    }
    Ok(())
}

/// An open DevTools connection to one page.
pub struct Conn {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    next: i64,
    focus_on: bool,
    timeout_ms: u64,
}

impl Conn {
    pub fn open(ws_url: &str) -> Res<Self> {
        let (ws, _resp) = connect(ws_url).map_err(|e| format!("websocket handshake failed: {e}"))?;
        // Bounded so a stuck page cannot hang an agent that was left alone.
        // Configurable per profile with `call_timeout_ms`.
        let ms = std::env::var("CDP_MCP_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(30_000);
        let mut conn = Conn { ws, next: 0, focus_on: false, timeout_ms: ms };
        // a page that never answers must not hang the server forever
        conn.set_read_timeout(ms);
        // Deliberately NOT calling Runtime.enable / Page.enable: those are
        // per-session state for *events*, which this server ignores. Every
        // command used here works without them, and each enable costs a round
        // trip on connect.
        Ok(conn)
    }

    fn set_read_timeout(&mut self, ms: u64) {
        if let MaybeTlsStream::Plain(s) = self.ws.get_mut() {
            s.set_read_timeout(Some(Duration::from_millis(ms))).ok();
        }
    }

    /// Read until a named protocol event arrives, skipping everything else.
    ///
    /// Only safe when no other call is in flight: replies are skipped, not
    /// queued, so this is for a deliberate one-event wait.
    pub fn wait_event(&mut self, method: &str, ms: u64) -> Res<Value> {
        let deadline = std::time::Instant::now() + Duration::from_millis(ms);
        self.set_read_timeout(ms);
        let out = loop {
            if std::time::Instant::now() >= deadline {
                break Err(format!("no {method} within {ms} ms"));
            }
            let m = match self.ws.read() {
                Ok(m) => m,
                Err(e) => break Err(format!("read failed: {e}")),
            };
            let text = match m {
                Message::Text(t) => t.to_string(),
                Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
                Message::Ping(p) => {
                    let _ = self.ws.send(Message::Pong(p));
                    continue;
                }
                _ => continue,
            };
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if v.get("method").and_then(Value::as_str) == Some(method) {
                    break Ok(v.get("params").cloned().unwrap_or(Value::Null));
                }
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
        self.next += 1;
        let id = self.next;
        let msg = json!({ "id": id, "method": method, "params": params }).to_string();
        self.ws
            .send(Message::text(msg))
            .map_err(|e| format!("send failed: {e}"))?;

        loop {
            let m = self.ws.read().map_err(|e| format!("read failed: {e}"))?;
            let text = match m {
                Message::Text(t) => t.to_string(),
                Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
                Message::Ping(p) => {
                    let _ = self.ws.send(Message::Pong(p));
                    continue;
                }
                Message::Close(_) => return Err("the page closed the connection".into()),
                _ => continue,
            };
            let v: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v.get("id").and_then(Value::as_i64) != Some(id) {
                continue; // an event, not our reply
            }
            if let Some(err) = v.get("error") {
                let m = err.get("message").and_then(Value::as_str).unwrap_or("unknown error");
                return Err(format!("{method}: {m}"));
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Send several commands at once and collect their replies in order.
    /// One round trip instead of one per command: a click is three events, and
    /// waiting for each reply cost more than the events themselves.
    pub fn call_many(&mut self, cmds: &[(&str, Value)]) -> Res<Vec<Value>> {
        let mut ids = Vec::with_capacity(cmds.len());
        for (method, params) in cmds {
            self.next += 1;
            let id = self.next;
            ids.push(id);
            let msg = json!({ "id": id, "method": method, "params": params }).to_string();
            self.ws.send(Message::text(msg)).map_err(|e| format!("send failed: {e}"))?;
        }
        let mut got: std::collections::HashMap<i64, Value> = std::collections::HashMap::new();
        while got.len() < ids.len() {
            let m = self.ws.read().map_err(|e| format!("read failed: {e}"))?;
            let text = match m {
                Message::Text(t) => t.to_string(),
                Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
                Message::Ping(pl) => { let _ = self.ws.send(Message::Pong(pl)); continue; }
                Message::Close(_) => return Err("the page closed the connection".into()),
                _ => continue,
            };
            let v: Value = match serde_json::from_str(&text) { Ok(v) => v, Err(_) => continue };
            let Some(id) = v.get("id").and_then(Value::as_i64) else { continue };
            if !ids.contains(&id) { continue; }
            if let Some(err) = v.get("error") {
                let m = err.get("message").and_then(Value::as_str).unwrap_or("unknown error");
                return Err(format!("batched call: {m}"));
            }
            got.insert(id, v.get("result").cloned().unwrap_or(Value::Null));
        }
        Ok(ids.iter().map(|i| got.get(i).cloned().unwrap_or(Value::Null)).collect())
    }

    /// Evaluate JS in the page and return the value as JSON.
    pub fn eval(&mut self, expression: &str) -> Res<Value> {
        let r = self.call(
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": true,
                "userGesture": true,
            }),
        )?;
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

    pub fn text(&mut self) -> Res<String> {
        let v = self.eval("(document.body ? document.body.innerText : \"\")")?;
        Ok(v.as_str().unwrap_or_default().to_string())
    }
}
