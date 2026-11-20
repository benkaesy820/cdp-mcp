// End-to-end tests against a real browser.
//
//   cargo build --release
//   node tests/e2e/run.mjs [path/to/cdp-mcp]
//
// Needs a Chromium-family browser on --remote-debugging-port=9222. Every test
// opens its own background tab and closes it, so the browser is left as found.
// Each check pins a bug that actually happened; the comment says which.
import { server } from "./mcp.mjs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const bin = process.argv[2] || join(here, "../../target/release/cdp-mcp");
const page = (f, hash = "") => "file://" + join(here, "pages", f) + hash;
const s = server(bin);
await s.rpc("initialize", { protocolVersion: "2024-11-05" });

// The cross-origin frame has to be a different SITE, not just a different port:
// two ports on 127.0.0.1 are the same site, so Chromium keeps them in one
// process and site isolation never comes into play.
import { createServer } from "node:http";
import { readFileSync } from "node:fs";
let framePort = 0;
async function serveFrames() {
  if (framePort) return framePort;
  const srv = createServer((q, res) => {
    res.writeHead(200, { "content-type": "text/html" });
    res.end(readFileSync(join(here, "pages", "cross.html")));
  });
  await new Promise(r => srv.listen(0, "0.0.0.0", r));
  framePort = srv.address().port;
  process.on("exit", () => srv.close());
  return framePort;
}

let failed = 0;
const check = (name, ok, got) => {
  console.log(`${ok ? "ok  " : "FAIL"}  ${name}${ok ? "" : "  — got " + JSON.stringify(got)}`);
  if (!ok) failed++;
};
const sleep = ms => new Promise(z => setTimeout(z, ms));
async function withTab(url, fn) {
  const tab = (await s.tool("tabs", { action: "open", url })).v.tab;
  const ev = async e => (await s.tool("eval", { tab, expression: e })).v.value;
  try { await fn(tab, ev); } finally { await s.tool("tabs", { action: "close", tab }); }
}

await withTab(page("basic.html"), async (tab, ev) => {
  const els = (await s.tool("elements", { tab, limit: 999 })).v.elements;
  const idx = l => els.find(e => e.label === l)?.i;

  // An off-screen target used to be "scrolled" via elementFromPoint, which is
  // null off-screen, so the click landed on nothing.
  await s.tool("input", { tab, actions: [{ click: { i: idx("Far button") } }] });
  await sleep(300);
  check("click by index scrolls an off-screen element into view", (await ev("c.far||0")) === 1, await ev("c"));

  // `scroll-behavior: smooth` made scroll report moved:false.
  await ev("scrollTo({top:0,behavior:'instant'})");
  const sc = await s.tool("input", { tab, actions: [{ scroll: { dy: 500 } }] });
  check("scroll moves under scroll-behavior: smooth", sc.v.log?.[0]?.scrolled?.moved === true, sc.v);

  // A failed step used to make with_conn re-run the whole action list.
  await ev("scrollTo({top:0,behavior:'instant'}); c.near=0");
  await s.tool("input", { tab, actions: [{ click: { i: idx("Near button") } }, { wait: { text: "never-appears", ms: 500 } }] });
  check("a failure mid-list does not repeat earlier actions", (await ev("c.near")) === 1, await ev("c.near"));

  const w = await s.tool("input", { tab, actions: [{ click: { i: idx("Make text arrive") } }, { wait: { text: "Arrived ✓", ms: 5000 } }] });
  check("wait {text} sees text that appears later", w.ok, w.v);

  await s.tool("input", { tab, actions: [{ click: { i: idx("Type here") } }, { insert: "hello" }, { key: "Control+a" }, { key: "Backspace" }, { key: "x" }] });
  check("keys: Control+a, Backspace, a printable key", (await ev("field.value")) === "x", await ev("field.value"));

  await ev("c.near=0");
  const t = performance.now();
  const r = await s.tool("input", { tab, actions: Array.from({ length: 100 }, () => ({ click: { i: idx("Near button") } })) });
  check("100 clicks by index all land", r.ok && (await ev("c.near")) === 100, await ev("c.near"));
  console.log(`      (${((performance.now() - t) / 100).toFixed(1)} ms per click)`);
});

