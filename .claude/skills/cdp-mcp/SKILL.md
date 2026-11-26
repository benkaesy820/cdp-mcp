---
name: cdp-mcp
description: Drive the user's own Chromium-family browser (Helium, Chrome, Chromium, Brave) through the cdp MCP tools or the `cdp` command — open and read web pages, click buttons and links, open menus, fill and submit forms, pick options, dismiss cookie banners, wait for pages, take screenshots, using the user's real logins. Use for any browser task, and for installing or wiring cdp-mcp into OpenCode or Claude Code on Linux.
---

# cdp-mcp: use the person's real browser

The tools act in the person's own browser, signed in as them. Everything below
is the exact shape of a call and of its answer.

## The loop — do this every time

1. **`status`** — see the browser and its tabs. Each tab has a handle: `t1`, `t2`…
2. **Go to the page** in one tab: `tabs {action:"goto", tab:"t1", url:"…", idle:true}`.
   If there is no tab you may use, `tabs {action:"open", url:"…", idle:true}` makes one.
3. **`elements`** — what can be clicked or typed into, each with an index `i`.
4. **`input`** — act on an index: click, type, press keys, select.
5. **Check what happened** — `elements {mode:"delta"}` (what appeared or went) or
   `text` (what the page says now). Then go back to 3 or 4.

**Golden rule: an index `i` comes from the latest `elements` answer only.** After
any click, typing, scrolling or page change, the numbers can shift (opening a
menu inserts new elements, so the ones after it move). Scan again before using an
index you did not just read.

## Reading an `elements` answer

A real answer (trimmed):

```json
{ "elements": [
    "0 button icon:menu · header collapsed",
    "5 link Products · nav",
    "7 checkbox Account · nav unchecked",
    "8 summary More filters · collapsed",
    "10 textbox Email · required",
    "12 tab Tab A · current",
    "15 button Reply · ×2",
    "17 link About us · footer" ],
  "collapsed": [
    "drawer of 4 (Home | Settings | Billing | Sign out) — opened by 0 button icon:menu",
    "menu of 2 (Laptops | Phones) — opened by 5 link Products" ],
  "overlays": [ "dialog Holiday offer · closers: 2 OK" ],
  "scroll": { "y": 0, "h": 2400, "vh": 510 },
  "total": 18, "shown": 17 }
```

Each line is **`i role label · flags`**:

- **`i`** — the number you pass to `input` (`{click:{i:10}}`).
- **role** — `button`, `link`, `textbox`, `checkbox`, `radio`, `combobox` (a
  `<select>`), `tab`, `summary`, …
- **label** — the name a person sees. `icon:menu`, `icon:search`, `icon:close`
  are icon-only buttons (`icon:menu` is the hamburger). `(no label)` means the
  page gave it no name.
- **flags** (after `·`, only when true):

| Flag | Meaning |
|---|---|
| `header` `nav` `footer` `aside` `dialog` `search` | where it is on the page; no region flag = main content |
| `off` | off screen; a click scrolls to it by itself |
| `collapsed` / `expanded` | a menu, accordion or `<details>` is closed / open |
| `popup=menu` | clicking it opens a menu or dialog |
| `checked` / `unchecked` | checkbox, radio or switch state |
| `current` | the active tab, or the current page in a nav |
| `selected=Free` | what a `<select>` shows now (visible text) |
| `value=…` | what is typed in a field (`***` for a password) |
| `required`, `invalid`, `disabled`, `pressed` | form and button state |
| `×2` | this label appears 2 times; only the first index is listed — use `find` to get them all |
| `frame=host` | inside an embedded frame from another site; clicking works the same |

**`collapsed`** lists what the page hides — closed menus, drawers, sidebars,
`<details>` — with what is inside and the index that opens it. To reach "Billing"
above: click `0`, then scan again and click the new index of Billing. `maybe
opened by` is a guess; check with a delta after clicking.

**`overlays`** are dialogs, cookie walls and modals in front of the page, with
the indexes that close them.

Other fields: `total` (how many elements), `shown` (how many listed; raise
`limit` or use `find` when smaller), `capped` + `hint` (a very big page: narrow
with `find` or `within`), `suspect` (the list is empty and the page is nearly
empty — it may still be loading: call `idle` once, scan again, and then believe it).

### Asking for less

- `elements {find:"save"}` — only labels containing "save" (case-insensitive),
  every repeat listed. The cheapest way to find one thing.
- `elements {role:"textbox"}` — only fields (or `button`, `link`, `checkbox`…).
- `elements {within:"main"}` — one part of the page, by CSS selector. The
  indexes then belong to that scoped list; `input` uses the same scope
  automatically.
- `elements {mode:"viewport"}` — only what is on screen.
- `elements {mode:"delta"}` — only what changed since the last scan. Use it after a click.
- `elements {full:true}` — JSON objects with pixel coordinates (`x`,`y`,`w`,`h`),
  only when you need coordinates.

