//! A portable chat surface, served by the tool itself.
//!
//! Why this shape: a chat window that has to *impersonate* the agent (session
//! id, credential file, wake script) is a copy of the client with extra failure
//! modes. This is a queue and nothing else — the agent drains it by waiting, the
//! page reads it over HTTP, and no secret is involved anywhere.
//!
//! One consequence, stated plainly: a message sent while the agent is not
//! waiting is *stored*, not delivered, and is answered the next time it waits.
//! That is the cost of having no credentials, and it is the right trade: it
//! works from any machine, any browser, any OS.
//!
//! Everything is `std`: a thread per connection, blocking reads, a request cap.
//! The crate stays dependency-free for this feature.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const MAX_REQUEST: usize = 64 * 1024;

/// Linux inotify, declared directly.
///
/// The alternative was a loop asking "has the file changed yet", which is the
/// polling this design exists to avoid. The kernel already knows when a file is
/// written; these four calls let it say so. Declaring them is smaller than
/// taking a dependency for the same thing.
mod notify {
    use std::os::fd::{FromRawFd, RawFd};

    #[repr(C)]
    struct PollFd {
        fd: RawFd,
        events: i16,
        revents: i16,
    }

    const POLLIN: i16 = 0x001;
    const IN_MODIFY: u32 = 0x0000_0002;
    const IN_CLOSE_WRITE: u32 = 0x0000_0008;
    const IN_MOVED_TO: u32 = 0x0000_0080;
    const IN_CREATE: u32 = 0x0000_0100;
    const IN_DELETE: u32 = 0x0000_0200;
    const IN_ATTRIB: u32 = 0x0000_0004;

    extern "C" {
        fn inotify_init1(flags: i32) -> i32;
        fn inotify_add_watch(fd: i32, path: *const i8, mask: u32) -> i32;
        fn poll(fds: *mut PollFd, nfds: u64, timeout: i32) -> i32;
    }

    /// A watch on one file. Falls back to a slow stat loop where inotify is
    /// unavailable, so the page still updates rather than silently going stale.
    pub struct Watch {
        fd: RawFd,
        fallback: Option<(std::path::PathBuf, std::time::SystemTime, u64)>,
    }

    impl Watch {
        pub fn file(path: &std::path::Path) -> Self {
            let c = std::ffi::CString::new(path.to_string_lossy().as_bytes()).unwrap_or_default();
            let fd = unsafe { inotify_init1(0) };
            if fd < 0 {
                let stamp = std::fs::metadata(path).and_then(|m| m.modified()).ok();
                let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                return Watch { fd: -1, fallback: Some((path.to_path_buf(), stamp.unwrap_or(std::time::UNIX_EPOCH), len)) };
            }
            // A missing file is normal before the first message: watch the
            // directory's create events too by re-adding on demand.
            unsafe { inotify_add_watch(fd, c.as_ptr(), IN_MODIFY | IN_CLOSE_WRITE | IN_MOVED_TO | IN_CREATE | IN_DELETE | IN_ATTRIB) };
            Watch { fd, fallback: None }
        }

        /// Block until something changes, or `timeout_ms` passes.
        ///
        /// Returns true when the file changed. A generous timeout is used as a
        /// heartbeat, so an abandoned client is noticed without a poll loop.
        pub fn wait(&mut self, path: &std::path::Path, timeout_ms: i32) -> bool {
            if self.fd >= 0 {
                let mut pfd = PollFd { fd: self.fd, events: POLLIN, revents: 0 };
                let n = unsafe { poll(&mut pfd, 1, timeout_ms) };
                if n > 0 {
                    // Drain the queue so the next wait blocks again.
                    let mut file = unsafe { std::fs::File::from_raw_fd(self.fd) };
                    let mut buf = [0u8; 4096];
                    use std::io::Read;
                    let _ = file.read(&mut buf);
                    std::mem::forget(file);
                    return true;
                }
                return false;
            }
            // Fallback: one stat every timeout_ms. Far cheaper than an HTTP
            // request, and only reached when inotify is missing.
            std::thread::sleep(std::time::Duration::from_millis(timeout_ms.max(200) as u64));
            if let Some((p, stamp, len)) = &mut self.fallback {
                let now = std::fs::metadata(&*p).and_then(|m| m.modified()).ok();
                let n = std::fs::metadata(&*p).map(|m| m.len()).unwrap_or(*len);
                if now.unwrap_or(std::time::UNIX_EPOCH) != *stamp || n != *len {
                    *stamp = now.unwrap_or(std::time::UNIX_EPOCH);
                    *len = n;
                    let _ = path;
                    return true;
                }
            }
            false
        }
    }

    impl Drop for Watch {
        fn drop(&mut self) {
            if self.fd >= 0 {
                let file = unsafe { std::fs::File::from_raw_fd(self.fd) };
                drop(file);
            }
        }
    }
}