await withTab(page("scoped.html", "#" + "é".repeat(400)), async (tab, ev) => {
  const all = await s.tool("elements", { tab });
  check("a long non-ASCII URL does not break elements", all.ok, all.v);

  // Indexes from `within` used to be clicked in a whole-page scan.
  const m = (await s.tool("elements", { tab, within: "main" })).v.elements;
  await s.tool("input", { tab, actions: [{ click: { i: m.find(e => e.label === "Save").i } }] });
  check("an index from elements{within} clicks inside that scope", JSON.stringify(await ev("h")) === '{"main":1}', await ev("h"));

  const si = m.find(e => e.tag === "select").i;
  await s.tool("input", { tab, actions: [{ select: { i: si, value: "two" } }] });
  check("select sets the value", (await ev("sel.value")) === "two", await ev("sel.value"));
  const bad = await s.tool("input", { tab, actions: [{ select: { i: si, value: "nope" } }] });
  check("select of a missing option is an error", !bad.ok, bad.v);

  await s.tool("input", { tab, actions: [{ click: { i: m.find(e => e.label === "combo").i } }, { key: "Control+Shift+z" }, { key: "Shift+a" }, { key: "1" }] });
  check("multi-modifier and shifted keys", (await ev("h.redo")) === 1 && (await ev("t.value")) === "A1", [await ev("h.redo"), await ev("t.value")]);

  const sc = await s.tool("input", { tab, actions: [{ scroll: { dy: 800 } }] });
  check("scroll falls back to the scrolling panel", sc.v.log?.[0]?.scrolled?.container === true && sc.v.log[0].scrolled.moved, sc.v);

  // A cancelled flow used to run to the end: stdin was not read meanwhile.
  const id = s.nextId();
  const flow = s.tool("flow", { tab, steps: [{ tool: "eval", args: { tab, expression: "1" } }], repeat: { max: 200, delayMs: 500, untilText: "never" } });
  await sleep(800);
  const pinged = await Promise.race([s.rpc("ping", {}).then(() => true), sleep(2000).then(() => false)]);
  check("ping is answered while a tool runs", pinged, pinged);
  const t = performance.now();
  s.notify("notifications/cancelled", { requestId: id, reason: "test" });
  const alive = await Promise.race([s.tool("eval", { tab, expression: "'alive'" }), sleep(3000).then(() => null)]);
  check("cancel stops a long flow promptly", alive?.v?.value === "alive", Math.round(performance.now() - t) + " ms");
  void flow;
});

await withTab(page("gestures.html"), async (tab, ev) => {
  const els = (await s.tool("elements", { tab })).v.elements;
  const I = l => els.find(e => e.label === l).i;
  await s.tool("input", { tab, actions: [{ drag: { from: { x: 40, y: 40 }, to: { x: 260, y: 160 }, steps: 12 } }] });
  check("drag delivers every step", (await ev("R.mouseMoves")) === 12, await ev("R.mouseMoves"));
  await s.tool("input", { tab, actions: [{ tap: { i: I("Pad") } }] });
  check("tap", (await ev("R.touch.start")) === 1 && (await ev("R.touch.end")) === 1, await ev("R.touch"));
  // Pipelined touch moves were coalesced: 10 sent, 2 seen.
  await s.tool("input", { tab, actions: [{ swipe: { from: { x: 250, y: 100 }, to: { x: 50, y: 100 }, steps: 10 } }] });
  check("swipe delivers every move", (await ev("R.touch.move")) === 10, await ev("R.touch"));
  // The unfocused browser cannot read the Wayland clipboard, so paste goes
  // through the page; it must not need the system clipboard at all.
  await s.tool("paste", { tab, i: I("Paste text"), text: "pasted ✓ 123" });
  check("paste text", (await ev("ta.value")) === "pasted ✓ 123", await ev("ta.value"));
  await s.tool("paste", { tab, i: I("Paste image"), path: join(here, "pages", "red.png") });
  check("paste an image into an editor that reads clipboard files", (await ev("ed.querySelectorAll('img').length")) === 1, await ev("R.paste"));
});

// ---- choosing which file input receives an upload ---------------------
// A form with an avatar and a banner has two identical file inputs. Picking the
// first match silently uploads to the wrong one.
await withTab(page("upload.html"), async (tab, ev) => {
  const r = (await s.tool("upload", {
    tab, path: join(here, "pages", "red.png"), selector: "input[type=file]", index: 1,
  })).v;
  const counts = await ev("[...document.querySelectorAll('input[type=file]')].map(i => i.files.length)");
  check("upload honours the index when several inputs match", JSON.stringify(counts) === "[0,1]", counts);
  check("upload reports which index it used", r.index === 1, r);
});

