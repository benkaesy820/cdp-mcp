// Minimal MCP stdio client for the end-to-end tests.
import { spawn } from "node:child_process";
export function server(bin) {
  const p = spawn(bin, [], { stdio: ["pipe", "pipe", "inherit"] });
  let buf = "", id = 0; const pend = new Map();
  p.stdout.on("data", d => { buf += d; let i; while ((i = buf.indexOf("\n")) >= 0) { const l = buf.slice(0, i); buf = buf.slice(i + 1); if (!l.trim()) continue; const m = JSON.parse(l); pend.get(m.id)?.(m); pend.delete(m.id); } });
  const rpc = (method, params) => new Promise(r => { const i = ++id; pend.set(i, r); p.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: i, method, params }) + "\n"); });
  const notify = (method, params) => p.stdin.write(JSON.stringify({ jsonrpc: "2.0", method, params }) + "\n");
  const tool = async (name, args = {}) => {
    const t = performance.now(); const m = await rpc("tools/call", { name, arguments: args }); const ms = performance.now() - t;
    const txt = m.result?.content?.[0]?.text ?? JSON.stringify(m.error);
    let v; try { v = JSON.parse(txt); } catch { v = txt; }
    return { ok: !m.result?.isError, v, ms: Math.round(ms) };
  };
  return { p, rpc, notify, tool, nextId: () => id + 1, close: () => p.kill() };
}
