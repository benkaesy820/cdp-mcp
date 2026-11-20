// A page with activities to measure `idle` against, and the server that serves
// it. No dependencies.
//
// Each activity is a shape a real app produces: a popup after a delay, text
// streaming in, a list growing, a form mounting, a navigation, and attribute
// churn that changes nothing on screen. Every one has a declared expectation,
// so a run can score not just how fast the wake came back but whether what it
// said was enough to act on without looking again.
import { createServer } from "node:http";

export const CASES = [
  {
    name: "popup appears after a delay",
    button: "Pop up a dialog",
    delayMs: 300,
    idle: { untilSelector: "[role=dialog]" },
    // What the report has to contain for the caller to stop asking questions.
    expect: (r) => (r.woke === "selector" && (r.dialogs || 0) >= 1
      && JSON.stringify(r.appeared || []).includes("Confirm booking")),
  },
  {
    name: "text streams in",
    button: "Stream text",
    delayMs: 990,
    idle: { untilText: "end-of-stream" },
    expect: (r) => r.woke === "text" && (r.changes || 0) >= 2 && (r.textChars || 0) > 0,
  },
  {
    name: "list grows ten times",
    button: "Append a list",
    delayMs: 600,
    idle: { untilSelector: "#items li:nth-child(10)" },
    expect: (r) => r.woke === "selector" && (r.changes || 0) >= 3
      && JSON.stringify(r.appeared || []).includes("item"),
  },
  {
    name: "form mounts",
    button: "Reveal a form",
    delayMs: 500,
    idle: { untilSelector: "input[name=email]" },
    expect: (r) => r.woke === "selector"
      && JSON.stringify(r.appeared || []).includes("\"email\""),
  },
  {
    name: "page navigates",
    button: "Navigate away",
    delayMs: 300,
    idle: { untilSelector: "#second" },
    expect: (r) => r.woke === "selector" && r.navigated === true,
  },
  {
    // Attribute churn is not activity. A wait with no condition must stay quiet
    // and report that nothing changed, rather than waking twenty-five times.
    name: "attribute churn is not a change",
    button: "Toggle a class only",
    delayMs: 0,
    idle: { maxIdleMs: 2000 },
    expect: (r) => r.woke === "idleLimit" && r.changes === 0,
  },
];

const PAGE = `<!doctype html><meta charset="utf-8"><title>idle lab</title>
<style>
  body { font: 15px/1.5 system-ui, sans-serif; margin: 24px; max-width: 46rem }
  button { display: block; margin: 6px 0; padding: 6px 10px; font: inherit }
  #stage { margin-top: 18px }
  [role=dialog] { border: 2px solid #333; padding: 12px; margin: 12px 0; max-width: 28rem }
  .toggling { outline: 2px solid #999 }
</style>
<h1>idle lab</h1>
<p>Each button produces one shape of change. The benchmark times the wake and
checks whether the report was enough on its own.</p>
<div id="controls">
  <button id="b-popup">Pop up a dialog</button>
  <button id="b-stream">Stream text</button>
  <button id="b-list">Append a list</button>
  <button id="b-form">Reveal a form</button>
  <button id="b-nav">Navigate away</button>
  <button id="b-churn">Toggle a class only</button>
</div>
<div id="churner">this element only ever changes its class</div>
<div id="stage"></div>
<script>
  const stage = document.getElementById('stage');

  document.getElementById('b-popup').onclick = () => {
    setTimeout(() => {
      const d = document.createElement('div');
      d.setAttribute('role', 'dialog');
      d.setAttribute('aria-label', 'Confirm booking');
      d.textContent = 'Are you sure?';
      stage.appendChild(d);
    }, 300);
  };

  document.getElementById('b-stream').onclick = () => {
    const pre = document.createElement('pre');
    pre.id = 'stream';
    stage.appendChild(pre);
    const words = ['thinking', 'about', 'the', 'problem', 'and', 'then', 'the', 'answer', 'arrives', 'at', 'end-of-stream'];
    let i = 0;
    setTimeout(function tick() {
      if (i >= words.length) return;
      pre.textContent += (i ? ' ' : '') + words[i++];
      setTimeout(tick, 90);
    }, 90);
  };

  document.getElementById('b-list').onclick = () => {
    const ul = document.createElement('ul');
    ul.id = 'items';
    stage.appendChild(ul);
    let i = 0;
    setTimeout(function tick() {
      if (i >= 10) return;
      const li = document.createElement('li');
      li.textContent = 'item ' + (++i);
      ul.appendChild(li);
      setTimeout(tick, 60);
    }, 60);
  };

  document.getElementById('b-form').onclick = () => {
    setTimeout(() => {
      const f = document.createElement('form');
      f.id = 'signup';
      f.innerHTML = '<label for="e">Email</label><input id="e" name="email" placeholder="you@example.com"><button>Join</button>';
      stage.appendChild(f);
    }, 500);
  };

  document.getElementById('b-nav').onclick = () => {
    setTimeout(() => { location.href = '/second'; }, 300);
  };

  document.getElementById('b-churn').onclick = () => {
    const el = document.getElementById('churner');
    let n = 0;
    const t = setInterval(() => {
      el.classList.toggle('toggling');
      if (++n >= 25) clearInterval(t);
    }, 20);
  };
</script>`;

export async function lab() {
  const srv = createServer((req, res) => {
    const path = (req.url || "/").split("?")[0];
    if (path === "/second") {
      res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
      return res.end("<!doctype html><h1 id=second>second page</h1><p>arrived</p>");
    }
    res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
    res.end(PAGE);
  });
  await new Promise((r) => srv.listen(0, "127.0.0.1", r));
  const base = `http://127.0.0.1:${srv.address().port}`;
  return { base, close: () => srv.close() };
}