// ---- reading a page as text, and waiting for a selector -----------------
// Both were missing and every agent had to hand-write the same JS instead.
await withTab(page("basic.html"), async (tab, ev) => {
  const t = (await s.tool("text", { tab })).v;
  check("text returns what the page says", /Far button/.test(t.text || ""), (t.text || "").slice(0, 60));
  check("text reports its own size", (t.chars || 0) > 20, t.chars);

  // A selector that is already there must wake immediately, not burn the timeout.
  const t0 = Date.now();
  const w = (await s.tool("idle", { tab, untilSelector: "#near", timeoutMs: 8000 })).v;
  const ms = Date.now() - t0;
  check("idle {untilSelector} wakes on an existing element", w.woke === "selector", w);
  check("idle {untilSelector} does not wait out the timeout", ms < 1000, ms + " ms");

  // The page pushes; nothing is polled. A change 600 ms out has to come back
  // within a few ms of it, so any poll interval at all fails this.
  await ev("(() => { setTimeout(() => { const d = document.createElement('div'); d.id = 'late'; d.textContent = 'arrived late'; document.body.appendChild(d); }, 600); })()");
  const t1 = Date.now();
  const late = (await s.tool("idle", { tab, untilSelector: "#late", timeoutMs: 8000 })).v;
  const waited = Date.now() - t1;
  check("idle wakes on a selector that appears mid-wait", late.woke === "selector", late);
  check("idle wakes within 250 ms of the change, not on a poll tick", waited >= 540 && waited - 600 < 250, waited + " ms");
  check("idle reports what appeared, so no second scan is needed",
    JSON.stringify(late.appeared || []).includes("arrived late"), late.appeared);

  // A popup: the report has to name it and its role without a rescan, which is
  // the whole point of streaming the change back.
  await ev("(() => { setTimeout(() => { const d = document.createElement('div'); d.id = 'confirm'; d.setAttribute('role','dialog'); d.setAttribute('aria-label','Confirm booking'); d.textContent = 'Are you sure?'; document.body.appendChild(d); }, 400); })()");
  const pop = (await s.tool("idle", { tab, untilSelector: "[role=dialog]", timeoutMs: 8000 })).v;
  check("idle wakes on a dialog by role", pop.woke === "selector", pop);
  check("idle counts the dialog", (pop.dialogs || 0) >= 1, pop);
  check("idle names the dialog and its label",
    JSON.stringify(pop.appeared || []).includes("Confirm booking"), pop.appeared);

  await ev("(() => { setTimeout(() => { document.getElementById('late').textContent = 'needle-is-here'; }, 400); })()");
  const t2 = Date.now();
  const txt = (await s.tool("idle", { tab, untilText: "needle-is-here", timeoutMs: 8000 })).v;
  const waited2 = Date.now() - t2;
  check("idle {untilText} wakes on text that appears mid-wait", txt.woke === "text", txt);
  check("idle {untilText} wakes promptly too", waited2 >= 360 && waited2 - 400 < 250, waited2 + " ms");

  // A bulk insert has to be summarised: the report is capped, but the count
  // still says how much actually arrived.
  await ev("(() => { setTimeout(() => { const ul = document.createElement('ul'); ul.id = 'bulk'; document.body.appendChild(ul); for (let i = 0; i < 40; i++) { const li = document.createElement('li'); li.textContent = 'row ' + i; ul.appendChild(li); } }, 300); })()");
  const bulk = (await s.tool("idle", { tab, untilSelector: "#bulk", timeoutMs: 8000 })).v;
  check("idle settles a bulk insert", bulk.woke === "selector", bulk.woke);
  check("idle names at most eight elements", (bulk.appeared || []).length <= 8, (bulk.appeared || []).length);
  check("idle still says how many arrived", (bulk.addedCount || 0) >= 40, bulk.addedCount);

  // The quiet clock is the server's own read timeout on a socket, so a limit of
  // any size is honoured exactly. This is the case an internal slice would break.
  const q = (await s.tool("idle", { tab, maxIdleMs: 3000, timeoutMs: 8000 })).v;
  check("idle {maxIdleMs} reports an idle page", q.woke === "idleLimit", q);
  check("idle {maxIdleMs} above one second is honoured", (q.quietMs || 0) >= 3000 && (q.quietMs || 0) < 3400, q);
  // A last-look that always thinks something changed would spend a second whole
  // period before answering.
  check("idle {maxIdleMs} answers once, not twice", (q.elapsedMs || 0) < 3400, q.elapsedMs);
  check("idle {maxIdleMs} says nothing changed", q.changes === 0, q.changes);

  // A watcher that outlives its wait keeps pushing into a socket nobody reads,
  // and the next tool on this tab pays for it.
  check("idle tears its watcher down when it gives up", (await ev("String(window.__cdpWatch)")) === "null", await ev("typeof window.__cdpWatch"));

  // A shadow root renders without any light-DOM record. Reporting "idle" while
  // a component is still painting is worse than one extra read, so the quiet
  // decision looks once before it is made.
  await ev("(() => { const host = document.createElement('div'); host.id = 'comp'; const root = host.attachShadow({mode:'open'}); root.innerHTML = '<span>starting</span>'; document.body.appendChild(host); setTimeout(() => { root.querySelector('span').textContent = 'component finished rendering'; }, 120); })()");
  const sh = (await s.tool("idle", { tab, maxIdleMs: 200, timeoutMs: 6000 })).v;
  // Without the last look it would call the page idle at ~200 ms, before the
  // component had painted.
  check("idle does not call a page idle mid-render in a shadow root", sh.woke === "idleLimit" && (sh.elapsedMs || 0) >= 350, { woke: sh.woke, elapsedMs: sh.elapsedMs, textChars: sh.textChars });

  // A navigation destroys the observer. The wait has to resume in the new
  // document rather than sitting out the timeout.
  const target = page("scoped.html");
  await ev(`(() => { setTimeout(() => { location.href = ${JSON.stringify(target)}; }, 400); })()`);
  const t3 = Date.now();
  const nav = (await s.tool("idle", { tab, untilSelector: "#panel", timeoutMs: 8000 })).v;
  const waited3 = Date.now() - t3;
  check("idle survives a navigation mid-wait", nav.woke === "selector", nav);
  check("idle resumes in the new document instead of timing out", waited3 < 3000, waited3 + " ms");
});

