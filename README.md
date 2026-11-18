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

Eighteen tools. Every one resolves its own tab, so nothing has to be called
before anything else.

| Tool | What it does |
|---|---|
| `status` | browser version, every open page tab, and a doctor result when CDP is down |
| `tabs` | `list` · `open {url}` (background, so it never steals focus) · `close {tab}` |
| `eval` | evaluate JavaScript in the page, return JSON, awaits promises |
| `elements` | visible interactive elements with `{i, tag, role, label, x, y, w, h}` — **inside shadow roots, same-origin frames and cross-origin frames too**; plus `overlays`, `media`, `scroll` |
| `input` | real input, in order, in one call |
| `flow` | run steps server-side and repeat until a condition holds — N acts for one turn |
| `idle` | wait with no polling and no cost while it waits: returns the moment the human types, `untilText` appears, or the page stops changing |
| `images` | every image on the page with its source and size |
| `image` | `open` / `link` / `copy` — copy puts real pixels on the clipboard for a real Ctrl+V |
| `link` | `open` a link (or the current page) in a tab, or `copy` its URL |
| `clip` | agent-owned clipboard: `set` `get` `list` `clear`, and `toSystem` as the one opt-in |
| `download` | save a resource to a path, including cross-origin ones |
| `upload` | set a file on an `<input type=file>` |
| `paste` | type text, or paste an image, into the focused element |
| `shot` | screenshot to a path — opt-in, because an image costs context |
| `report` | file an issue or an idea with version, profile and page attached |
| `profile` | per-agent settings and a capped memory of what was done |
| `batch` | several tool calls in one request |

`elements` indexes are **absolute and re-resolved at click time**, so they cannot
go stale between the scan and the click: it scrolls the element into view,
re-measures, and hit-tests what is actually on top. `within` scopes the scan to a
subtree; the indexes it returns stay valid for `input`.

### `input` actions

```json
[
  { "click": { "i": 7 } },                          // index from elements
  { "click": { "x": 400, "y": 320 } },              // or raw coordinates
  { "click": { "i": 3, "button": "right" } },        // right / middle, count:2, modifiers
  { "hover": { "i": 4 } },
  { "drag": { "from": { "i": 2 }, "to": { "x": 500 } }, "steps": 12 },
  { "tap": { "i": 5 } }, { "swipe": { "from": {...}, "to": {...} } },   // touch
  { "select": { "i": 6, "value": "two" } },
  { "insert": "text, typed like a keyboard" },
  { "key": "Control+a" },                           // chords, Escape, Tab, arrows
  { "scroll": { "dy": 600 } },                      // reports whether it moved
  { "dismiss": { "prefer": "reject" } },            // closes cookie walls
  { "wait": { "text": "Saved", "ms": 15000 } },
  { "wait": { "idle": true, "ms": 15000 } },        // network settled
  { "sleep": { "ms": 300 } }
]
```

### What it can see through

A page's own script cannot read across a shadow boundary or a site boundary, so
a naive scan misses the buttons inside web components (YouTube, Reddit, most
modern apps) and inside embedded widgets. `elements` covers all three:

| Boundary | How |
|---|---|
| open shadow roots, including nested | walks every `shadowRoot` |
| same-origin iframes | walks `contentDocument` |
| cross-origin iframes | attaches to the frame's own DevTools target and reads it in its own session |

Cross-origin frame elements come back with a `frame` label, and clicking one
dispatches the event to that frame's session at frame-local coordinates. A
cross-origin frame is only separate when it is a different **site** — two ports
on `127.0.0.1` are one site and stay in one process, which is a real limit of
site isolation, not of this tool.

### A typical flow, three calls

```jsonc
// 1. what is on the page
{ "name": "elements", "arguments": { "find": "save" } }

// 2. do several things at once
{ "name": "input", "arguments": { "actions": [
    { "click": { "i": 12 } },
    { "insert": "Hello" },
    { "key": "Enter" }
] } }

// 3. prove it worked
{ "name": "eval", "arguments": { "expression": "document.body.innerText.includes('Hello')" } }
```

### Talking to it while it works

`~/.local/bin/cdp-chat` is a small window (niri: `Mod+Shift+N`) that gives a
human a way to redirect the agent mid-run without reaching for a terminal. It
writes `chat.jsonl`; the agent waits on that file with `idle`, which **costs
nothing while it blocks** — no turns, no polling, no tokens. The reply costs one
turn. A message is also handed to the running session directly, so it can land
while the agent is mid-task rather than only at its next wait.

`cdp-chat-toggle` starts, raises or stops it (`stop`, `status`). Closing the
window quits the process: a pidfile checked against the process's own command
line, never a pattern match.

**Clear deletes the conversation on both sides**, including the agent's read
cursor, so a cleared chat is not half-remembered by the agent. One archive of the
whole exchange is kept as `conversation-<stamp>.jsonl`.

### Cost

Measured on this machine (medians): `eval` ~2 ms, a click ~17 ms, a full
`elements` scan of a large page ~28 ms and ~8 KB of JSON instead of 1.5 MB of
HTML. `batch` runs several calls in one request. `flow` runs a loop server-side,
so a hundred clicks cost one model turn. `idle` waits for nothing to happen
without spending a turn.

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
- **A cross-origin frame is only separate when it is a different site.** Two
  ports on `127.0.0.1`, or a subdomain of the same registrable domain, stay in
  one renderer, so site isolation never applies and the frame is read as part of
  the page.
- **Screenshots need a window the compositor is drawing.** If the browser window
  is not on a visible workspace, a capture can wait seconds for a frame that
  never arrives. It says so and gives up; every other tool is unaffected.
- `paste` is Wayland-only, because it goes through `wl-copy`.
- Drag is one round trip per interpolation step, deliberately: batching the
  moves makes Chromium coalesce them into a single jump, and a slider or a
  signature field needs the path.

## Licence

MIT.
