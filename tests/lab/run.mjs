// Score `idle` against the lab page: how long after the change did it return,
// and was what it said enough to act on without looking at the page again?
//
//   cargo build --release
//   node tests/lab/run.mjs [path/to/cdp-mcp]
//
// Latency is measured from the real click (the `input` tool, not a JS .click)
// to the moment `idle` returns, minus the delay the activity is known to take.
import { server } from "../e2e/mcp.mjs";
import { lab, CASES } from "./server.mjs";

const bin = process.argv[2];
if (!bin) {
  console.error("usage: node tests/lab/run.mjs /path/to/cdp-mcp");
  process.exit(2);
}

const host = await lab();
const s = server(bin);
await s.rpc("initialize", { protocolVersion: "2024-11-05" });

// Only the fields a caller would actually use, so the table stays readable.
const brief = (r) => ({
  woke: r.woke,
  changes: r.changes ?? 0,
  quietMs: r.quietMs,
  ...(r.dialogs ? { dialogs: r.dialogs } : {}),
  ...(r.textChars ? { textChars: r.textChars, textDelta: r.textDelta ?? 0 } : {}),
  ...(r.navigated ? { navigated: true } : {}),
  appeared: (r.appeared || []).length,
  ...((r.appeared || []).length ? { named: (r.appeared || []).slice(0, 2) } : {}),
});

const rows = [];
for (const c of CASES) {
  const rec = { case: c.name };
  const open = await s.tool("tabs", { action: "open", url: host.base + "/" });
  const tab = open.v?.tab;
  if (!tab) {
    rec.error = "could not open a tab";
    rows.push(rec);
    continue;
  }
  try {
    // Let the page finish its own load before the clock starts.
    await s.tool("idle", { tab, maxIdleMs: 400, timeoutMs: 5000 });

    const els = (await s.tool("elements", { tab, limit: 200 })).v?.elements || [];
    const hit = els.find((e) => (e.label || "").trim() === c.button);
    if (!hit) {
      rec.error = "button not found: " + c.button;
      rows.push(rec);
      continue;
    }

    const t0 = Date.now();
    await s.tool("input", { tab, actions: [{ click: { i: hit.i } }] });
    const r = (await s.tool("idle", { tab, ...c.idle, timeoutMs: 12_000 })).v || {};
    const wall = Date.now() - t0;

    rec.wallMs = wall;
    rec.latenessMs = c.delayMs ? wall - c.delayMs : null;
    rec.reported = brief(r);
    rec.quality = !!(c.expect && c.expect(r));
  } finally {
    await s.tool("tabs", { action: "close", tab });
  }
  rows.push(rec);
}

s.close();
host.close();

const timed = rows.filter((r) => typeof r.latenessMs === "number" && r.latenessMs !== null);
const mean = (a) => (a.length ? Math.round(a.reduce((x, y) => x + y, 0) / a.length) : null);
const pad = (s, n) => String(s).padEnd(n).slice(0, n);
const padl = (s, n) => String(s).padStart(n).slice(0, n);

console.log("");
console.log("  " + pad("activity", 34) + padl("lateness", 10) + padl("woke", 12) + padl("changes", 9) + padl("named", 7) + "  enough?");
console.log("  " + "-".repeat(34) + " " + "-".repeat(9) + " " + "-".repeat(11) + " " + "-".repeat(8) + " " + "-".repeat(6) + "  -------");
for (const r of rows) {
  if (r.error) {
    console.log("  " + pad(r.case, 34) + "  " + r.error);
    continue;
  }
  const late = r.latenessMs === null ? "-" : r.latenessMs + " ms";
  console.log(
    "  " + pad(r.case, 34)
    + padl(late, 10)
    + padl(r.reported.woke ?? "?", 12)
    + padl(r.reported.changes ?? 0, 9)
    + padl(r.reported.appeared ?? 0, 7)
    + "  " + (r.quality ? "yes" : "NO"),
  );
}
console.log("");
console.log(JSON.stringify({
  binary: bin,
  meanLatenessMs: mean(timed.map((r) => r.latenessMs)),
  maxLatenessMs: timed.length ? Math.max(...timed.map((r) => r.latenessMs)) : null,
  qualityPassed: rows.filter((r) => r.quality).length,
  cases: rows.length,
  rows,
}, null, 2));
if (rows.some((r) => r.quality !== true)) process.exitCode = 1;
