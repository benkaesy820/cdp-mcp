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

s.close();
console.log(failed ? `\n${failed} failed` : "\nall passed");
process.exit(failed ? 1 : 0);
