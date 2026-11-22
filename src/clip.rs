// The system clipboard: one backend table, one place that decides what this
// session can do. Linux has Wayland or X11, macOS has pbcopy/pbpaste, Windows
// has nothing here. Kept out of main.rs because none of it touches the browser.

use serde_json::{json, Value};

use crate::{current_os, data_dir, env_nonempty, Os};

/// The system clipboard's backend, chosen from the session and the tools on
/// PATH. Linux has two, macOS one, Windows none here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ClipBackend { Wayland, X11, Mac }

/// Is `program` on PATH? A plain scan: no dependency, and it answers the
/// question a user asks when the system clipboard stops working.
fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

fn clip_backend_for(
    os: Os,
    get: &dyn Fn(&str) -> Option<String>,
    has: &dyn Fn(&str) -> bool,
) -> Option<ClipBackend> {
    match os {
        Os::Mac => (has("pbcopy") && has("pbpaste")).then_some(ClipBackend::Mac),
        Os::Windows => None,
        Os::Linux => {
            // Wayland first: it is also the case where wl-clipboard is the only
            // tool that can see the human's selection.
            if env_nonempty(get, "WAYLAND_DISPLAY").is_some() && has("wl-copy") && has("wl-paste") {
                return Some(ClipBackend::Wayland);
            }
            if env_nonempty(get, "DISPLAY").is_some() && has("xclip") {
                return Some(ClipBackend::X11);
            }
            None
        }
    }
}

pub(crate) fn clip_backend() -> Option<ClipBackend> {
    clip_backend_for(current_os(), &|k| std::env::var(k).ok(), &which)
}

/// Why there is no backend, said in terms of the session that was detected
/// rather than always blaming wl-clipboard.
fn clip_unavailable_reason() -> String {
    let get = |k: &str| std::env::var(k).ok();
    match current_os() {
        Os::Mac => "no clipboard tools on PATH: this needs pbcopy and pbpaste".into(),
        Os::Windows => "the system clipboard has no Windows backend here; use method:\"page\"".into(),
        Os::Linux => {
            let wayland = env_nonempty(&get, "WAYLAND_DISPLAY").is_some();
            let x11 = env_nonempty(&get, "DISPLAY").is_some();
            match (wayland, x11) {
                (true, true) => "no clipboard tool available: install wl-clipboard (Wayland) or xclip (X11)".into(),
                (true, false) => "no clipboard tool available: install wl-clipboard for this Wayland session".into(),
                (false, true) => "no clipboard tool available: install xclip for this X11 session".into(),
                (false, false) => "no display session found (WAYLAND_DISPLAY and DISPLAY are both empty), so the system clipboard is out of reach".into(),
            }
        }
    }
}

