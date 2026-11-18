# cdp-mcp

A tiny MCP server that drives **your own** Chromium browser over the DevTools protocol.

No extension. No relay. No second browser, no fresh profile — it attaches to the
browser you are already using, so every login you have is already there when an
agent starts working.

**534 KB, 44 crates, ~1 ms to initialize, no async runtime.**

---

## Why another browser MCP

Three decisions, each of which fixes something that made the alternatives
unpleasant to use:

**It attaches to a running browser.** Playwright-style tools launch their own
Chromium with an empty profile, which means no logins, and a login is usually
the whole task. This connects to a browser you start normally.

**Real input events.** Clicks and keystrokes go through `Input.dispatchMouseEvent`
and `Input.insertText`, not `element.click()`. Rich editors — React, Draft,
anything contenteditable — ignore synthetic DOM events and drop line breaks or
duplicate text. Real events are accepted because they *are* real events.

**No references to hold.** `elements` returns every visible interactive element
with its label and clickable centre. `input {click:{i}}` re-resolves that index
at click time — scrolling it into view and re-measuring — so a stale list can
never click the wrong thing. There is no `tab` id to thread through every call
either: omit it and you get the active page.

## Requirements

A Chromium-family browser started with a DevTools port:

```sh
# Linux (Helium, Chrome, Chromium, Brave, Edge, Vivaldi…)
helium-browser --remote-debugging-port=9222
google-chrome --remote-debugging-port=9222

# macOS
/Applications/Google\ Chrome.app/Contents/MacOS/Google\ Chrome --remote-debugging-port=9222

# Windows
chrome.exe --remote-debugging-port=9222
```

To make that permanent from your launcher, add the flag to a **user-level**
`.desktop` override on Linux (copy `/usr/share/applications/<browser>.desktop`
into `~/.local/share/applications/` and edit its `Exec=` line). Do not edit the
system file.

## Install

**Prebuilt (recommended).** One static binary, no runtime:

```sh
# Linux / macOS
curl -LsSf https://github.com/benkaesy/cdp-mcp/releases/latest/download/cdp-mcp-installer.sh | sh

# Windows (PowerShell)
irm https://github.com/benkaesy/cdp-mcp/releases/latest/download/cdp-mcp-installer.ps1 | iex
```

**With Cargo** (Rust 1.88+):

```sh
cargo install cdp-mcp
```

**From source**, if you want to read it first:

```sh
git clone https://github.com/benkaesy/cdp-mcp && cd cdp-mcp
cargo build --release      # target/release/cdp-mcp
```

That is the whole install. One binary, no runtime, nothing to keep updated.

### What is verified, and what is not

| platform | status |
|---|---|
| **Linux x86_64 — Arch, Debian/Ubuntu, Fedora, Alpine** | **one static binary, used daily; everything below measured here** |
| Linux aarch64 | builds in CI; not run by hand |
| macOS | builds in CI; **never run** |
| Windows | builds in CI; **never run** |

### Why one Linux binary covers every distro

The release builds for `x86_64-unknown-linux-musl`. Measured on the machine this
was developed on: `file` reports **"statically linked"**, `objdump` finds **zero
`GLIBC_` symbols**, and the static build drives the browser. A normal glibc build
required `GLIBC_2.39` and would refuse to start on Debian 12 (2.36) or Ubuntu
22.04 (2.35) — the usual reason a Rust binary "only works on Arch".

So: **no distro package needed.** The same file runs on Arch, Debian, Ubuntu,
Fedora and Alpine. A `.deb`/`.rpm`/AUR package would only add a download path,
not compatibility.

The tool speaks a protocol Chromium implements identically everywhere, but the
clipboard features (`paste`) are **Wayland-only** (they shell out to `wl-copy`)
and report a clear error elsewhere. Say so rather than guess when you file an
issue.

## Configure

Any MCP client that speaks stdio. OpenCode (`~/.config/opencode/opencode.json`):

```json
{
  "mcp": {
    "servers": {
      "cdp": { "type": "local", "command": ["/home/you/.local/bin/cdp-mcp"] }
    }
  }
}
```

Claude Desktop (`claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "cdp": { "command": "/home/you/.local/bin/cdp-mcp", "args": [] }
  }
}
```

Options:

| Flag | Meaning |
|---|---|
| `--cdp URL` | endpoint, default `http://127.0.0.1:9222` |
| `--allow-remote` | permit a non-loopback endpoint (see Security) |
| `-h`, `--help` / `-V`, `--version` | |

## Tools

| Tool | What it does |
|---|---|
| `status` | browser version and every open page tab |
| `tabs` | `{action:"list"}` · `{action:"open",url}` (waits for the document) · `{action:"close",tab}` |
| `eval` | evaluate a JavaScript expression in the page, return JSON, awaits promises |
| `elements` | visible interactive elements with `{i, tag, role, label, x, y, w, h}` |
| `input` | real input, in order, in one call |
| `shot` | PNG screenshot to a path |

`input` actions:

```json
[
  { "click": { "i": 7 } },                    // index from elements, re-resolved
  { "click": { "x": 400, "y": 320 } },        // or raw coordinates
  { "insert": "text, typed like a keyboard"  },
  { "key": "Enter" },                          // Enter Escape Tab Backspace Arrows
  { "scroll": { "dy": 600 } },
  { "wait": { "text": "Saved", "ms": 15000 } },
  { "sleep": { "ms": 300 } }
]
```

A typical flow — read, act, verify, in three calls:

```jsonc
// 1. what is on the page
{ "name": "elements", "arguments": {} }

// 2. do several things at once
{ "name": "input", "arguments": { "actions": [
    { "click": { "i": 12 } },
    { "insert": "Hello" },
    { "key": "Enter" }
] } }

// 3. prove it worked
{ "name": "eval", "arguments": { "expression": "document.body.innerText.includes('Hello')" } }
```

## Security

**CDP has no authentication.** Anything that can reach the port controls the
browser completely — every cookie, every session, everything open.

- The default endpoint is loopback, and a non-loopback endpoint is **refused**
  unless you pass `--allow-remote`.
- Never expose the port: no tunnel route, no reverse proxy, no
  `--remote-debugging-address=0.0.0.0`. If you are using Cloudflare Tunnel or
  ngrok, do not add a route to `127.0.0.1:9222`.
- Close the browser when you are done if the machine is shared, or start it
  without the flag when you are not automating.

## Limits

- Chromium only. Firefox is not supported (different protocol).
- No accessibility tree; `elements` is a flat list of interactive nodes.
- No network interception or tracing. CDP can do both; this exposes neither.
- One request at a time per call — deliberately, so there is no async runtime.

## Licence

MIT.