/// Stream the conversation to one browser over Server-Sent Events.
///
/// One connection, held open, written only when something actually changes. The
/// browser's `EventSource` reconnects by itself if the socket drops, so the page
/// needs no retry logic and no timer.
fn stream_events(store: &Store, stream: &mut TcpStream) -> std::io::Result<()> {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                Cache-Control: no-store\r\nConnection: keep-alive\r\n\
                X-Accel-Buffering: no\r\n\r\n";
    stream.write_all(head.as_bytes())?;
    stream.flush()?;

    let chat = store.dir.join("chat.jsonl");
    let talk = store.dir.join("talk.jsonl");
    let mut watch = notify::Watch::file(&chat);

    let send_now = |stream: &mut TcpStream, store: &Store| -> std::io::Result<()> {
        let payload = json!({
            "unread": store.unread(),
            "cursor": store.consumed(),
            "size": store.size(),
            "messages": store.conversation(),
        });
        write!(stream, "data: {payload}\n\n")?;
        stream.flush()
    };

    send_now(stream, store)?;
    loop {
        // 20 s of silence means "say something" so a dead socket is discovered
        // by the write failing, without the page having to ask.
        let changed = watch.wait(&chat, 20_000);
        if !store.dir.join("talk.jsonl").exists() {
            let _ = talk;
        }
        // Woken by a change, or by the heartbeat timeout. Either way the state is
        // re-read and sent: on a change that is the point, and on a timeout it
        // proves the socket is still alive without the page asking.
        if send_now(stream, store).is_err() {
            return Ok(());
        }
        if !changed && (write!(stream, ": keepalive\n\n").is_err() || stream.flush().is_err()) {
            return Ok(());
        }
    }
}

/// Where the conversation lives. The same files the agent reads, so there is no
/// second source of truth to keep in step.
pub struct Store {
    pub dir: PathBuf,
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn chat(&self) -> PathBuf {
        self.dir.join("chat.jsonl")
    }

    fn talk(&self) -> PathBuf {
        self.dir.join("talk.jsonl")
    }

    fn cursor(&self) -> PathBuf {
        self.dir.join("chat.cursor")
    }

    fn read_jsonl(path: &PathBuf) -> Vec<Value> {
        let body = match std::fs::read_to_string(path) {
            Ok(b) => b,
            Err(_) => return Vec::new(),
        };
        body.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .collect()
    }

    /// The whole exchange, oldest first, with a stable id per line.
    pub fn conversation(&self) -> Vec<Value> {
        let mut all: Vec<(u64, Value)> = Vec::new();
        for (path, who) in [(self.chat(), "human"), (self.talk(), "agent")] {
            let offset = if path == self.talk() { self.chat().metadata().map(|m| m.len()).unwrap_or(0) } else { 0 };
            let _ = offset;
            for (n, mut v) in Self::read_jsonl(&path).into_iter().enumerate() {
                if v.get("from").is_none() {
                    v["from"] = json!(who);
                }
                let ts = v.get("ts").and_then(Value::as_u64).unwrap_or(0);
                let ts = if ts < 100_000_000_000 { ts * 1000 } else { ts };
                all.push((ts + n as u64, v));
            }
        }
        all.sort_by_key(|(k, _)| *k);
        all.into_iter().map(|(_, v)| v).collect()
    }

    /// How much of the queue the agent has already taken.
    pub fn consumed(&self) -> u64 {
        std::fs::read_to_string(self.cursor())
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0)
    }

    pub fn size(&self) -> u64 {
        self.chat().metadata().map(|m| m.len()).unwrap_or(0)
    }

    /// True when there is something the agent has not read yet.
    pub fn unread(&self) -> bool {
        self.size() > self.consumed()
    }

    pub fn append_human(&self, text: &str) -> Result<Value, String> {
        use std::io::Write as _;
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("cannot create data dir: {e}"))?;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let rec = json!({ "id": ts, "ts": ts, "from": "human", "text": text });
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.chat())
            .map_err(|e| format!("cannot write the queue: {e}"))?;
        writeln!(f, "{rec}").map_err(|e| format!("cannot write the queue: {e}"))?;
        Ok(rec)
    }

    /// Clear both sides and the agent's read position.
    ///
    /// The cursor is part of the state: leaving it pointing into a deleted file
    /// makes the agent read from the wrong place, which is worse than keeping
    /// the history.
    pub fn clear(&self) -> Result<String, String> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let archive = self.dir.join(format!("conversation-{stamp}.jsonl"));
        let all = self.conversation();
        if !all.is_empty() {
            let body: String = all.iter().map(|v| format!("{v}\n")).collect();
            std::fs::write(&archive, body).map_err(|e| format!("cannot archive: {e}"))?;
        }
        for p in [self.chat(), self.talk(), self.cursor()] {
            let _ = std::fs::remove_file(&p);
        }
        Ok(archive.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default())
    }
}

/// One HTTP response, kept boring on purpose.
fn respond(stream: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) {
    respond_keep(stream, status, ctype, body, false)
}