struct ClipCmd {
    program: &'static str,
    args: Vec<String>,
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

fn is_text_mime(mime: &str) -> bool {
    mime.starts_with("text/") || matches!(mime, "UTF8_STRING" | "STRING")
}

/// What X11 should advertise for a MIME. `xclip` publishes exactly the atom it
/// was handed, and standard apps ask for `UTF8_STRING`; publishing only
/// `text/plain;charset=utf-8` makes the paste work in Chromium and nowhere else.
fn x11_target(mime: &str) -> &str {
    if mime.starts_with("text/plain") { "UTF8_STRING" } else { mime }
}

fn clip_types_cmd(b: ClipBackend) -> Option<ClipCmd> {
    match b {
        ClipBackend::Wayland => Some(ClipCmd { program: "wl-paste", args: args(&["--list-types"]) }),
        ClipBackend::X11 => Some(ClipCmd {
            program: "xclip",
            args: args(&["-selection", "clipboard", "-o", "-t", "TARGETS"]),
        }),
        // pbpaste cannot say what the pasteboard holds.
        ClipBackend::Mac => None,
    }
}

fn clip_read_cmd(b: ClipBackend, mime: &str) -> Result<ClipCmd, String> {
    match b {
        ClipBackend::Wayland => Ok(ClipCmd {
            program: "wl-paste",
            args: args(&["--type", mime, "--no-newline"]),
        }),
        ClipBackend::X11 => Ok(ClipCmd {
            program: "xclip",
            args: args(&["-selection", "clipboard", "-o", "-t", x11_target(mime)]),
        }),
        ClipBackend::Mac if is_text_mime(mime) => Ok(ClipCmd { program: "pbpaste", args: Vec::new() }),
        ClipBackend::Mac => Err(format!(
            "the macOS clipboard backend here is text-only; it cannot read {mime}"
        )),
    }
}

fn clip_write_cmd(b: ClipBackend, mime: &str) -> Result<ClipCmd, String> {
    match b {
        ClipBackend::Wayland => Ok(ClipCmd { program: "wl-copy", args: args(&["--type", mime]) }),
        ClipBackend::X11 => Ok(ClipCmd {
            program: "xclip",
            args: args(&["-selection", "clipboard", "-i", "-t", x11_target(mime)]),
        }),
        ClipBackend::Mac if is_text_mime(mime) => Ok(ClipCmd { program: "pbcopy", args: Vec::new() }),
        ClipBackend::Mac => Err(format!(
            "the macOS clipboard backend here is text-only: cannot put {mime} on it. \
             Use paste method:\"page\" for images"
        )),
    }
}

/// Run a clipboard command. `None` on any failure.
fn run_clip_cmd(cmd: &ClipCmd) -> Option<Vec<u8>> {
    std::process::Command::new(cmd.program)
        .args(&cmd.args)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| o.stdout)
}

fn clip_types(b: ClipBackend) -> Vec<String> {
    match clip_types_cmd(b) {
        Some(cmd) => run_clip_cmd(&cmd)
            .map(|out| String::from_utf8_lossy(&out).lines().map(str::to_string).collect())
            .unwrap_or_default(),
        None => Vec::new(),
    }
}

fn clip_read_from(b: ClipBackend, mime: &str) -> Option<Vec<u8>> {
    let cmd = clip_read_cmd(b, mime).ok()?;
    run_clip_cmd(&cmd)
}

pub(crate) fn clip_read(mime: &str) -> Option<Vec<u8>> {
    clip_read_from(clip_backend()?, mime)
}

/// Put bytes on the system clipboard. The error names what this session is
/// missing instead of always blaming wl-clipboard.
pub(crate) fn clip_write(mime: &str, bytes: &[u8]) -> Result<(), String> {
    let backend = clip_backend().ok_or_else(clip_unavailable_reason)?;
    let cmd = clip_write_cmd(backend, mime)?;
    // A file gives the writer a clean EOF. wl-copy forks and serves the
    // selection from its child, so a pipe we own is a lifetime race.
    let tmp = clip_temp("cdp-mcp-clip");
    clip_stage(&tmp, bytes).map_err(|e| format!("cannot stage the clipboard payload: {e}"))?;
    let file = std::fs::File::open(&tmp).map_err(|e| format!("cannot open {}: {e}", tmp.display()))?;
    // stdout and stderr null on purpose: wl-copy forks a child that serves the
    // selection, and that child would otherwise inherit this process's stdio and
    // hold it open after we exit (measured: the MCP client's pipe never closed).
    let status = std::process::Command::new(cmd.program)
        .args(&cmd.args)
        .stdin(std::process::Stdio::from(file))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let _ = std::fs::remove_file(&tmp);
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("{} exited with {s}", cmd.program)),
        Err(e) => Err(format!("cannot run {}: {e}", cmd.program)),
    }
}

