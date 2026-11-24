# cdp — drive the person's browser from a shell

Use this instead of the MCP tools when the job is a few commands in a session
that is already long: the MCP catalog costs ~17 KB of context on every turn, a
command costs nothing. Reach for the MCP server when you need a long-lived
session, `batch`, or `flow`.

Verified on Helium (Chrome/157) and cdp-mcp 0.10.7+.

## The five commands you will use most

```sh
cdp status                      # t1  Title  URL   (handles, 5 ms)
cdp read --max 800              # the page as text
cdp find "Save"                 # i  label  x,y  tap|---
cdp click 3                     # tap element 3 from that list
cdp go https://example.com      # navigate the current tab
```

`status` prints one line per tab: the handle first. **Use the handle** (`--tab t1`)
rather than the 32-character target id — it is shorter and it is what the model
gets right.

## Everything else

```sh
cdp tabs                        # list
cdp tabs open https://url       # background tab; never steals focus
cdp tabs close t2
cdp type "hello"                # ASCII into the focused field
cdp key Enter                   # Enter, Escape, Tab, Backspace
cdp shot --path /tmp/s.png      # one line: the path
cdp eval "document.title"
cdp json elements '{"find":"Save","limit":10}'   # escape hatch: any tool, raw args
```

Flags, anywhere after the verb: `--tab t1|id`, `--json` (raw payload),
`--max N` (read limit), `--path P` (shot destination).

## Rules that keep it from being annoying

- **One tab, and it stays in the background.** `tabs open` does not focus the
  window; research work navigates the tab you already have instead of spawning
  more. Bringing a tab to the front is refused unless the person consented at
  `cdp-mcp setup`.
- **Read with `find`, not with `read`, when you know what you want.** A full page
  can be thousands of tokens; three matching nodes are tens.
- **`read` starts at the top of the body**, so it includes site chrome. If the
  first lines are a menu, use the MCP `text {within:"main"}` or filter.
- **Errors are one line and name the fix** (`unknown tab handle t9; run status`,
  `no browser is publishing a DevTools endpoint`). Do not retry blindly: the
  message says what to change.
- **A screenshot path is enough.** Do not inline an image; the tool writes the
  file and prints its path in ~250 B.

## When it does not work

| Message | Meaning |
|---|---|
| `no browser is publishing a DevTools endpoint` | Nothing is listening on 9222 and discovery found no published port. Start one: `helium-cdp`, or `cdp-mcp setup` to allow it. |
| `unknown tab handle t9` | Handles are per server process; run `cdp status` again. |
| `refusing non-loopback endpoint` | CDP has no auth; only 127.0.0.1 is allowed. |