## Doing things: `input`

`input` takes a list of actions, run in order in one call:

```json
{ "tab": "t1", "actions": [
  { "click": { "i": 10 } },
  { "insert": "me@example.com" },
  { "key": "Enter" }
] }
```

| Action | Example | Notes |
|---|---|---|
| click | `{"click":{"i":5}}` | scrolls to it first; `{"click":{"x":400,"y":300}}` for a point |
| type | `{"insert":"hello"}` | types into the focused field: click the field first |
| key | `{"key":"Enter"}` | also `Tab`, `Escape`, `Backspace`, `ArrowDown`, `Control+a` |
| select | `{"select":{"i":1,"value":"Pro"}}` | a `<select>`: the visible text or the option value; a list for a multiple select |
| scroll | `{"scroll":{"dy":800}}` | down 800 px; negative is up; add `"i":3` to scroll the pane under element 3 |
| hover | `{"hover":{"i":5}}` | opens hover menus |
| close a popup | `{"dismiss":{}}` | clicks a cookie banner's or modal's reject/close button; `{"prefer":"accept"}` to accept |
| wait | `{"wait":{"text":"Saved","ms":15000}}` | until that text is on the page; `{"wait":{"idle":true,"ms":15000}}` until loading settles |
| browser dialog | `{"dialog":{"accept":true}}` | answers an alert/confirm; add `"text":"…"` for a prompt |

The answer has a `log` with one entry per action, for example
`{"log":[{"clicked":{"i":10,"x":107,"y":8}},{"inserted":14},{"key":"Enter"}]}`.
`url` is filled in when the page navigated. `remapped:{from:6,to:7}` in a click
means the page moved the element and the click still reached the one you meant.

To replace what is in a field: click it, `{"key":"Control+a"}`, then `{"insert":"new text"}`.

## Recipes

**Read a page**
`tabs {action:"goto", tab:"t1", url:"https://…", idle:true}` → `text {}`.
`text` reads the page's main content plus any open dialog (`within` in the answer
says what it read). `text {within:"body"}` reads everything; `limit` sets how many
characters (default 4000).

**Click a button you can name**
`elements {find:"Save"}` → `input {actions:[{click:{i:<that i>}}]}` →
`elements {mode:"delta"}` or `text {}` to see the result.

**Open a hamburger menu or sidebar**
Look in `collapsed` for the item you need and its opener index → click the opener →
`elements {mode:"delta"}`: the menu's items appear in `added`, with new indexes → click one.

**Fill a form**
`elements {role:"textbox"}` (or `within:"form"`) → one `input` call:
`[{click:{i:A}}, {insert:"…"}, {click:{i:B}}, {insert:"…"}]` → `elements {find:"submit"}`
→ click it → `text {}` to read the result. Check `required`/`invalid` flags first.

**Cookie banner or popup in the way**
`input {actions:[{dismiss:{}}]}`. If the log says `"dismissed":null`, read the
`overlays` line and click one of its closer indexes, or press `{"key":"Escape"}`.

**Something further down the page**
`find` it and click: the click scrolls by itself. To load more of a feed:
`[{scroll:{dy:1500}}, {wait:{idle:true,ms:5000}}]`, then scan again.

**A page that never finishes loading** (YouTube, feeds, live dashboards)
Do not wait for it to stop. `tabs {…, idle:true}` returns once what is on screen
is still; if it says `"idle":false`, the page has content anyway — scan or read
it now. Waiting longer, or calling `idle` again and again, gains nothing.
`idle {}` alone waits for 2 s with no visible change (`offScreen` lists rows that
arrived below the fold, which do not count).

**Wait for something to appear**
`idle {untilText:"Order confirmed", timeoutMs:30000}` or
`idle {untilSelector:"[role=dialog]"}`. It wakes the moment it appears and says
what changed (`woke`). Do not loop on `elements` to wait.

**Screenshot**
`shot {}` saves a PNG and returns its `path`. `shot {i:5}` captures one element.
Look at the file only if you need to see it.

**Repeat a step without spending turns**
`flow {steps:[{tool:"input", args:{actions:[{click:{i:12}}]}}], repeat:{untilText:"No more results", max:20}}`
runs it inside the server until the text appears or `max` is reached.

## Other tools

| Tool | Example | For |
|---|---|---|
| `upload` | `upload {i:4, path:"/home/me/cv.pdf"}` | attach a file: `i` is the upload button or drop zone (or `selector` for an `<input type=file>`); `paths` for several |
| `paste` | `paste {i:7, path:"/tmp/image.png"}` | paste an image, video or PDF (or `text`) into a field or editor |
| `download` | `download {url:"https://…/report.csv", path:"/tmp/report.csv"}` | save a file with the person's cookies |
| `pdf` | `pdf {path:"/tmp/page.pdf"}` | print the page to a PDF |
| `emulate` | `emulate {preset:"mobile"}` … `emulate {reset:true}` | see the mobile layout; scan again afterwards |
| `eval` | `eval {expression:"document.title"}` | run JavaScript when no other tool fits |