/// Save the clipboard in the most useful representation available.
pub(crate) fn clip_save() -> Option<(String, Vec<u8>)> {
    let backend = clip_backend()?;
    let types = clip_types(backend);
    for want in ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "image/png"] {
        if types.iter().any(|t| t == want) {
            if let Some(bytes) = clip_read_from(backend, want) {
                if !bytes.is_empty() && bytes.len() < 32 * 1024 * 1024 {
                    return Some((want.to_string(), bytes));
                }
            }
        }
    }
    // A backend that cannot list what it holds (macOS pbpaste): try text. An
    // empty list from a backend that *can* list means an empty clipboard, and a
    // second read would just fail slowly.
    if clip_types_cmd(backend).is_none() {
        let want = "text/plain;charset=utf-8";
        if let Some(bytes) = clip_read_from(backend, want) {
            if !bytes.is_empty() && bytes.len() < 32 * 1024 * 1024 {
                return Some((want.to_string(), bytes));
            }
        }
    }
    None
}

/// The detached restore for method "system": runs after the paste and removes
/// the staged file whatever happens, whichever backend wrote it.
pub(crate) fn clip_restore_script(b: ClipBackend, mime: &str, tmp: &str) -> Result<String, String> {
    let cmd = clip_write_cmd(b, mime)?;
    let mut line = cmd.program.to_string();
    for a in &cmd.args {
        line.push(' ');
        line.push_str(&sh_quote(a));
    }
    Ok(format!("sleep 2; {} < {}; rm -f {}", line, sh_quote(tmp), sh_quote(tmp)))
}

/// Quote only where the shell needs it: flags and MIME types stay readable,
/// a path with a space is quoted.
fn sh_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '=' | '+' | '@' | '%')
        });
    if plain {
        s.to_string()
    } else {
        shell_quote(s)
    }
}

/// A fresh temp path per call: two clipboard writes inside the restore delay
/// must not share a file, or the first restore deletes the second payload.
pub(crate) fn clip_temp(prefix: &str) -> std::path::PathBuf {
    // The clock can be coarse, and two calls in the same tick would collide; the
    // counter makes uniqueness independent of the clock.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("{prefix}-{}-{nanos}-{seq}", std::process::id()))
}