/// `keep` leaves the socket open for the next request on it.
///
/// A page that polls every second otherwise pays a TCP handshake and a thread
/// per poll; on loopback the handshake is cheap but the thread is not. The
/// caller decides, because the last response before a close must not advertise
/// keep-alive.
fn respond_keep(stream: &mut TcpStream, status: &str, ctype: &str, body: &[u8], keep: bool) {
    let connection = if keep { "keep-alive" } else { "close" };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: {connection}\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn request(stream: &mut TcpStream) -> Option<(String, String, String)> {
    // Short: an idle keep-alive socket returns the thread quickly, and a slow
    // client is not worth protecting from itself on loopback.
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_REQUEST {
            return None;
        }
        // Headers are done, and the body (if any) is complete per Content-Length.
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).to_string();
            let len = head
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                .and_then(|l| l.split(':').nth(1))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if buf.len() >= pos + 4 + len {
                let mut parts = head.lines().next().unwrap_or("").split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let target = parts.next().unwrap_or("/").to_string();
                let body = String::from_utf8_lossy(&buf[pos + 4..pos + 4 + len]).to_string();
                return Some((method, target, body));
            }
        }
    }
    None
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Serve until the process is told to stop.
///
/// Bound to loopback only: this is a local control surface, and it has no
/// authentication, exactly like CDP itself. Port 0 picks a free port and the
/// chosen one is printed, so two profiles never fight over a number.
pub fn serve(store: Store, port: u16) -> Result<(), String> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| format!("cannot bind 127.0.0.1:{port}: {e}"))?;
    let actual = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    println!("chat page: http://127.0.0.1:{actual}/");

    let dir = store.dir.clone();
    for conn in listener.incoming() {
        let mut stream = match conn {
            Ok(s) => s,
            Err(_) => continue,
        };
        // One thread per connection, because an event stream is held open for
        // as long as the page is: serving it inline in the listener would stop
        // every other client, and a `return` in a handler would end the server.
        let dir = dir.clone();
        std::thread::spawn(move || {
        let store = Store::new(dir);
        let started = Instant::now();
        stream.set_nodelay(true).ok();
        loop {
        let Some((method, target, body)) = request(&mut stream) else {
            break;
        };
        let path = target.split('?').next().unwrap_or("/");
        let mut keep = true;
        if let Some(conn) = target.split("keep=").nth(1) {
            keep = !conn.starts_with("0");
        }

        match (method.as_str(), path) {
            ("GET", "/") => respond_keep(&mut stream, "200 OK", "text/html; charset=utf-8", page().as_bytes(), keep),
            ("GET", "/api/events") => {
                // The whole point: one held-open connection, pushed on change.
                let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
                let _ = stream_events(&store, &mut stream);
                // This socket is done: a stream is not reused for another
                // request. Break out of the per-connection loop only.
                break;
            }
            ("GET", "/api/bundle") => {
                // One request for everything the page draws. It always needs
                // both halves together, so asking twice doubled the connection
                // count for no benefit: measured 2 new TCP connections every
                // 1.2 s before this.
                let unread = store.unread();
                let payload = json!({
                    "unread": unread,
                    "cursor": store.consumed(),
                    "size": store.size(),
                    "waiting": !unread,
                    "messages": store.conversation(),
                });
                respond_keep(&mut stream, "200 OK", "application/json", payload.to_string().as_bytes(), keep);
            }
            ("GET", "/api/state") => {
                let unread = store.unread();
                let payload = json!({
                    "unread": unread,
                    "cursor": store.consumed(),
                    "size": store.size(),
                    // Honest, not a heartbeat: the page can only know what the
                    // files say. `waiting` means there is nothing for the agent
                    // to read yet, not that it is definitely blocked.
                    "waiting": !unread,
                    "uptimeMs": started.elapsed().as_millis() as u64,
                });
                respond_keep(&mut stream, "200 OK", "application/json", payload.to_string().as_bytes(), keep);
            }
            ("GET", "/api/messages") => {
                let payload = json!({ "messages": store.conversation() });
                respond_keep(&mut stream, "200 OK", "application/json", payload.to_string().as_bytes(), keep);
            }
            ("POST", "/api/send") => {
                let text = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| v.get("text").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_default();
                let text = text.trim().to_string();
                if text.is_empty() {
                    respond(&mut stream, "400 Bad Request", "application/json",
                             br#"{"error":"empty message"}"#);
                    continue;
                }
                match store.append_human(&text) {
                    Ok(rec) => respond(&mut stream, "200 OK", "application/json",
                                       json!({ "stored": rec, "queued": true }).to_string().as_bytes()),
                    Err(e) => respond(&mut stream, "500 Internal Server Error", "application/json",
                                      json!({ "error": e }).to_string().as_bytes()),
                }
            }
            ("POST", "/api/clear") => match store.clear() {
                Ok(archive) => respond(&mut stream, "200 OK", "application/json",
                                       json!({ "cleared": true, "archive": archive }).to_string().as_bytes()),
                Err(e) => respond(&mut stream, "500 Internal Server Error", "application/json",
                                  json!({ "error": e }).to_string().as_bytes()),
            },
            _ => respond(&mut stream, "404 Not Found", "text/plain", b"not found"),
        }
        if !keep {
            break;
        }
        } // per-connection request loop
        }); // per-connection thread
    }
    Ok(())
}

/// The page. One file, no build step, no webfont, no framework: it has to work
/// on a phone, on Windows, and offline.
fn page() -> &'static str {
    include_str!("webchat.html")
}