// ---- shadow DOM and frames --------------------------------------------
// A page's own script cannot read across a shadow boundary or a site boundary,
// so these elements were invisible to `elements` and a click had nothing to
// aim at. Reported as "the button is there but elements doesn't list it".
const fport = await serveFrames();
await withTab(page("frames.html?port=" + fport), async (tab, ev) => {
  // The cross-origin frame runs in its own process and loads separately; give
  // it a moment, and prove the frame really is cross-site before blaming the
  // scanner.
  let frameReady = false;
  for (let i = 0; i < 20 && !frameReady; i++) {
    await sleep(250);
    frameReady = await ev("(() => { const f = document.getElementById('cross'); return !!(f && f.src && f.src.includes('localhost')); })()");
  }
  check("cross-origin frame is embedded from another site", frameReady, await ev("document.getElementById('cross')?.src"));
  const els = (await s.tool("elements", { tab, limit: 999 })).v.elements;
  const at = l => els.find(e => (e.label || "").trim() === l);
  const H = async () => JSON.parse(await ev("JSON.stringify(H)"));

  // Open shadow root: the button only exists inside the custom element.
  check("shadow DOM button is listed", !!at("Shadow button"), els.map(e => e.label));
  await s.tool("input", { tab, actions: [{ click: { i: at("Shadow button").i } }] });
  await sleep(250);
  check("shadow DOM button is clickable", (await H()).shadow === 1, await H());

  // A nested shadow root inside the first one.
  check("nested shadow root is listed", !!at("Deep shadow link"), els.map(e => e.label));
  await s.tool("input", { tab, actions: [{ click: { i: at("Deep shadow link").i } }] });
  await sleep(250);
  check("nested shadow root is clickable", (await H()).deep === 1, await H());

  // Same-origin iframe, including a target that needs the frame scrolled.
  check("same-origin frame button is listed", !!at("Inner near"), els.map(e => e.label));
  await s.tool("input", { tab, actions: [{ click: { i: at("Inner near").i } }] });
  await sleep(250);
  check("same-origin frame button is clickable", (await H())["same-near"] === 1, await H());

  // Cross-origin frame: its own renderer, its own target, its own session.
  const cross = els.find(e => /Cross far|Reject all/.test(e.label || ""));
  check("cross-origin frame button is listed", !!cross, els.map(e => e.label));
  if (cross) {
    await s.tool("input", { tab, actions: [{ click: { i: cross.i } }] });
    await sleep(400);
    const h = await H();
    check("cross-origin frame button is clickable", !!(h.cross || h["cross-far"]), h);
  }
});

s.close();
console.log(failed ? `\n${failed} failed` : "\nall passed");
process.exit(failed ? 1 : 0);