## When a call fails

Every error is one line that names the fix. Do what it says; do not retry the same call.

| Error says | What to do |
|---|---|
| `"blocked":true … "reason":"obscured"` | something covers the target (`obscuredBy` says what). Close it: `dismiss`, an overlay closer, `Escape`, or the menu's own toggle. Then scan again. |
| `"reason":"disabled"` | the control is disabled. Fill the required fields first; look for `required`/`invalid`. |
| `"reason":"detached"` or `"zero_size"` | it is gone or hidden now. Scan again. |
| `no element with index 99 (22 found)` | the index is from an old scan. Scan again. |
| `element 6 is not what the last scan showed` | the page changed under that index. Scan again and use the new index. |
| `"dialog":{…}` / `the page is blocked by this dialog` | a browser alert or confirm is open: `input {actions:[{dialog:{accept:true}}]}` (or `accept:false`). |
| `select failed: no option X (options: …)` | use one of the listed options. |
| `'Post' was already clicked in this run` | a final action (Post, Send, Delete, Pay…) was already clicked once. Check the result with `text` or a delta; never click it twice unless the person asked. |
| `unknown tab handle t9` | handles are per session: call `status`. |
| `scopeMissing: true` | the `within` selector matched nothing: drop it or use another. |
| `status` shows `"up": false` | the browser has no DevTools port. Tell the person; see Install. |

## Rules

- **One tab.** Navigate it in place with `goto`. Open a second tab only when the
  task needs two pages at once, and close it after (`tabs {action:"close", tab:"t2"}`).
- **Never bring a tab to the front** (`foreground`); the person may be typing elsewhere.
- **Ask before anything irreversible**: posting, sending, paying, deleting,
  accepting terms. The person is signed in; it really happens.
- **Prefer `find` and `delta` over full scans**, and `text` over screenshots, to keep
  answers small.
- **Do not guess indexes**, and do not reuse one after the page changed.
- **Do not claim it worked without checking**: read the result with `text` or a delta.

## Without MCP: the `cdp` command

The same tools from a shell, when MCP tools are not loaded or the session is long
(the MCP tool list costs about 17 KB of context on every turn; a command costs nothing):

```sh
cdp status                      # t1  Title  URL — one line per tab
cdp go https://example.com      # navigate the current tab
cdp read --max 800              # the page as text (main content)
cdp find "Save"                 # i  label  x,y  tap|---
cdp click 3                     # click element 3 from that list
cdp type "hello"                # type into the focused field
cdp key Enter                   # Enter, Escape, Tab, Backspace…
cdp shot --path /tmp/s.png      # prints the path
cdp eval "document.title"
cdp tabs open https://url       # background tab
cdp tabs close t2
cdp json elements '{"find":"Save","limit":10}'   # any tool with raw JSON arguments
```

Flags anywhere after the verb: `--tab t1`, `--json` (raw answer), `--max N`, `--path P`.

## Install (only when it is not set up)

Check first: `curl -s http://127.0.0.1:9222/json/version` must answer. If not, the
person must start their browser with `--remote-debugging-port=9222` (stock Chrome
also needs its own `--user-data-dir`), or run `cdp-browser`. Firefox and Safari
do not work.

Install from a release (needs `curl`, `tar`, `python3`):

```sh
curl -fsSL https://github.com/benkaesy820/cdp-mcp/releases/latest/download/install.sh -o /tmp/cdp-install.sh
bash /tmp/cdp-install.sh                 # asks where to install
bash /tmp/cdp-install.sh --global --yes  # every project, no questions
```

It installs the binary to `~/.local/bin/cdp-mcp`, the MCP entry for each agent it
finds (OpenCode, Claude Code, Codex), and this skill. Manual entries, if needed:

- OpenCode, `~/.config/opencode/opencode.json`:
  `{"mcp":{"servers":{"cdp":{"type":"local","command":["/home/YOU/.local/bin/cdp-mcp","--profile","default"]}}}}`
- Claude Code: `claude mcp add cdp -- /home/YOU/.local/bin/cdp-mcp --profile default`

Restart the agent after changing MCP config, then call `status` to confirm. Do not
say it works before `status` answers with `browser` and `tabs`.

## Safety and limits

- CDP has no password: keep it on 127.0.0.1. The server refuses any other address;
  never tunnel or expose the port.
- `paste` hands data to the page and never touches the system clipboard.
- One call at a time per server; a long `idle` makes the others wait.
- `idle` does not watch the network, only what is visible: after a click that
  loads data, wait for the text you expect (`untilText`) rather than for quiet.
- Screenshots only, no video.
- Tested on one machine only: Arch Linux x86_64, Helium 157 (Chrome/157), cdp-mcp
  0.16. The aarch64 build is compiled but has never been run; macOS and Windows have
  no build. Say so rather than implying wider support.
