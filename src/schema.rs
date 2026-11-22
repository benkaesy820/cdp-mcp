// The tool schemas: one JSON document per tool, kept apart from dispatch so a
// schema change never touches behaviour.

use serde_json::{json, Value};

pub(crate) fn tool_defs() -> Value {
    json!([
        {
            "name": "flow",
            "description": "Run a sequence of tool calls inside the server, optionally repeating until a condition is true. The point is cost: N browser steps for ONE model turn. Use `repeat:{untilText|untilGone, max, delayMs}` for 'click more until the thing appears' without a turn per click. Returns iteration count, whether the condition was met, and the last three iterations only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "steps": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": { "tool": { "type": "string" }, "args": { "type": "object" } },
                            "required": ["tool"]
                        }
                    },
                    "repeat": {
                        "type": "object",
                        "properties": {
                            "untilText": { "type": "string" },
                            "untilGone": { "type": "string" },
                            "max": { "type": "integer", "description": "default 1, max 200" },
                            "delayMs": { "type": "integer", "description": "between iterations, default 700" }
                        }
                    },
                    "stopOnError": { "type": "boolean", "description": "default true" },
                    "tab": { "type": "string" }
                },
                "required": ["steps"]
            }
        },
        {
            "name": "idle",
            "description": "Wait without spinning and without asking: a MutationObserver inside the page calls back the moment something changes, so a still page costs no traffic at all and a change is noticed within a few milliseconds instead of on a timer. Returns when `untilText` or `untilSelector` matches, or when the page has been still for `maxIdleMs`. The result names what changed (`appeared`, `dialogs`, `changes`, `textChars`/`textDelta`, `url`, `navigated`), so you usually do not need to scan the page afterwards. `changes` counts reports from the page, coalesced to at most one per 80 ms while it is moving, so read it as activity rather than as a mutation count. Use it instead of a sleep loop when waiting for a page.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "timeoutMs": { "type": "integer", "description": "stop waiting after this long, message or not (default 600000 = 10 min, max 900000)" },
                    "maxIdleMs": { "type": "integer", "description": "treat the page as idle after this long with no change (defaults to timeoutMs). Values below about 100 ms are approximate: reports are coalesced" },
                    "untilText": { "type": "string", "description": "also wake when this text appears" },
                    "untilSelector": { "type": "string", "description": "also wake when this CSS selector matches something, e.g. input[name=email] or [role=dialog]" },

                    "tab": { "type": "string" }
                }
            }
        },
        {
            "name": "status",
            "description": "Report the browser version and every open page tab. Start here.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "tabs",
            "description": "List, open, close or navigate page tabs. goto, back, forward and reload act on the tab in place: they report moved and the resulting url, return moved:false at the ends of history, and return the dialog instead of waiting when the page asks before it leaves. A document navigation also drops this tab's scanned indexes and scope, so rescan before using an old index.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "open", "close", "goto", "back", "forward", "reload"] },
                    "url": { "type": "string", "description": "for action=open or goto" },
                    "idle": { "type": "boolean", "description": "for action=open or a navigation: also wait for loading to settle (single-page apps paint after readyState)" },
                    "ignoreCache": { "type": "boolean", "description": "for action=reload: bypass the HTTP cache" },
                    "foreground": { "type": "boolean", "description": "for action=open: bring the tab to the front and focus the browser. Default false, because activating a tab takes the keyboard away from whoever is typing." },
                    "tab": { "type": "string", "description": "the tab to act on; omit for the last tab this server used" },
                    "all": { "type": "boolean", "description": "for action=list: include chrome-extension:// pages (default false)" }
                },
                "required": ["action"]
            }
        },
        {
            "name": "pdf",
            "description": "Print the page to a PDF. Saves to `path` when given, otherwise under this profile's data directory as pdfs/page-<ms>.pdf. Backgrounds print by default: a styled page is what a human expects to read. Letter by default; a4, landscape, scale, pageRanges and preferCSSPageSize are explicit.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab": { "type": "string" },
                    "path": { "type": "string", "description": "where to write it; omit for the profile's pdfs folder" },
                    "format": { "type": "string", "enum": ["letter", "a4"], "description": "paper size (default letter)" },
                    "landscape": { "type": "boolean", "description": "default false" },
                    "printBackground": { "type": "boolean", "description": "default true" },
                    "scale": { "type": "number", "description": "0.1..2.0, default 1.0" },
                    "pageRanges": { "type": "string", "description": "e.g. \"1-2,4\"" },
                    "preferCSSPageSize": { "type": "boolean", "description": "honour an @page size when the page sets one" }
                }
            }
        },
        {
            "name": "emulate",
            "description": "Emulate a viewport, touch input and CSS media. A preset sets all of it: mobile 390x844@3, tablet 768x1024@2, desktop 1280x800@1; explicit width/height/deviceScaleFactor/mobile/touch override the preset. Media calls add up: colorScheme then reducedMotion keeps both, because Emulation.setEmulatedMedia replaces the whole list and the active set is tracked per tab. reset clears metrics, touch and media. A viewport change moves every element, so this tab's scan cache is dropped and the next elements rescans; a within scope survives.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab": { "type": "string" },
                    "reset": { "type": "boolean", "description": "clear device metrics, touch and media" },
                    "preset": { "type": "string", "enum": ["mobile", "tablet", "desktop"] },
                    "width": { "type": "integer", "description": "viewport width, 100..7680" },
                    "height": { "type": "integer", "description": "viewport height, 100..7680" },
                    "deviceScaleFactor": { "type": "number", "description": "device pixel ratio, greater than 0" },
                    "mobile": { "type": "boolean", "description": "emulate a mobile viewport" },
                    "touch": { "type": "boolean", "description": "defaults to mobile when a viewport is applied; works on its own too" },
                    "colorScheme": { "type": "string", "enum": ["light", "dark", "no-preference"] },
                    "reducedMotion": { "type": "string", "enum": ["reduce", "no-preference"] },
                    "media": { "type": "string", "enum": ["screen", "print", ""] }
                }
            }
        },
        {
            "name": "eval",
            "description": "Evaluate a JavaScript expression in the tab and return its value as JSON. Await promises. Omit `tab` for the first page tab.",
            "inputSchema": {
                "type": "object",
                "properties": { "expression": { "type": "string" }, "tab": { "type": "string" } },
                "required": ["expression"]
            }
        },
        {
            "name": "elements",
            "description": "Scan the page: `overlays` lists dialogs/modals/cookie walls with their dismiss buttons, and `elements` lists visible interactive nodes with a clickable index `i` and centre point; `capped` says the scanner stopped at its element cap (600 by default; pass `limit` above 600 to lift it, up to 2000) with matching elements left. Filter with `find` and cap with `limit` to keep the payload small. Indexes are absolute in the unfiltered list and are re-resolved at click time, so they cannot silently go stale. A cross-origin frame element is listed with its `frame` and acted on inside that frame's own session: click re-resolves it live (scrolling the frame if needed), and hover and select route into the frame too; `paste` resolves indexes in the main document. Form controls include current `value` (passwords masked), `checked`, `selected` and `disabled`; `within` scopes the scan to a selector in the main document (cross-origin frame elements are left out), the result echoes `scope`, and a selector that matches nothing returns no elements with `scopeMissing: true` instead of the whole page, and `overlays` stay page-wide and are still reported.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tab": { "type": "string" },
                    "find": { "type": "string", "description": "only elements whose label contains this text (case-insensitive)" },
                    "limit": { "type": "integer", "description": "max elements to return (default 120)" },
                    "full": { "type": "boolean", "description": "include width and height" },
                    "mode": { "type": "string", "enum": ["full", "viewport", "delta"],
                              "description": "full = everything; viewport = only what is on screen; delta = only what changed since the previous scan (use after a click)" },
                    "shot": { "type": "string", "enum": ["none", "viewport", "full", "custom"],
                              "description": "also return a PNG in this same result: viewport = what you see, full = whole page (height-capped), custom = region {x,y,w,h}" },
                    "region": { "type": "object", "description": "for shot=custom: {x,y,w,h} in CSS pixels",
                                "properties": { "x": {"type":"number"}, "y": {"type":"number"}, "w": {"type":"number"}, "h": {"type":"number"} } },
                    "shotMaxWidth": { "type": "integer", "description": "downscale the attached capture to this width (default 1024)" },
                    "shotFormat": { "type": "string", "enum": ["jpeg", "png", "webp"], "description": "format of the attached capture (default jpeg)" },
                    "unique": { "type": "boolean", "description": "collapse elements that share a label (repeated nav links)" },
                    "within": { "type": "string", "description": "CSS selector to scope the scan, e.g. \"main\" — much cheaper on big pages" },
                    "role": { "type": "string", "description": "only elements with this aria role or tag, e.g. button, link, input" }
                }
            }
        },
        {
            "name": "text",
            "description": "The page as readable text, top to bottom: headings, labels, paragraphs, list items. Use this to find out what a page says or to confirm a sentence is there; use `elements` for what can be clicked. `within` scopes it to a CSS selector in the main document (main, article, [role=dialog]); a selector that matches nothing returns empty text with `scopeMissing: true`, and an invalid selector is an error. `limit` caps characters (default 4000). `frames: true` appends the readable text of visible cross-origin iframes, each labeled with its host.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "within": { "type": "string", "description": "CSS selector to read, default body" },
                    "limit": { "type": "integer", "description": "max characters to return (default 4000)" },
                    "frames": { "type": "boolean", "description": "also include readable text from visible cross-origin iframes, labeled with the frame's host (default false)" },
                    "tab": { "type": "string" }
                }
            }
        },
        {
            "name": "input",
            "description": "Send real user input, in order. Actions: {click:{i|x,y[,button:right|middle,count:2,modifiers:[Shift]]}} (an index is scrolled into view and resolved in the same `within` scope as the last elements call, or in the action's own `within` when it carries one), {hover:{i}}, {drag:{from:{i},to:{i},steps:12}}, {tap:{i}} (touch), {swipe:{from,to}} (touch), {select:{i,value}} (<select>; value is a string, or an array for a multiple select), {insert:\"text\"}, {key:\"Enter\"|\"Control+a\"|\"Control+Shift+z\"|\"a\"|\"Escape\"|\"Tab\"|\"ArrowDown\"|\"F5\"}, {scroll:{dy[,dx][,i|x,y]}} (a real wheel event at that element, that point, or the middle of the viewport: scrolls the page or the pane under it, and a canvas receives the wheel too; the log reports from/to/axis/moved, `delivered` for whether the wheel reached the page, `atBottom` for the vertical end, `container` and the wheel point), {dismiss:{prefer:\"reject\"}} (closes cookie walls and modals, refusing cookies when offered), {wait:{idle:true,ms:15000}} (waits for loading to settle), {wait:{text:\"...\",ms:15000}}, {sleep:{ms:300}}. Real events, so React/Draft editors accept them. An indexed click is refused when the element is detached, zero-size, disabled or covered, with `{blocked, reason, obscuredBy}`; `force` bypasses that check, and the log entry then carries `covered` with what was on top. With `settleMs`, the result also reports `navigated`, `url`, `openedTab`, `downloads` (started with filename and guid, or completed with a verified `path` when downloadDir/saveDownloads is set) and same-origin errors plus a failed destination document. Indexes inside a cross-origin iframe are clicked, hovered and selected inside that frame's own session, with the same refused-when-blocked answers as the page. A JavaScript dialog blocks the page: the call that hit it returns `dialog:{type,message}`, every other tool then fails fast with that dialog, and `{dialog:{accept:true,text:\"...\"}}` answers it — one per call, so a chained alert after a confirm is reported for a second answer.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "actions": { "type": "array", "items": { "type": "object" } },
                    "tab": { "type": "string" },
                    "announce": { "type": "boolean", "description": "also check for a dialog that appeared because of these actions (default false: costs a round trip)" },
                    "force": { "type": "boolean", "description": "allow repeating a final confirmation (Post/Publish/Delete/…) that was already clicked in this run, and bypass the click obstruction check. Verify with delta first." },
                    "downloadDir": { "type": "string", "description": "save downloads started by these actions into this folder; the browser's own downloads are put back before the call returns" },
                    "saveDownloads": { "type": "boolean", "description": "save downloads under <data>/downloads (default false); implies a 5 s settle window unless settleMs says otherwise" },
                    "settleMs": { "type": "integer", "description": "after the actions, read CDP events for up to this many ms and return same-origin HTTP 4xx/5xx, uncaught exceptions and console errors in `errors` (default 0 = off). Adds the settle time to the call." }
                },
                "required": ["actions"]
            }
        },
        {
            "name": "upload",
            "description": "Attach local files to a file input, or to the chooser an element opens. Selector mode sets them on the input matching `selector`/`index` (default input[type=file]); `i` mode clicks that element and intercepts the file chooser it opens — a button that creates a detached input, a styled dropzone — then sets the files on it. Use `paths` for several files; a single-file chooser refuses more than one. Every local path is checked before anything is clicked, and interception is disarmed before the call returns.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "one absolute local path" },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "several files, for a multiple input or chooser" },
                    "selector": { "type": "string", "description": "CSS selector for the file input (default input[type=file])" },
                    "index": { "type": "integer", "description": "which match to use when several inputs share the selector (default 0). An avatar and a banner on one form are the common case." },
                    "i": { "type": "integer", "description": "click this element and intercept the file chooser it opens; cannot be combined with selector or index" },
                    "within": { "type": "string", "description": "scope for i; defaults to the tab's last elements scope" },
                    "timeoutMs": { "type": "integer", "description": "how long to wait for the chooser after the click, 500..15000 (default 5000)" },
                    "tab": { "type": "string" }
                }
            }
        },
        {
            "name": "paste",
            "description": "Paste text or a file (image, video, PDF) into the focused element, or into the element `i` after clicking it. The data is handed to the page as a paste event: a file nobody takes as a paste is offered as a drop, and text nobody intercepts is inserted as typed text. It never touches the system clipboard, so it works while the browser is unfocused. The reply says how it landed (`via`) or why it did not.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "local file to paste" },
                    "text": { "type": "string", "description": "text to paste (instead of a file)" },
                    "i": { "type": "integer", "description": "click this element index first to focus it" },
                    "tab": { "type": "string" }
                }
            }
        },
        {
            "name": "download",
            "description": "Download a resource to a local path, fetched inside the page so cookies and referer are correct (authenticated images work). Choose the target with url, or with i = the position in the page's list of <img> that have a source (lazy placeholders without one are skipped).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "url": { "type": "string", "description": "absolute or relative URL to fetch" },
                    "i": { "type": "integer", "description": "position in the page's sourced <img> list (alternative to url)" },
                    "maxBytes": { "type": "integer", "description": "refuse larger files (default 10 MiB)" },
                    "tab": { "type": "string" }
                },
                "required": ["path"]
            }
        },
        {
            "name": "shot",
            "description": "Screenshot the tab: the viewport by default, the whole page, a region, or one element by index. `path` is optional: omit it and the file lands in this profile's screenshot_dir with a timestamped name. `i` resolves in the main document, open shadow roots and same-origin frames (not cross-origin iframes), scrolls the element into view and captures just its box; `padding` widens that box.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "tab": { "type": "string" },
                    "mode": { "type": "string", "enum": ["viewport", "full", "custom"],
                              "description": "viewport (default) = what you see; full = whole page, height-capped at 8000px; custom = region {x,y,w,h}" },
                    "region": { "type": "object", "description": "for mode=custom: {x,y,w,h} in CSS pixels",
                                "properties": { "x": {"type":"number"}, "y": {"type":"number"}, "w": {"type":"number"}, "h": {"type":"number"} } },
                    "format": { "type": "string", "enum": ["png", "jpeg", "webp"], "description": "default: the profile's shot_format, else png" },
                    "quality": { "type": "integer", "description": "jpeg/webp quality 0-100 (default 85)" },
                    "i": { "type": "integer", "description": "capture this element's box, from elements; scrolls it into view first" },
                    "within": { "type": "string", "description": "scope for i; defaults to the last elements scope for this tab" },
                    "padding": { "type": "number", "description": "extra pixels around an element capture, 0..200 (default 0)" }
                }
            }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_schemas_are_well_formed() {
        let defs = tool_defs();
        let names: Vec<&str> = defs.as_array().unwrap().iter()
            .map(|t| t["name"].as_str().unwrap()).collect();
        for want in ["status", "tabs", "eval", "elements", "input", "shot", "paste", "flow", "emulate", "pdf"] {
            assert!(names.contains(&want), "{want} missing");
        }
        for t in defs.as_array().unwrap() {
            assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
        }
    }
}