/// Write clipboard bytes where only this user can read them.
pub(crate) fn clip_stage(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    use std::io::Write as _;
    opts.open(path)?.write_all(bytes)
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Append one entry to this profile's agent clipboard and keep the store small.
///
/// `clip`, `link` and `image` each opened the JSONL file themselves; two of them
/// never trimmed it, so the "clipboard, not an archive" cap only held for one
/// path. One helper means one behaviour, and the cap applies to all three.
pub(crate) fn clip_append(profile: &str, text: &str, label: Option<&str>) -> Result<(u64, usize), String> {
    let path = data_dir().join(format!("clip-{profile}.jsonl"));
    std::fs::create_dir_all(data_dir()).ok();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0) as u64;
    let rec = json!({ "id": ts, "ts": ts, "text": text, "label": label });
    {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        writeln!(f, "{rec}").map_err(|e| e.to_string())?;
    }
    let all: Vec<Value> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();
    if all.len() > 60 {
        let keep: Vec<String> = all.iter().rev().take(60).rev().map(|v| v.to_string()).collect();
        std::fs::write(&path, keep.join("\n") + "\n").map_err(|e| e.to_string())?;
    }
    Ok((ts, all.len().min(60)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clipboard_backend_follows_the_session() {
        let wayland = |k: &str| match k { "WAYLAND_DISPLAY" => Some("wayland-0".to_string()), _ => None };
        let both = |k: &str| match k {
            "WAYLAND_DISPLAY" => Some("wayland-0".to_string()),
            "DISPLAY" => Some(":0".to_string()),
            _ => None,
        };
        let x11 = |k: &str| match k { "DISPLAY" => Some(":0".to_string()), _ => None };
        let none = |_k: &str| None;
        let wl = |p: &str| p.starts_with("wl-");
        let xclip = |p: &str| p == "xclip";
        let nothing = |_p: &str| false;

        assert_eq!(clip_backend_for(Os::Linux, &wayland, &wl), Some(ClipBackend::Wayland));
        // Wayland advertised but wl-clipboard missing: X11 is the fallback.
        assert_eq!(clip_backend_for(Os::Linux, &both, &xclip), Some(ClipBackend::X11));
        assert_eq!(clip_backend_for(Os::Linux, &x11, &xclip), Some(ClipBackend::X11));
        assert_eq!(clip_backend_for(Os::Linux, &x11, &nothing), None);
        assert_eq!(clip_backend_for(Os::Linux, &none, &wl), None);
        assert_eq!(clip_backend_for(Os::Mac, &none, &|_p: &str| true), Some(ClipBackend::Mac));
        assert_eq!(clip_backend_for(Os::Mac, &none, &nothing), None);
        assert_eq!(clip_backend_for(Os::Windows, &none, &|_p: &str| true), None);
    }

    #[test]
    fn clipboard_commands_match_the_backend() {
        let w = clip_write_cmd(ClipBackend::Wayland, "image/png").unwrap();
        assert_eq!(w.program, "wl-copy");
        assert_eq!(w.args, ["--type", "image/png"]);

        // xclip publishes only the atom it is given, and standard X11 apps ask
        // for UTF8_STRING, so text/plain* is advertised under that name.
        let x = clip_write_cmd(ClipBackend::X11, "text/plain").unwrap();
        assert_eq!(x.program, "xclip");
        assert_eq!(x.args, ["-selection", "clipboard", "-i", "-t", "UTF8_STRING"]);
        let xi = clip_write_cmd(ClipBackend::X11, "image/png").unwrap();
        assert_eq!(xi.args, ["-selection", "clipboard", "-i", "-t", "image/png"]);

        let m = clip_write_cmd(ClipBackend::Mac, "text/plain;charset=utf-8").unwrap();
        assert_eq!(m.program, "pbcopy");
        assert!(m.args.is_empty());
        assert!(clip_write_cmd(ClipBackend::Mac, "image/png").is_err());

        let r = clip_read_cmd(ClipBackend::Wayland, "text/plain").unwrap();
        assert_eq!(r.program, "wl-paste");
        assert_eq!(r.args, ["--type", "text/plain", "--no-newline"]);

        let rx = clip_read_cmd(ClipBackend::X11, "text/plain;charset=utf-8").unwrap();
        assert_eq!(rx.args, ["-selection", "clipboard", "-o", "-t", "UTF8_STRING"]);

        assert_eq!(clip_read_cmd(ClipBackend::Mac, "text/plain").unwrap().program, "pbpaste");
        assert!(clip_read_cmd(ClipBackend::Mac, "image/png").is_err());

        // pbpaste cannot list types, so macOS has no list command at all.
        assert!(clip_types_cmd(ClipBackend::Mac).is_none());
        assert_eq!(clip_types_cmd(ClipBackend::X11).unwrap().args, ["-selection", "clipboard", "-o", "-t", "TARGETS"]);
    }

    #[test]
    fn the_restore_script_quotes_its_arguments_and_cleans_up() {
        let s = clip_restore_script(ClipBackend::Wayland, "text/plain", "/tmp/a b").unwrap();
        assert!(s.starts_with("sleep 2; wl-copy --type text/plain < '/tmp/a b'; rm -f '/tmp/a b'"), "{s}");

        let m = clip_restore_script(ClipBackend::Mac, "text/plain;charset=utf-8", "/tmp/x").unwrap();
        assert!(m.starts_with("sleep 2; pbcopy < /tmp/x; rm -f /tmp/x"), "{m}");

        let x = clip_restore_script(ClipBackend::X11, "text/plain;charset=utf-8", "/tmp/y").unwrap();
        assert!(x.contains("xclip -selection clipboard -i -t UTF8_STRING < /tmp/y"), "{x}");

        assert!(clip_restore_script(ClipBackend::Mac, "image/png", "/tmp/x").is_err());
    }

    #[test]
    fn clipboard_staging_is_unique_and_private() {
        let a = clip_temp("cdp-mcp-test");
        let b = clip_temp("cdp-mcp-test");
        assert_ne!(a, b, "two writes inside the restore delay must not share a file");
        clip_stage(&a, b"secret").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&a).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "clipboard bytes must not be world-readable");
        }
        std::fs::remove_file(&a).ok();
    }
}
