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
import { readFileSync, unlinkSync } from "node:fs";
let framePort = 0;
async function serveFrames() {
  if (framePort) return framePort;
  const srv = createServer((q, res) => {
    const path = new URL(q.url, "http://x").pathname;
    if (path === "/api/save") {
      res.writeHead(500, { "content-type": "text/plain" });
      res.end("save failed");
      return;
    }
    if (path === "/api/422") {
      res.writeHead(422, { "content-type": "text/plain" });
      res.end("invalid");
      return;
    }
    if (path === "/api/slow") {
      setTimeout(() => { res.writeHead(200); res.end("slow"); }, 300);
      return;
    }
    if (path === "/api/500") {
      res.writeHead(500, { "content-type": "text/plain" });
      res.end("server error");
      return;
    }
    if (path === "/file.bin") {
      // A download that changes no DOM state: only the receipt can show it.
      res.writeHead(200, {
        "content-type": "application/octet-stream",
        "content-disposition": 'attachment; filename="cdp-mcp-e2e-download.bin"',
      });
      res.end("cdp-mcp download fixture");
      return;
    }
    if (path === "/xnav-404") {
      res.writeHead(404, { "content-type": "text/html" });
      res.end(readFileSync(join(here, "pages", "xnav-dst.html")));
      return;
    }
    res.writeHead(200, { "content-type": "text/html" });
    const files = {
      "/receipts": "receipts.html", "/sprint2": "sprint2.html", "/imgs": "imgs.html",
      "/dialogs": "dialogs.html", "/unload": "unload.html", "/download": "download.html",
      "/xnav-src": "xnav-src.html", "/xnav-dst": "xnav-dst.html",
      "/emulate": "emulate.html",
    };
    const file = files[path] || "cross.html";
    res.end(readFileSync(join(here, "pages", file)));
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
  check("idle {untilText} wakes promptly too", waited2 >= 300 && waited2 - 400 < 250, waited2 + " ms");

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
  // The frame's own target attaches after the src is set; poll the scan rather
  // than blaming the scanner for a frame that has not loaded yet.
  let els = [];
  for (let i = 0; i < 12; i++) {
    els = (await s.tool("elements", { tab, limit: 999 })).v.elements;
    if (els.some(e => /Cross far|Reject all/.test(e.label || ""))) break;
    await sleep(300);
  }
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
  const fswitch = els.find(e => (e.label || "").trim() === "Frame Switch");
  const ffield = els.find(e => (e.label || "").trim() === "Frame Field");
  check("a cross-origin frame scan reads form state the same way",
        fswitch?.checked === true && ffield?.disabled === true && ffield?.value === "y",
        { switch: fswitch, field: ffield });
  if (cross) {
    await s.tool("input", { tab, actions: [{ click: { i: cross.i } }] });
    await sleep(400);
    const h = await H();
    check("cross-origin frame button is clickable", !!(h.cross || h["cross-far"]), h);
  }

  // The frame branch is not a way around the click rules: a final confirmation
  // is guarded, and a disabled control is refused, exactly as in the page.
  const del = els.find(e => (e.label || "").trim() === "Delete");
  const g1 = await s.tool("input", { tab, actions: [{ click: { i: del?.i } }] });
  const g2 = await s.tool("input", { tab, actions: [{ click: { i: del?.i } }] });
  check("a frame click on a final confirmation is guarded like a page click",
        g1.ok && !g2.ok && /already clicked/.test(String(g2.v)),
        { first: g1.ok, second: String(g2.v).slice(0, 140) });
  const ffieldEl = els.find(e => (e.label || "").trim() === "Frame Field");
  const fdis = await s.tool("input", { tab, actions: [{ click: { i: ffieldEl?.i } }] });
  check("a disabled control in a cross-origin frame is refused",
        !fdis.ok && /"reason":"disabled"/.test(String(fdis.v)), String(fdis.v).slice(0, 140));
  // force is the documented bypass on the page path; it must be one on frames too,
  // or a refusal can never be lifted.
  const g3 = await s.tool("input", { tab, actions: [{ click: { i: del?.i } }], force: true });
  const fforce = await s.tool("input", { tab, actions: [{ click: { i: ffieldEl?.i } }], force: true });
  check("force bypasses the frame refusals", g3.ok && fforce.ok,
        { guarded: g3.ok, disabled: fforce.ok, v: String(fforce.v).slice(0, 120) });

  // Below the frame's own fold: the page cannot scroll the frame's document, so
  // clicking would dispatch outside its viewport and burn the guard for nothing.
  const offEl = els.find(e => (e.label || "").trim() === "Cross far");
  const offClick = await s.tool("input", { tab, actions: [{ click: { i: offEl?.i } }] });
  check("a frame element outside the frame's box is refused",
        !offClick.ok && /"reason":"outside_frame"/.test(String(offClick.v)),
        String(offClick.v).slice(0, 140));

  // A scoped click indexes the scoped list, which holds no frame elements; it
  // must not fall back to the tab's unscoped scan and hit the frame anyway.
  const hitsBefore = (await H()).cross || 0;
  const scopedClick = await s.tool("input", { tab, actions: [{ click: { i: cross.i, within: "body" } }] });
  await sleep(250);
  const hitsAfter = (await H()).cross || 0;
  check("a scoped click never resolves into a cross-origin frame",
        hitsAfter === hitsBefore,
        { ok: scopedClick.ok, v: String(scopedClick.v).slice(0, 120), before: hitsBefore, after: hitsAfter });
  // A scope is this document only: appending frame elements would hand the
  // caller indexes outside what it scoped, and cross-origin ones at that.
  const scopedFrame = (await s.tool("elements", { tab, within: "body", limit: 999 })).v;
  const sfEls = scopedFrame.elements || [];
  check("a scoped scan leaves cross-origin frames out",
        scopedFrame.framesScanned === 0 &&
        sfEls.some(e => (e.label || "").trim() === "Top button") &&
        !sfEls.some(e => /Cross far|Reject all/.test(e.label || "")),
        { framesScanned: scopedFrame.framesScanned, labels: sfEls.map(e => e.label).slice(0, 10) });
});

// Two tabs are cached at once. A frame click must resolve against the tab it
// names: reading whichever cache entry came first aimed the event at another
// page's session, so one of two otherwise identical clicks failed.
await withTab(page("frames.html?port=" + fport), async (tabA) => {
  const tabB = (await s.tool("tabs", { action: "open", url: page("frames.html?port=" + fport) })).v.tab;
  try {
    const frameEl = async (t) => {
      for (let i = 0; i < 20; i++) {
        const els = (await s.tool("elements", { tab: t, limit: 999 })).v.elements || [];
        const c = els.find(e => (e.label || "").trim() === "Reject all");
        if (c) return c;
        await sleep(250);
      }
      return null;
    };
    const a = await frameEl(tabA);
    const b = await frameEl(tabB);
    check("both tabs list their own frame element", !!a && !!b && a.i === b.i, { a: a?.i, b: b?.i });
    const ca = await s.tool("input", { tab: tabA, actions: [{ click: { i: a?.i } }] });
    const cb = await s.tool("input", { tab: tabB, actions: [{ click: { i: b?.i } }] });
    const hits = async (t) => JSON.parse((await s.tool("eval", { tab: t, expression: "JSON.stringify(H)" })).v.value);
    const ha = await hits(tabA), hb = await hits(tabB);
    check("a frame click lands in the tab it names, with two tabs cached",
          ca.ok && cb.ok && ha.cross === 1 && hb.cross === 1,
          { a: JSON.stringify(ca.v).slice(0, 120), b: JSON.stringify(cb.v).slice(0, 120), ha, hb });
  } finally {
    // A failed assertion must not leave a second tab behind for the next run.
    await s.tool("tabs", { action: "close", tab: tabB });
  }
});

// `images` lists only <img> that have a source, so image {i} must index that
// same list: a lazy placeholder before the real image shifted every index.
await withTab(`http://localhost:${fport}/imgs`, async (tab) => {
  const listed = (await s.tool("images", { tab })).v;
  check("images lists only images with a source",
        listed.count === 1 && /real-image\.png$/.test(listed.images?.[0]?.src || ""),
        listed.images?.map(i => i.src));
  const opened = (await s.tool("image", { tab, action: "open", i: 0 })).v;
  check("an image index matches the images list",
        /real-image\.png$/.test(opened.url || ""), opened);
  if (opened.tab) await s.tool("tabs", { action: "close", tab: opened.tab });
});

// A flow's own tab must reach its steps: they used to run on whatever tab the
// last call happened to touch.
await withTab(`http://localhost:${fport}/receipts`, async (tabA) => {
  const tabB = (await s.tool("tabs", { action: "open", url: `http://localhost:${fport}/sprint2` })).v.tab;
  try {
    await s.tool("elements", { tab: tabA, limit: 5 }); // make tabA the last tab used
    const flow = (await s.tool("flow", {
      tab: tabB,
      steps: [{ tool: "eval", args: { expression: "document.title" } }],
    })).v;
    const got = flow.trail?.[0]?.log?.[0]?.result?.value;
    check("a flow runs its steps on the tab it names", got === "cdp-mcp sprint 2", { got, tabB });
  } finally {
    await s.tool("tabs", { action: "close", tab: tabB });
  }
});

// ---- dialogs: a blocked renderer must never become a 30 s wait ------------
await withTab(`http://localhost:${fport}/dialogs`, async (tab, ev) => {
  const els = (await s.tool("elements", { tab, limit: 99 })).v.elements || [];
  const at = l => els.find(e => (e.label || "").trim() === l);
  const out = () => ev("document.getElementById('out').textContent");

  const ask = await s.tool("input", { tab, actions: [{ click: { i: at("Confirm")?.i } }] });
  check("a click that opens a confirm returns the dialog instead of hanging",
        ask.ok && ask.v?.dialog?.type === "confirm" && /Delete this item/.test(ask.v?.dialog?.message || "") && ask.ms < 4000,
        { ok: ask.ok, ms: ask.ms, dialog: ask.v?.dialog });

  const during = await s.tool("elements", { tab });
  check("another tool fails fast while a dialog is open",
        !during.ok && /dialog/.test(JSON.stringify(during.v)) && during.ms < 4000,
        { ok: during.ok, ms: during.ms, v: JSON.stringify(during.v).slice(0, 160) });

  // A batch that starts with a click cannot be the one that answers a dialog: it
  // must fail fast like any other page tool, not send the click and hang.
  const mixed = await s.tool("input", { tab, actions: [{ click: { i: at("Confirm")?.i } }, { dialog: { accept: true } }] });
  check("a batch that does not start with the dialog fails fast while one is open",
        !mixed.ok && /dialog/.test(JSON.stringify(mixed.v)) && mixed.ms < 4000,
        { ok: mixed.ok, ms: mixed.ms, v: JSON.stringify(mixed.v).slice(0, 140) });

  const no = await s.tool("input", { tab, actions: [{ dialog: { accept: false } }] });
  check("answering the dialog unblocks the page and settles the click",
        no.ok && no.v?.dialog === undefined && (await out()) === "ask:false",
        { ok: no.ok, dialog: no.v?.dialog, out: await out() });

  const pr = await s.tool("input", { tab, actions: [{ click: { i: at("Prompt")?.i } }] });
  check("a prompt is reported with its default text",
        pr.v?.dialog?.type === "prompt" && pr.v?.dialog?.defaultPrompt === "Ada", pr.v?.dialog);
  const answered = await s.tool("input", { tab, actions: [{ dialog: { accept: true, text: "Ben" } }] });
  check("a prompt answer carries the text",
        answered.ok && (await out()) === "prompt:Ben", { ok: answered.ok, out: await out() });

  const chain = await s.tool("input", { tab, actions: [{ click: { i: at("Chain")?.i } }] });
  check("the first dialog of a chain is reported", chain.v?.dialog?.type === "confirm", chain.v?.dialog);
  const chained = await s.tool("input", { tab, actions: [{ dialog: { accept: true } }] });
  check("answering a confirm reports the chained alert instead of swallowing it",
        chained.ok && chained.v?.dialog?.type === "alert" && /Second/.test(chained.v?.dialog?.message || ""),
        { ok: chained.ok, dialog: chained.v?.dialog });
  await s.tool("input", { tab, actions: [{ dialog: { accept: true } }] });
  check("the chain finishes and the handler completes", (await out()) === "chained", await out());

  // A dialog that opens while no call is in flight sits in the socket buffer.
  // The next tool must drain it before it sends anything.
  await s.tool("input", { tab, actions: [{ click: { i: at("Later")?.i } }] });
  await sleep(500);
  const late = await s.tool("elements", { tab });
  check("a dialog opened between calls is seen before the next command",
        !late.ok && /dialog/.test(JSON.stringify(late.v)) && late.ms < 4000,
        { ok: late.ok, ms: late.ms, v: JSON.stringify(late.v).slice(0, 140) });
  await s.tool("input", { tab, actions: [{ dialog: { accept: false } }] });

  // idle must report the dialog instead of sitting out its timeout.
  await s.tool("input", { tab, actions: [{ click: { i: at("Later")?.i } }] });
  const woke = await s.tool("idle", { tab, timeoutMs: 5000, maxIdleMs: 5000 });
  check("a dialog wakes idle with woke:dialog",
        woke.ok && woke.v?.woke === "dialog" && woke.v?.dialog?.type === "confirm" && woke.ms < 4000,
        { ok: woke.ok, ms: woke.ms, woke: woke.v?.woke, dialog: woke.v?.dialog });
  await s.tool("input", { tab, actions: [{ dialog: { accept: false } }] });

  // A dialog that opens *during* a settle window must end the window at once:
  // neither the quiet wait nor the Network teardown can run against a blocked
  // renderer. "Later" opens its dialog from a 150 ms timer, so it lands inside.
  const settled = await s.tool("input", { tab, actions: [{ click: { i: at("Later")?.i } }], settleMs: 1500 });
  check("a dialog that opens during settleMs ends the window instead of stalling",
        settled.ok && settled.v?.dialog?.type === "confirm" && settled.ms < 1000,
        { ok: settled.ok, ms: settled.ms, dialog: settled.v?.dialog });
  const after = await s.tool("input", { tab, actions: [{ dialog: { accept: false } }], settleMs: 800 });
  check("the dialog can be answered after a settle click", after.ok, JSON.stringify(after.v).slice(0, 140));
});

// A download changes no DOM state: the receipt is the only evidence, and it
// says started, not saved.
await withTab(`http://localhost:${fport}/download`, async (tab) => {
  const els = (await s.tool("elements", { tab, limit: 20 })).v.elements || [];
  const dl = els.find(e => (e.label || "").trim() === "Download");
  const r = await s.tool("input", { tab, actions: [{ click: { i: dl?.i } }], settleMs: 3000 });
  const got = r.v?.downloads?.[0];
  check("a click-triggered download is reported as started",
        !!got && got.status === "started" && /cdp-mcp-e2e-download/.test(got.filename || ""),
        { got, v: JSON.stringify(r.v).slice(0, 220) });
  // The browser saved it somewhere on this machine; this test must not leave it.
  for (const name of ["cdp-mcp-e2e-download.bin", ...Array.from({ length: 6 }, (_, i) => `cdp-mcp-e2e-download (${i + 1}).bin`)]) {
    try { unlinkSync(`${process.env.HOME}/Downloads/${name}`); } catch {}
  }
});

// A cross-origin navigation is the action's outcome: the settle window must stay
// open for the destination, and its failures belong to the new origin.
await withTab(`http://localhost:${fport}/xnav-src`, async (tab) => {
  const els = (await s.tool("elements", { tab, limit: 20 })).v.elements || [];
  const at = l => els.find(e => (e.label || "").trim() === l);
  const nav = await s.tool("input", { tab, actions: [{ click: { i: at("Good")?.i } }], settleMs: 2500 });
  const v = nav.v || {};
  const http = (v.errors || []).find(e => e.kind === "http" && e.status >= 500);
  check("a cross-origin navigation keeps the window open and reports the destination",
        v.navigated === true && /localhost\.:/.test(v.url || "") && !!http,
        { navigated: v.navigated, url: v.url, errors: v.errors, ms: nav.ms });
  check("a post-commit console error on the destination is reported",
        (v.errors || []).some(e => e.kind === "console" && /post-commit/.test(e.text || "")),
        v.errors);
});

// A cross-origin destination that answers 4xx is the navigation failing, not a
// stranger's request: it is reported even though the origin has just moved.
await withTab(`http://localhost:${fport}/xnav-src`, async (tab) => {
  const els = (await s.tool("elements", { tab, limit: 20 })).v.elements || [];
  const bad = els.find(e => (e.label || "").trim() === "Bad");
  const nav = await s.tool("input", { tab, actions: [{ click: { i: bad?.i } }], settleMs: 2500 });
  const http = (nav.v?.errors || []).find(e => e.kind === "http" && e.status === 404);
  check("a cross-origin document 404 is reported", !!http,
        { url: nav.v?.url, errors: nav.v?.errors });
});

// A beforeunload guard asks before a navigation leaves; the dialog arrives
// through the click, and accepting it lets the navigation finish.
await withTab(`http://localhost:${fport}/unload`, async (tab) => {
  const els = (await s.tool("elements", { tab, limit: 20 })).v.elements || [];
  const at = l => els.find(e => (e.label || "").trim() === l);
  const armed = await s.tool("input", { tab, actions: [{ click: { i: at("Add beforeunload guard")?.i } }] });
  const guardType = (await s.tool("eval", { tab, expression: "typeof window.onbeforeunload" })).v?.value;
  check("the beforeunload guard is installed",
        armed.ok && guardType === "function", { ok: armed.ok, guardType, v: JSON.stringify(armed.v).slice(0, 120) });
  const nav = await s.tool("input", { tab, actions: [{ click: { i: at("Leave")?.i } }], settleMs: 800 });
  check("a navigation guarded by beforeunload reports the dialog",
        nav.ok && nav.v?.dialog?.type === "beforeunload", { ok: nav.ok, dialog: nav.v?.dialog });
  const leave = await s.tool("input", { tab, actions: [{ dialog: { accept: true } }], settleMs: 1200 });
  const href = (await s.tool("eval", { tab, expression: "location.href" })).v?.value || "";
  check("accepting beforeunload lets the navigation finish",
        leave.ok && /\/imgs$/.test(href), { ok: leave.ok, href, v: JSON.stringify(leave.v).slice(0, 120) });
  const close = await s.tool("tabs", { action: "close", tab });
  check("a plain close is confirmed by the browser", close.ok && close.v?.closed === true, close.v);
});
// ---- in-tab navigation: goto, back, forward, reload ----------------------
await withTab(`http://localhost:${fport}/sprint2`, async (tab) => {
  // A scope and a delta baseline exist before the navigation; neither may
  // survive it, or an old index would resolve against the old page.
  await s.tool("elements", { tab, within: "#modal-form", limit: 20 });

  const g = await s.tool("tabs", { action: "goto", url: `http://localhost:${fport}/imgs`, tab });
  check("goto moves the tab and reports the new url",
        g.ok && g.v?.moved === true && /\/imgs$/.test(g.v?.url || ""), g.v);

  const stale = await s.tool("input", { tab, actions: [{ click: { i: 99 } }] });
  check("a document navigation clears the tab's click scope",
        !stale.ok && !/within/.test(String(stale.v)), String(stale.v).slice(0, 160));

  const d = (await s.tool("elements", { tab, mode: "delta" })).v;
  check("a document navigation clears the delta baseline", d.baseline === false, d);

  const title = (await s.tool("eval", { tab, expression: "document.title" })).v?.value;
  check("the new document is what the page answers with", title === "images", title);

  const h = await s.tool("tabs", { action: "goto", url: `http://localhost:${fport}/imgs#part`, tab });
  check("a hash-only goto returns at once as same-document",
        h.ok && h.v?.moved === true && h.v?.sameDocument === true && h.ms < 2000, h.v);

  const backHash = await s.tool("tabs", { action: "back", tab });
  check("a same-document history move is reported as same-document",
        backHash.ok && backHash.v?.moved === true && backHash.v?.sameDocument === true && backHash.ms < 3000,
        { v: backHash.v, ms: backHash.ms });

  const fwdHash = await s.tool("tabs", { action: "forward", tab });
  check("forward returns to the hash entry",
        fwdHash.ok && fwdHash.v?.moved === true && /#part$/.test(fwdHash.v?.url || "") && fwdHash.ms < 3000,
        { v: fwdHash.v, ms: fwdHash.ms });

  const bad = await s.tool("tabs", { action: "goto", url: "http://cdp-mcp-e2e.invalid/", tab });
  check("an unreachable host is an error, not a success",
        !bad.ok && /ERR_NAME_NOT_RESOLVED|net::ERR_/.test(String(bad.v)), String(bad.v).slice(0, 160));
});

await withTab(`http://localhost:${fport}/sprint2`, async (tab) => {
  const first = await s.tool("tabs", { action: "back", tab });
  check("back at the start of history is an honest no-op",
        first.ok && first.v?.moved === false, first.v);

  await s.tool("tabs", { action: "goto", url: `http://localhost:${fport}/imgs`, tab });
  const back = await s.tool("tabs", { action: "back", tab });
  check("back returns to the previous document",
        back.ok && back.v?.moved === true && /sprint2$/.test(back.v?.url || ""), back.v);

  const fwd = await s.tool("tabs", { action: "forward", tab });
  check("forward returns to the next document",
        fwd.ok && fwd.v?.moved === true && /imgs$/.test(fwd.v?.url || ""), fwd.v);

  const last = await s.tool("tabs", { action: "forward", tab });
  check("forward at the end of history is an honest no-op",
        last.ok && last.v?.moved === false, last.v);

  const rl = await s.tool("tabs", { action: "reload", tab, ignoreCache: true });
  check("reload with ignoreCache commits the same document",
        rl.ok && rl.v?.moved === true, rl.v);

  // Back to back, with no sleep: the navigation waits for DOMContentLoaded, so
  // the page is out of the "not attached" window by the time the second starts.
  const rl2 = await s.tool("tabs", { action: "reload", tab });
  check("a plain reload commits too", rl2.ok && rl2.v?.moved === true, rl2.v);

  const gi = await s.tool("tabs", { action: "goto", url: `http://localhost:${fport}/imgs`, tab, idle: true });
  check("goto with idle waits for loading to settle", gi.ok && gi.v?.idle === true, gi.v);
});

// A beforeunload guard turns a navigation into a dialog, not a timeout.
await withTab(`http://localhost:${fport}/unload`, async (tab) => {
  const els = (await s.tool("elements", { tab, limit: 20 })).v.elements || [];
  const guard = els.find(e => (e.label || "").trim() === "Add beforeunload guard");
  await s.tool("input", { tab, actions: [{ click: { i: guard?.i } }] });
  const nav = await s.tool("tabs", { action: "goto", url: `http://localhost:${fport}/imgs`, tab });
  check("a beforeunload guard makes goto report the dialog",
        nav.ok && nav.v?.dialog?.type === "beforeunload" && nav.v?.moved === false,
        { v: nav.v, ms: nav.ms });
  const leave = await s.tool("input", { tab, actions: [{ dialog: { accept: true } }], settleMs: 1500 });
  const href = (await s.tool("eval", { tab, expression: "location.href" })).v?.value || "";
  check("accepting it lets the navigation finish",
        leave.ok && /\/imgs$/.test(href), { ok: leave.ok, href });
});

// ---- emulate: viewport, touch and media ----------------------------------
await withTab(`http://localhost:${fport}/emulate`, async (tab) => {
  const read = async () => JSON.parse((await s.tool("eval", { tab, expression:
    "JSON.stringify({ w: innerWidth, h: innerHeight, dpr: devicePixelRatio, \
       mtp: navigator.maxTouchPoints, ot: 'ontouchstart' in window, \
       dark: matchMedia('(prefers-color-scheme: dark)').matches, \
       reduce: matchMedia('(prefers-reduced-motion: reduce)').matches, \
       print: matchMedia('print').matches })" })).v?.value);
  const before = await read();
  const near = (a, b) => Math.abs(a - b) < 0.01;

  const mob = (await s.tool("emulate", { tab, preset: "mobile" })).v;
  const m = await read();
  check("a mobile preset changes the page's own viewport, dpr and touch",
        m.w === 390 && m.h === 844 && near(m.dpr, 3) && m.mtp === 1,
        { applied: mob?.viewport, measured: mob?.measured, page: m });
  check("the tool says touch needs a reload to reach the page",
        mob?.touchNeedsReload === true, mob);

  // Every coordinate moved, so the previous scan's baseline is worthless.
  const dd = (await s.tool("elements", { tab, mode: "delta" })).v;
  check("a viewport change drops the delta baseline", dd.baseline === false, dd);

  const scan = (await s.tool("elements", { tab, limit: 5 })).v;
  check("the scan reports the emulated viewport height", scan.scroll?.vh === 844, scan.scroll);

  // Touch events are created with the document: a reload is what makes
  // 'ontouchstart' true, and the tool reported exactly that above.
  await s.tool("tabs", { action: "reload", tab });
  const afterReload = await read();
  check("after the reload the page sees touch events",
        afterReload.ot === true && afterReload.mtp === 1, afterReload);

  const ex = (await s.tool("emulate", { tab, width: 800, height: 600, colorScheme: "dark" })).v;
  const d = await read();
  check("explicit metrics apply and colorScheme reaches matchMedia",
        d.w === 800 && d.h === 600 && d.dark === true, { applied: ex?.viewport, page: d });

  await s.tool("emulate", { tab, reducedMotion: "reduce" });
  const both = await read();
  check("a second media call keeps the first feature",
        both.dark === true && both.reduce === true, both);

  await s.tool("emulate", { tab, media: "print" });
  const pr = await read();
  check("media:print is what the page sees", pr.print === true, pr);

  // Leave touch on so reset has something to turn off, which is the case the
  // reload note exists for.
  await s.tool("emulate", { tab, touch: true });
  const reset = (await s.tool("emulate", { tab, reset: true })).v;
  const after = await read();
  check("reset restores the real viewport",
        after.w === before.w && after.h === before.h && near(after.dpr, before.dpr),
        { before, after });
  check("reset drops the emulated media",
        after.print === false && after.dark === before.dark && after.reduce === before.reduce,
        { before, after });
  check("reset reports that touch needs a reload to leave the page",
        reset?.touchNeedsReload === true, reset);
});

await withTab(page("chatty.html"), async (tab, ev) => {
  const before = Number(await ev("window.__ticks")) || 0;
  const r = (await s.tool("idle", { tab, maxIdleMs: 1500, timeoutMs: 4000 })).v;
  const after = Number(await ev("window.__ticks")) || 0;
  const moved = after - before;
  check("a chatty page keeps the wait awake",
        r.woke === "timeout" && moved > 15 && r.elapsedMs >= 3900,
        { woke: r.woke, moved, elapsedMs: r.elapsedMs });
  const cap = Math.ceil(r.elapsedMs / 80) + 3;
  check("mutation reports are coalesced", r.changes <= cap && r.changes >= 3, { changes: r.changes, cap, moved });

  const t0 = Date.now();
  await s.tool("eval", {
    tab,
    expression: "(() => { setTimeout(() => { const d = document.createElement('div'); d.setAttribute('role','dialog'); d.setAttribute('aria-label','Chatty dialog'); document.body.appendChild(d); }, 300); })()",
  });
  const hit = (await s.tool("idle", { tab, untilSelector: "[role=dialog]", timeoutMs: 4000 })).v;
  const ms = Date.now() - t0;
  check("a hit on a chatty page is still immediate",
        hit.woke === "selector" && ms < 900 && (hit.appeared || []).some(a => a.label === "Chatty dialog"),
        { woke: hit.woke, ms, appeared: hit.appeared });

  // A text hit on a page that never stops moving: the 50 ms cap must not stop
  // the wake, only the number of layouts it costs.
  const tText2 = Date.now();
  await s.tool("eval", {
    tab,
    expression: "(() => { setTimeout(() => { const p = document.createElement('p'); p.textContent = 'Chatty text arrived'; document.body.appendChild(p); }, 220); })()",
  });
  const th = (await s.tool("idle", { tab, untilText: "Chatty text arrived", timeoutMs: 4000 })).v;
  const thMs = Date.now() - tText2;
  check("a text hit on a moving page still lands quickly",
        th.woke === "text" && thMs < 900, { woke: th.woke, ms: thMs });
});

// A cancelled wait used to leave its MutationObserver in the page: it kept
// reading innerText on every mutation and pushing into a socket nobody read,
// for the rest of the tab's life.
await withTab(page("basic.html"), async (tab, ev) => {
  // The main server is inside the wait, and it is single-threaded, so a second
  // MCP process is the only honest way to observe the page mid-wait.
  const probe = server(bin);
  await probe.rpc("initialize", { protocolVersion: "2024-11-05" });
  const watching = async () => (await probe.tool("eval", { tab, expression: "!!window.__cdpWatch" })).v?.value;

  const id = s.nextId();
  s.p.stdin.write(JSON.stringify({
    jsonrpc: "2.0", id, method: "tools/call",
    params: { name: "idle", arguments: { tab, untilSelector: "[data-never]", timeoutMs: 30000 } },
  }) + "\n");
  await sleep(300);
  const armed = await watching();
  check("the cancelled wait had installed its observer", armed === true, armed);
  s.notify("notifications/cancelled", { requestId: id });
  await sleep(800);
  const left = await ev("!!window.__cdpWatch");
  check("a cancelled wait leaves no observer behind", left === false, left);
  probe.close();

  // maxIdleMs defaults to timeoutMs, so both thresholds land on the same pass.
  const still = (await s.tool("idle", { tab, maxIdleMs: 400, timeoutMs: 400 })).v;
  check("a quiet wait at the timeout boundary says idle, not timeout",
        still.woke === "idleLimit" && still.changes === 0 && still.elapsedMs >= 380, still);

  // Attribute churn is not activity: a text wait used to treat every toggled
  // class as a change, so a page that only churns attributes could never reach
  // maxIdleMs and ran to the full timeout instead.
  await s.tool("eval", {
    tab,
    expression: "(() => { const d = document.createElement('div'); d.id = 'churn'; document.body.appendChild(d); window.__churn = setInterval(() => d.setAttribute('data-n', String(Date.now())), 20); })()",
  });
  const churn = (await s.tool("idle", { tab, untilText: "never appears here", maxIdleMs: 800, timeoutMs: 4000 })).v;
  check("attribute churn is not activity for a text wait",
        churn.woke === "idleLimit" && churn.elapsedMs < 2000, churn);
  await s.tool("eval", { tab, expression: "clearInterval(window.__churn)" });

  // Text that appears by removing an attribute produces no childList record.
  const tText = Date.now();
  await s.tool("eval", {
    tab,
    expression: "(() => { const d = document.createElement('div'); d.id='reveal'; d.hidden = true; d.textContent = 'Revealed by attribute'; document.body.appendChild(d); setTimeout(() => { d.hidden = false; }, 300); })()",
  });
  const rev = (await s.tool("idle", { tab, untilText: "Revealed by attribute", timeoutMs: 3000 })).v;
  const revMs = Date.now() - tText;
  check("text revealed by an attribute wakes the wait",
        rev.woke === "text" && revMs < 1200, { woke: rev.woke, ms: revMs });

  // The reveal can land inside the 50 ms window opened by unrelated attribute
  // churn. A skipped check must be deferred, not dropped: when the reveal is
  // the last mutation, nothing else would ever wake the wait.
  const tDef = Date.now();
  await s.tool("eval", {
    tab,
    expression: `(() => {
      const d = document.createElement('div');
      d.id = 'deferred'; d.hidden = true; d.textContent = 'Deferred reveal';
      document.body.appendChild(d);
      setTimeout(() => d.setAttribute('data-noise', '1'), 200);
      setTimeout(() => { d.hidden = false; }, 220);
    })()`,
  });
  const def = (await s.tool("idle", { tab, untilText: "Deferred reveal", timeoutMs: 3000 })).v;
  const defMs = Date.now() - tDef;
  check("a reveal inside the text-check window is not swallowed",
        def.woke === "text" && defMs < 1200, { woke: def.woke, ms: defMs });

  // A report that lands inside the 80 ms window must be flushed, not dropped:
  // the last text change is otherwise invisible, and the quiet check then waits
  // a second full maxIdleMs before it believes the page has stopped.
  await s.tool("eval", {
    tab,
    expression: `(() => {
      const add = (t) => { const p = document.createElement('p'); p.textContent = t; document.body.appendChild(p); };
      setTimeout(() => add('tail one'), 120);
      setTimeout(() => add('tail two'), 150);
    })()`,
  });
  const tTail = Date.now();
  const tail = (await s.tool("idle", { tab, maxIdleMs: 500, timeoutMs: 4000 })).v;
  const tailMs = Date.now() - tTail;
  const tailTexts = (tail.appeared || []).map(a => a.text);
  check("a report inside the coalescing window is flushed, not dropped",
        tailTexts.includes("tail one") && tailTexts.includes("tail two") && tailMs < 950,
        { ms: tailMs, woke: tail.woke, appeared: tailTexts });

  // `checked` is a property, not an attribute: no MutationObserver record. The
  // last look at the boundary is the only thing that can see it, and it has to
  // name the element.
  await s.tool("eval", {
    tab,
    expression: `(() => {
      const i = document.createElement('input');
      i.type = 'checkbox'; i.id = 'agree'; i.name = 'agree';
      document.body.appendChild(i);
      setTimeout(() => { i.checked = true; }, 300);
    })()`,
  });
  const tSel = Date.now();
  const sel = (await s.tool("idle", { tab, untilSelector: "#agree:checked", maxIdleMs: 700, timeoutMs: 3000 })).v;
  const selMs = Date.now() - tSel;
  check("a selector matched by a property change wakes at the boundary",
        sel.woke === "selector" && selMs < 1600 && (sel.appeared || []).some(a => a.label === "agree"),
        { woke: sel.woke, ms: selMs, appeared: sel.appeared });

  // The boundary probe must leave the watcher alive. A shadow-root change is
  // invisible to the light-DOM observer, so the quiet check looks, finds it and
  // continues; the later light-DOM change must still wake the same wait.
  await s.tool("eval", {
    tab,
    expression: `(() => {
      const host = document.createElement('div'); host.id = 'shadowhost';
      const sr = host.attachShadow({ mode: 'open' });
      sr.innerHTML = '<span>one</span>';
      document.body.appendChild(host);
      setTimeout(() => { sr.querySelector('span').textContent = 'two-two-two'; }, 350);
      setTimeout(() => { const d = document.createElement('div'); d.setAttribute('role','dialog'); d.setAttribute('aria-label','After probe'); document.body.appendChild(d); }, 700);
    })()`,
  });
  const tProbe = Date.now();
  const probeHit = (await s.tool("idle", { tab, untilSelector: "[aria-label='After probe']", maxIdleMs: 500, timeoutMs: 3000 })).v;
  const probeMs = Date.now() - tProbe;
  check("the boundary probe leaves the watcher alive",
        probeHit.woke === "selector" && probeMs < 860, { woke: probeHit.woke, ms: probeMs });

  // Indentation and the whitespace between tags must not eat the label budget.
  await s.tool("eval", {
    tab,
    expression: `(() => {
      const d = document.createElement('div'); d.id = 'pretty';
      d.innerHTML = '<div>\\n</div>'.repeat(120) + '<span>Deep label</span>';
      document.body.appendChild(d);
    })()`,
  });
  const pretty = (await s.tool("idle", { tab, untilSelector: "#pretty", timeoutMs: 3000 })).v;
  check("a label survives leading whitespace nodes",
        (pretty.appeared || []).some(a => a.text === "Deep label"),
        (pretty.appeared || []).map(a => a.text));

  // Adjacent inline runs have no whitespace in the source, so none may be
  // invented: <span>$</span><span>42</span> is "$42", not "$ 42".
  await s.tool("eval", {
    tab,
    expression: `(() => {
      const d = document.createElement('div'); d.id = 'inline';
      const a = document.createElement('span'); a.textContent = '$';
      const b = document.createElement('span'); b.textContent = '42';
      d.appendChild(a); d.appendChild(b); document.body.appendChild(d);
    })()`,
  });
  const inline = (await s.tool("idle", { tab, untilSelector: "#inline", timeoutMs: 3000 })).v;
  check("inline runs are not split by an invented space",
        (inline.appeared || []).some(a => a.text === "$42"),
        (inline.appeared || []).map(a => a.text));
});

// A rich editor can accept a paste event and insert nothing; the tool used to
// report success anyway. Gemini's Quill editor does exactly this.
await withTab(page("paste-eater.html"), async (tab, ev) => {
  await ev("document.getElementById('ed').focus()");
  const r = (await s.tool("paste", { tab, text: "pasted by the fallback" })).v;
  const got = await ev("document.getElementById('ed').innerText");
  check("a swallowed paste falls back to typed input",
        /inserted as typed text/.test(r.via || "") && got.includes("pasted by the fallback"),
        { via: r.via, got });

  // A button has a string `value`, which used to count as editable.
  await ev("document.getElementById('btn').focus()");
  const bad = await s.tool("paste", { tab, text: "should not land" });
  check("a paste into a non-editable element is an error, not a fake success",
        !bad.ok && /not editable/.test(String(bad.v)), { ok: bad.ok, v: String(bad.v).slice(0, 120) });
});

// A handler can cancel the paste and commit on the next task. A synchronous
// read that says "unchanged" must not trigger a second insert.
await withTab(page("paste-async.html"), async (tab, ev) => {
  await ev("document.getElementById('ed').focus()");
  const r = (await s.tool("paste", { tab, text: "by the async handler" })).v;
  const got = await ev("document.getElementById('ed').innerText");
  check("an async paste handler is not double-pasted",
        r.via === "paste event (delayed)" && got === "startby the async handler",
        { via: r.via, got });
});

// The insert can be blocked after the paste event was refused. Success must
// mean the text actually landed.
await withTab(page("paste-blocked.html"), async (tab, ev) => {
  await ev("document.getElementById('ed').focus()");
  const bad = await s.tool("paste", { tab, text: "will not land" });
  check("a blocked insert is an error, not a fake success",
        !bad.ok && /did not take/.test(String(bad.v)), { ok: bad.ok, v: String(bad.v).slice(0, 140) });
});

// Turn receipts: opt-in settle, same-origin, noise filtered.
const rport = await serveFrames();
await withTab(`http://127.0.0.1:${rport}/receipts`, async (tab) => {
  const els = (await s.tool("elements", { tab, limit: 200 })).v.elements || [];
  const at = (label) => els.find(e => e.label === label)?.i;
  const save = (await s.tool("input", { tab, actions: [{ click: { i: at("Save") } }], settleMs: 1500 })).v;
  const sk = (save.errors || []).map(e => `${e.kind}:${e.status ?? e.text ?? e.error}`);
  check("a click receipt names a silent 500", sk.some(k => k === "http:500"), sk);
  check("a click receipt names the uncaught exception", sk.some(k => k.startsWith("js:") && /save failed/i.test(k)), sk);

  const noisy = (await s.tool("input", { tab, actions: [{ click: { i: at("Noisy") } }], settleMs: 1500 })).v;
  const nk = (noisy.errors || []).map(e => `${e.kind}:${e.status ?? e.text ?? e.error}:${e.url || ""}`);
  check("a same-origin 422 is kept", nk.some(k => k.includes("http:422")), nk);
  check("third-party and aborted requests are filtered",
        !nk.some(k => k.includes("tracker.invalid") || /ERR_ABORTED/.test(k)), nk);

  const con = (await s.tool("input", { tab, actions: [{ click: { i: at("Console") } }], settleMs: 800 })).v;
  const ck = (con.errors || []).map(e => `${e.kind}:${e.text || e.status}`);
  check("a console.error is a receipt", ck.some(k => k === "console:console boom"), ck);

  await s.tool("input", { tab, actions: [{ click: { i: at("Later") } }] });
  const delta = (await s.tool("idle", { tab, maxIdleMs: 700, timeoutMs: 2000 })).v;
  const texts = (delta.appeared || []).map(a => a.text || "");
  check("a wait reports the text that changed",
        texts.some(t => /Coupon expired/.test(t)) && texts.some(t => /\$45/.test(t)), texts);

  await s.tool("input", { tab, actions: [{ click: { i: at("NoiseDom") } }] });
  const nd = (await s.tool("idle", { tab, maxIdleMs: 600, timeoutMs: 2000 })).v;
  check("comment and whitespace nodes are not reported as a change",
        !(nd.appeared || []).some(a => a.tag === "body"), nd.appeared);
});

// Timeout diagnostics: the near miss, not a rescan.
await withTab(`http://127.0.0.1:${rport}/receipts`, async (tab) => {
  const near = (await s.tool("idle", { tab, untilSelector: "[role=dialog]", maxIdleMs: 5000, timeoutMs: 500 })).v;
  check("a timed-out selector reports the near miss",
        near.woke === "timeout" && (near.near?.dialogs || []).some(d => d.label === "Settings"),
        near.near);
  const text = (await s.tool("idle", { tab, untilText: "Saved changes", maxIdleMs: 5000, timeoutMs: 500 })).v;
  check("a timed-out text wait reports the closest text",
        text.woke === "timeout" && /Save changes/.test(text.near?.textNear || ""),
        text.near);
  const pseudo = (await s.tool("idle", { tab, untilSelector: "button#save:disabled", maxIdleMs: 5000, timeoutMs: 500 })).v;
  check("a pseudo-class miss reports the real element",
        pseudo.woke === "timeout" && (pseudo.near?.selectorMatches || []).some(m => m.tag === "button"),
        pseudo.near);
  const scriptOnly = (await s.tool("idle", { tab, untilText: "tracker invalid", maxIdleMs: 5000, timeoutMs: 500 })).v;
  check("diagnostics ignore inline script text",
        !(scriptOnly.near?.textNear || "").length, scriptOnly.near);
});

// ---- sprint 2: honest actions and scoped state -----------------------------
await withTab(`http://127.0.0.1:${rport}/sprint2`, async (tab, ev) => {
  const els = (await s.tool("elements", { tab, limit: 999 })).v.elements || [];
  const at = l => els.find(e => (e.label || "").trim() === l);

  const blocked = await s.tool("input", { tab, actions: [{ click: { i: at("Covered")?.i } }] });
  const bv = String(blocked.v);
  check("an obscured click is blocked and names the blocker",
        !blocked.ok && /"blocked":true/.test(bv) && /Cookie Banner/.test(bv), bv.slice(0, 200));

  const forced = await s.tool("input", { tab, actions: [{ click: { i: at("Covered")?.i } }], force: true });
  check("a forced click bypasses the obstruction check", forced.ok, String(forced.v).slice(0, 160));

  const dis = await s.tool("input", { tab, actions: [{ click: { i: at("Disabled")?.i } }] });
  check("a disabled click is blocked",
        !dis.ok && /"reason":"disabled"/.test(String(dis.v)), String(dis.v).slice(0, 160));

  // A native checkbox with role=switch keeps its own checked state: the ARIA
  // fallback is for custom controls, not a replacement for the real one.
  check("role=switch does not overwrite a native checked state",
        at("Native Switch")?.checked === true, at("Native Switch"));

  // The browser's own :disabled rules: a fieldset disables its controls, but
  // not a link inside it, nor a button inside its first legend.
  const fieldEl = at("Field Input"), linkEl = at("Inside Link"), legendEl = at("Legend Button");
  check("a disabled fieldset disables controls, not links or its legend",
        !!fieldEl && fieldEl.disabled === true &&
        !!linkEl && linkEl.disabled !== true &&
        !!legendEl && legendEl.disabled !== true,
        { field: fieldEl, link: linkEl, legend: legendEl });

  const inField = await s.tool("input", { tab, actions: [{ click: { i: at("Field Input")?.i } }] });
  check("a control in a disabled fieldset is refused, matching the scan",
        !inField.ok && /"reason":"disabled"/.test(String(inField.v)), String(inField.v).slice(0, 160));

  const inLink = await s.tool("input", { tab, actions: [{ click: { i: at("Inside Link")?.i } }] });
  await sleep(120);
  check("a link inside a disabled fieldset still clicks",
        inLink.ok && (await ev("window.__inside || 0")) === 1,
        { ok: inLink.ok, hit: await ev("window.__inside || 0"), v: String(inLink.v).slice(0, 120) });

  // The first legend of a fieldset is outside its disabled scope, so its
  // controls stay usable; the closest('fieldset[disabled]') walk got this wrong.
  const inLegend = await s.tool("input", { tab, actions: [{ click: { i: at("Legend Button")?.i } }] });
  await sleep(120);
  check("a button in a disabled fieldset's legend still clicks",
        inLegend.ok && (await ev("window.__legend || 0")) === 1,
        { ok: inLegend.ok, hit: await ev("window.__legend || 0"), v: String(inLegend.v).slice(0, 120) });

  await s.tool("input", { tab, actions: [{ click: { i: at("Icon Save")?.i } }] });
  await sleep(150);
  check("a click on a child of the target is allowed",
        (await ev("window.__icon || 0")) === 1, await ev("window.__icon || 0"));

  const form = (await s.tool("elements", { tab, within: "#modal-form", limit: 99 })).v;
  const fels = form.elements || [];
  const f = l => fels.find(e => (e.label || "").trim() === l);
  check("a scoped scan returns only the form",
        fels.length === 4 && !fels.some(e => /Go|Covered|NewTab/.test(e.label || "")),
        fels.map(e => e.label));
  check("form state is reported, with the password masked",
        f("Name")?.value === "Ada" && f("Secret")?.value === "***" &&
        f("Agree")?.checked === true && f("Plan")?.selected === "Pro",
        { name: f("Name"), secret: f("Secret"), agree: f("Agree"), plan: f("Plan") });

  check("a scoped scan echoes its scope",
        form.scope === "#modal-form" && form.scopeMissing === false,
        { scope: form.scope, scopeMissing: form.scopeMissing });

  // A scope that matches nothing must not come back as the whole page: that is
  // how an agent clicks something it never asked about.
  const miss = (await s.tool("elements", { tab, within: "#not-mounted", limit: 99 })).v;
  check("a scope that matches nothing returns no elements, not the whole page",
        (miss.elements || []).length === 0 && miss.total === 0 && miss.scopeMissing === true &&
        miss.suspect === null,
        { total: miss.total, scopeMissing: miss.scopeMissing, suspect: miss.suspect,
          labels: (miss.elements || []).map(e => e.label) });

  // Same rule for text: a missing scope is not the body.
  const tmiss = (await s.tool("text", { tab, within: "#not-mounted" })).v;
  check("a text scope that matches nothing returns no text, not the whole page",
        (tmiss.text || "").length === 0 && tmiss.scopeMissing === true,
        { chars: tmiss.chars, scopeMissing: tmiss.scopeMissing, head: (tmiss.text || "").slice(0, 60) });

  // Overlays are page-wide by design, so a stale scope must not hide a modal;
  // and delta is the mode used to verify a click, so it must not read a missing
  // scope as "nothing changed".
  check("a missing scope still reports page-wide overlays",
        JSON.stringify(miss.overlays || []).includes("Cookie Banner"), miss.overlays);
  const dmiss = (await s.tool("elements", { tab, within: "#not-mounted", mode: "delta" })).v;
  check("a delta scan reports a missing scope too",
        dmiss.scopeMissing === true, { scope: dmiss.scope, scopeMissing: dmiss.scopeMissing });

  // A baseline from a different scope cannot be compared: everything would read
  // as added and everything old as removed.
  const dscope = (await s.tool("elements", { tab, within: "#off-box", mode: "delta" })).v;
  check("a delta against another scope reports no baseline",
        dscope.baseline === false && dscope.scopeChanged === true,
        { baseline: dscope.baseline, scopeChanged: dscope.scopeChanged, added: dscope.addedCount });

  // An empty selector is no selector, not "everything".
  const emptyScope = (await s.tool("elements", { tab, within: "", limit: 999 })).v;
  const emptyText = (await s.tool("text", { tab, within: "" })).v;
  check("an empty within means the whole page",
        emptyScope.scope === null && emptyScope.scopeMissing === false && emptyScope.total > 0 &&
        (emptyText.text || "").length > 0 && emptyText.scopeMissing === false,
        { scope: emptyScope.scope, scopeMissing: emptyScope.scopeMissing, total: emptyScope.total, chars: emptyText.chars });

  const badWithin = await s.tool("text", { tab, within: 5 });
  check("text rejects a non-string within instead of ignoring it",
        !badWithin.ok && /CSS selector/.test(String(badWithin.v)), String(badWithin.v).slice(0, 100));

  // The scoped scan above set the click scope to #modal-form. A real caller
  // rescans before using indexes from the page again; so does this test.
  const els2 = (await s.tool("elements", { tab, limit: 999 })).v.elements || [];
  const at2 = l => els2.find(e => (e.label || "").trim() === l);

  const nt = await s.tool("input", { tab, actions: [{ click: { i: at2("NewTab")?.i } }], settleMs: 1800 });
  const ntv = nt.v || {};
  check("a target=_blank click reports the opened tab",
        !!ntv.openedTab?.tab && /\/receipts$/.test(ntv.openedTab?.url || ""), ntv.openedTab);
  if (ntv.openedTab?.tab) await s.tool("tabs", { action: "close", tab: ntv.openedTab.tab });

  const nav = await s.tool("input", { tab, actions: [{ click: { i: at2("Go")?.i } }], settleMs: 1800 });
  const nv = nav.v || {};
  check("a same-tab navigation is reported with the new URL",
        nv.navigated === true && /\/receipts$/.test(nv.url || ""),
        { navigated: nv.navigated, url: nv.url });
});

s.close();
console.log(failed ? `\n${failed} failed` : "\nall passed");
process.exit(failed ? 1 : 0);
