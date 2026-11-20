---
name: cdp-mcp
description: Install and configure cdp-mcp, a small MCP server that lets a coding agent drive the user's own Chromium-family browser over CDP. Use when the user asks for browser automation, page reading, clicking, typing, screenshots, or a logged-in browser on Linux, or when wiring cdp-mcp into OpenCode or Claude Code.
---

# cdp-mcp

A browser tool for coding agents: one static Rust binary over MCP stdio, attached
to the user's real browser. No extension, no Playwright, no second profile, so
every login is already there.

## Check before installing

1. Linux x86-64 (v1). Ask before assuming another OS.
2. A Chromium-family browser is running with a DevTools port:

   ```sh
   curl -s http://127.0.0.1:9222/json/version
   ```

   If that fails, ask the user to start their browser with
   `--remote-debugging-port=9222`. Recent stock Chrome needs a dedicated
   `--user-data-dir` for that flag; Helium, Chromium, Brave, Edge and Vivaldi
   usually accept it directly. Firefox and Safari do not work.
3. Rust 1.88+ only if building from source.

## Install

From the checkout, run the installer:

```sh
./install.sh
```

It finds OpenCode and Claude Code, asks whether to install for every project or
only one, then installs the binary, the MCP entry, and this skill. For scripts:
`./install.sh --global --yes` or `./install.sh --project DIR --yes`.

Manual fallback:
`cargo install --git https://github.com/benkaesy820/cdp-mcp`, or build and copy:

```sh
cargo build --release --target x86_64-unknown-linux-musl
install -m 755 target/x86_64-unknown-linux-musl/release/cdp-mcp ~/.local/bin/cdp-mcp
```

## Configure the agent

OpenCode, in `~/.config/opencode/opencode.json`:

```json
{
  "mcp": {
    "servers": {
      "cdp": {
        "type": "local",
        "command": ["/home/YOU/.local/bin/cdp-mcp", "--profile", "default"]
      }
    }
  }
}
```

Claude Code:

```sh
claude mcp add cdp -- /home/YOU/.local/bin/cdp-mcp --profile default
```

Restart or reload the agent after editing MCP config. A Claude Code project-scoped server needs one approval on next launch. Use a different
`--profile` per agent if they should not share the last-used tab.

## Verify

Call the `status` tool. It should return `"up": true` with the browser version,
or a doctor result with the fix. Then call `tabs list`. Do not claim it works
without that call.

## Safety

- CDP has no authentication. Keep the endpoint on loopback; never tunnel or
  proxy the port.
- The default build refuses non-loopback endpoints and
  `paste { method: "system" }`. `--allow-remote` and the system paste exist only
  in `cargo build --features experimental`. Do not build experimental unless the
  user asks and understands the risk.
- The tool acts as the logged-in user. Ask before anything irreversible:
  posting, paying, deleting, sending.

## Limits to state plainly

- Linux x86-64 only for v1.
- No network awareness: a click that fetches while the DOM stays still can read
  as idle before the response renders.
- One call at a time per server; a long wait queues the rest.
- Reports coalesce at 80 ms, text checks at 50 ms, `maxIdleMs` below about
  100 ms is approximate.
- Screenshots only, no video.
- `maxIdleMs` defaults to `timeoutMs`.

The README has the full tool list and configuration.
