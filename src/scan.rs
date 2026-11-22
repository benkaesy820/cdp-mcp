// The scanner: one injection that returns overlays and interactive elements
// together, so a single call tells the agent both "what can I touch" and
// "what is in the way".
//
// Index `i` is the ABSOLUTE position in `elements` and is stable for the page
// state: `input {click:{i}}` re-runs this same scan and indexes into it, so a
// filtered view never shifts what a click lands on.

use serde_json::{json, Value};

use crate::cdp;
use crate::input::{blocked_click, guard_irreversible, irreversible};
use crate::shot::{capture_guarded, parse_region};
use crate::{b64encode, clip_str, nonempty_scope, scan_cache, scope_cache, with_conn, Cfg};

pub const SCAN_JS: &str = r#"(() => {
  const seen = new Set();
  const labelOf = (el) => String(
    el.getAttribute('aria-label') || el.getAttribute('placeholder') || el.getAttribute('title') ||
    el.innerText || el.value || el.getAttribute('name') || ''
  ).trim().replace(/\s+/g, ' ').slice(0, 90);
  const boxOf = (r) => ({ x: Math.round(r.x + r.width / 2), y: Math.round(r.y + r.height / 2),
                          w: Math.round(r.width), h: Math.round(r.height) });
  const box = (el) => boxOf(el.getBoundingClientRect());
  const shown = (el, r) => {
    if (r.width < 2 || r.height < 2) return false;
    // checkVisibility is far cheaper than resolving computed style per element
    if (el.checkVisibility && !el.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true })) return false;
    if (r.bottom < -2000 || r.top > innerHeight + 4000) return false;
    return true;
  };
  const visible = (el) => shown(el, el.getBoundingClientRect());

  // ---- interactive elements (absolute index = click target) ----
  // SCOPE limits the scan to a subtree (e.g. "main"), which is the difference
  // between a few dozen candidates and a few thousand on a big page. Overlays
  // below still scan the whole document, because a modal is rarely inside it.
  const SCOPE = __SCOPE__;
  // PICK = {i, scroll?, select?}: resolve element i of this same scan and act on the
  // element itself. Indexing by element, not by point, is what makes a click
  // on something below the fold land: elementFromPoint is null off-screen.
  const PICK = __PICK__;
  const scopeEl = SCOPE ? document.querySelector(SCOPE) : null;
  // A miss is not "the whole page": the element list must not silently become the
  // document. PICK keeps its own shape so a stale index fails with the usual "no
  // element" message instead of resolving against a list it never came from.
  // Overlays and media are page-wide by design, so they are still scanned.
  const scopeMissed = !!(SCOPE && !scopeEl);
  if (scopeMissed && PICK) return { missing: true, i: PICK.i, total: 0, scopeMissing: true };
  const root = scopeEl || document;
  const sel = 'a[href], button, input, select, textarea, summary, [role="button"], [role="link"], [role="tab"], [role="menuitem"], [role="checkbox"], [role="radio"], [role="switch"], [role="combobox"], [contenteditable]:not([contenteditable="false"]), [onclick], [tabindex]:not([tabindex="-1"])';
  const elements = [];
  const refs = [];
  // The same-origin <iframe> chain each element sits in ([] = this document).
  const chains = [];

  // Where a frame chain's content box is, in this viewport's coordinates, and
  // the part of it that is actually visible (each frame clips what is inside).
  const frameBox = (chain) => {
    let x = 0, y = 0, L = 0, T = 0, R = innerWidth, B = innerHeight;
    for (const f of chain) {
      const r = f.getBoundingClientRect();
      x += r.left + f.clientLeft; y += r.top + f.clientTop;
      L = Math.max(L, x); T = Math.max(T, y);
      R = Math.min(R, x + f.clientWidth); B = Math.min(B, y + f.clientHeight);
    }
    return { x, y, L, T, R, B };
  };
  const placed = (el, fb) => {
    const r = el.getBoundingClientRect();
    return fb.x || fb.y ? new DOMRect(r.x + fb.x, r.y + fb.y, r.width, r.height) : r;
  };
  const full = () => elements.length >= 600 || (PICK && elements.length > PICK.i);

  // Light DOM first, then open shadow roots (web components keep their
  // buttons there), then same-origin frames. querySelectorAll stops at both
  // boundaries, which is why these were invisible. The order is fixed, so an
  // index means the same element on every scan of the same page state.
  const collect = (node, chain, frameLabel) => {
    const fb = frameBox(chain);
    for (const el of node.querySelectorAll(sel)) {
      if (full()) return;
      const r = placed(el, fb);
      if (!shown(el, r)) continue;
      const b = boxOf(r);
      const key = b.x + ',' + b.y + ',' + b.w;
      if (seen.has(key)) continue;
      seen.add(key);
      const rec = { i: elements.length, tag: el.tagName.toLowerCase(), label: labelOf(el) };
      // Effective role, derived when the element does not carry one: a plain
      // <a href> *is* a link, and filtering by role would miss every one of them.
      const explicit = el.getAttribute('role');
      const t = el.tagName;
      const derived = t === 'A' ? 'link'
        : t === 'BUTTON' ? 'button'
        : t === 'SELECT' ? 'combobox'
        : t === 'TEXTAREA' ? 'textbox'
        : t === 'INPUT' ? (el.type === 'checkbox' ? 'checkbox' : el.type === 'radio' ? 'radio'
                          : el.type === 'submit' ? 'button' : 'textbox')
        : el.isContentEditable ? 'textbox'
        : '';
      const eff = explicit || derived;
      if (eff) rec.role = eff;
      // :disabled is the browser's own answer: the disabled attribute and an
      // ancestor fieldset[disabled], minus the first-legend exception, and it
      // leaves a link or a [role=button] inside the fieldset clickable.
      let dis = !!el.disabled || el.getAttribute('aria-disabled') === 'true';
      try { if (!dis && el.matches && el.matches(':disabled')) dis = true; } catch (e) {}
      if (dis) rec.disabled = true;
      // Current form state, so a scoped scan answers "what is typed here"
      // without a second eval. Passwords never leave the page in plaintext.
      const tagName = el.tagName;
      if (tagName === 'INPUT') {
          const ty = (el.type || 'text').toLowerCase();
          if (ty === 'checkbox' || ty === 'radio') rec.checked = !!el.checked;
          else if (ty === 'password') { if (el.value) rec.value = '***'; }
          else if (el.value) rec.value = String(el.value).slice(0, 60);
      } else if (tagName === 'TEXTAREA') {
          if (el.value) rec.value = String(el.value).slice(0, 60);
      } else if (tagName === 'SELECT') {
          if (el.multiple) {
              // `selectedIndex` reports one entry; a multiple select can have
              // several, and hiding the rest is how a selection reads as lost.
              rec.selected = [...el.selectedOptions].map((o) => String(o.text || '').trim().slice(0, 60));
          } else {
              const opt = el.options[el.selectedIndex];
              if (opt) rec.selected = String(opt.text || '').trim().slice(0, 60);
          }
      } else if (el.isContentEditable || el.getAttribute('role') === 'textbox') {
          const v = (el.innerText || '').replace(/\s+/g, ' ').trim().slice(0, 60);
          if (v) rec.value = v;
      }
      const roleAttr = el.getAttribute('role');
      // A native control already answered: role=switch on a checked checkbox
      // usually has no aria-checked, and must not read as false.
      if (rec.checked === undefined && (roleAttr === 'checkbox' || roleAttr === 'switch' || roleAttr === 'radio')) {
          rec.checked = el.getAttribute('aria-checked') === 'true';
      }
      if (frameLabel) rec.frame = frameLabel;
      Object.assign(rec, b);
      // off-screen (or outside its frame's visible area): needs scrolling
      // before a coordinate click will land
      if (r.top < fb.T - 4 || r.bottom > fb.B + 4) rec.off = true;
      elements.push(rec);
      refs.push(el);
      chains.push(chain);
    }
    for (const host of node.querySelectorAll('*')) {
      if (full()) return;
      if (host.shadowRoot) collect(host.shadowRoot, chain, frameLabel);
    }
    for (const f of node.querySelectorAll('iframe, frame')) {
      if (full()) return;
      let doc = null;
      try { doc = f.contentDocument; } catch (e) { /* cross-origin */ }
      if (!doc || !doc.documentElement || !shown(f, placed(f, fb))) continue;
      const label = String(f.getAttribute('title') || f.getAttribute('name') || f.id || 'frame').slice(0, 40);
      collect(doc, chain.concat([f]), label);
    }
  };
  collect(root, [], '');

  // A miss was scanned as the document so overlays and media are real; the
  // element list is what the caller did not ask for, so it is what gets emptied.
  if (scopeMissed) { elements.length = 0; refs.length = 0; chains.length = 0; }

  if (PICK) {
    const el = refs[PICK.i];
    if (!el) return { missing: true, total: elements.length };
    const chain = chains[PICK.i];
    let fb = frameBox(chain), r = placed(el, fb);
    let scrolled = false;
    if (PICK.scroll !== false && (r.top < fb.T || r.bottom > fb.B || r.left < fb.L || r.right > fb.R)) {
      // 'instant' overrides a page's `scroll-behavior: smooth`, which would
      // otherwise leave the element mid-flight when we measure it. It scrolls
      // every scrollable ancestor, frames included.
      el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
      fb = frameBox(chain); r = placed(el, fb);
      scrolled = true;
    }
    const x = r.x + r.width / 2, y = r.y + r.height / 2;
    const out = { i: PICK.i, tag: elements[PICK.i].tag, label: elements[PICK.i].label,
                  x: Math.round(x), y: Math.round(y), scrolled };
    out.connected = el.isConnected;
    out.zero = r.width === 0 || r.height === 0;
    // Document-space bounds after any scroll: the clip for an element shot.
    // `r` is in the top document's viewport space, frames included.
    out.rect = { x: Math.round(r.x + scrollX), y: Math.round(r.y + scrollY),
                 w: Math.round(r.width), h: Math.round(r.height) };
    out.disabled = !!el.disabled || el.getAttribute('aria-disabled') === 'true';
    try { if (!out.disabled && el.matches && el.matches(':disabled')) out.disabled = true; } catch (e) {}
    // Would a real click at the centre reach this element, or something on
    // top? Hit-test through shadow roots and same-origin frames, the way the
    // browser will route the click.
    const deepAt = (px, py) => {
      let t = document.elementFromPoint(px, py), ox = 0, oy = 0;
      for (let k = 0; t && k < 32; k++) {
        if (t.shadowRoot) {
          const n = t.shadowRoot.elementFromPoint(px - ox, py - oy);
          if (n && n !== t) { t = n; continue; }
        }
        if (t.tagName === 'IFRAME' || t.tagName === 'FRAME') {
          let d = null;
          try { d = t.contentDocument; } catch (e) {}
          if (d) {
            const fr = t.getBoundingClientRect();
            ox += fr.left + t.clientLeft; oy += fr.top + t.clientTop;
            const n = d.elementFromPoint(px - ox, py - oy);
            if (n) { t = n; continue; }
          }
        }
        break;
      }
      return t;
    };
    // `contains` stops at shadow and frame boundaries; walk across them.
    const up = (n) => n.parentNode || n.host || (n.nodeType === 9 && n.defaultView ? n.defaultView.frameElement : null);
    const holds = (a, b) => { for (let n = b; n; n = up(n)) if (n === a) return true; return false; };
    const top = deepAt(x, y);
    out.hit = !!top && (holds(el, top) || holds(top, el));
    if (!out.hit && top) out.coveredBy = labelOf(top).slice(0, 60) || top.tagName.toLowerCase();
    if (!out.hit && top) {
        // The blocker, in the same compact shape as the wait diagnostics.
        const desc = (n) => {
            if (!n || n.nodeType !== 1) return null;
            const o = { tag: n.tagName.toLowerCase() };
            const role = n.getAttribute('role');
            if (role) o.role = role;
            const label = n.getAttribute('aria-label') || n.getAttribute('placeholder') || n.getAttribute('name');
            if (label) o.label = label.slice(0, 60);
            const t = (n.innerText || n.textContent || '').replace(/\s+/g, ' ').trim().slice(0, 120);
            if (t) o.text = t;
            return o;
        };
        const b = desc(top);
        if (b && !b.label && !b.text) {
            const d = top.closest && top.closest('[role="dialog"],dialog,[aria-modal="true"]');
            if (d) Object.assign(b, desc(d));
        }
        out.blocker = b;
    }
    const a = el.closest('a[href]') || el.querySelector('a[href]');
    if (a) out.href = a.href;
    if (PICK.select !== undefined) {
      const s = el.tagName === 'SELECT' ? el : el.closest('select');
      if (!s) { out.selectError = 'not a select'; }
      else if (Array.isArray(PICK.select)) {
        const wanted = PICK.select.map(String);
        const missing = wanted.filter((v) => ![...s.options].some((o) => o.value === v));
        if (missing.length) out.selectError = 'no option ' + missing.join(', ');
        else if (!s.multiple && wanted.length > 1)
          out.selectError = 'this <select> takes one value; it is not multiple';
        else {
          const set = new Set(wanted);
          for (const o of s.options) o.selected = set.has(o.value);
          s.dispatchEvent(new Event('input', { bubbles: true }));
          s.dispatchEvent(new Event('change', { bubbles: true }));
          out.selected = [...s.options].filter((o) => o.selected).map((o) => o.value);
        }
      }
      else {
        s.value = PICK.select;
        s.dispatchEvent(new Event('input', { bubbles: true }));
        s.dispatchEvent(new Event('change', { bubbles: true }));
        out.selected = s.value;
      }
    }
    return out;
  }

  // ---- overlays: dialogs, alert dialogs, and big fixed/absolute layers ----
  const overlays = [];
  const seenOverlay = new Set();
  const candidates = [...document.querySelectorAll('[role="dialog"], [role="alertdialog"], [aria-modal="true"], dialog[open]')];
  // Walking every div on a real article means thousands of getComputedStyle
  // calls. Overlays live near the top of the tree (portals mount on <body>),
  // so a depth-limited walk finds the same elements for a fraction of the cost.
  const layerCandidates = [];
  const walk = (el, depth) => {
    if (depth > 4) return;
    for (const c of el.children) {
      const tg = c.tagName;
      if (tg === 'DIV' || tg === 'SECTION' || tg === 'ASIDE' || tg === 'FORM') layerCandidates.push(c);
      walk(c, depth + 1);
    }
  };
  if (document.body) walk(document.body, 0);
  for (const el of layerCandidates) {
    const cs = getComputedStyle(el);
    if (cs.position !== 'fixed' && cs.position !== 'absolute') continue;
    const r = el.getBoundingClientRect();
    // A cookie wall is usually wide and short, a modal is boxy: accept either
    // a decent share of the viewport area, or something that spans its width.
    const areaShare = (r.width * r.height) / (innerWidth * innerHeight);
    const spansWidth = r.width >= innerWidth * 0.7 && r.height >= 40;
    if (areaShare < 0.15 && !spansWidth) continue;
    if (cs.position === 'absolute' && Number(cs.zIndex || 0) < 10) continue;
    candidates.push(el);
  }
  const CLOSE_RE = /(close|dismiss|cancel|accept|agree|allow|ok|got it|no thanks|not now|later|continue|skip|reject|deny|refuse|necessary|essential|manage|preferences|save choices|✕|×|x)/i;
  for (const el of candidates) {
    if (!visible(el)) continue;
    const b = box(el);
    const key = Math.round(b.y) + ':' + labelOf(el).slice(0, 24);
    if (seenOverlay.has(key)) continue;
    seenOverlay.add(key);
    const role = el.getAttribute('role') || '';
    const kind = role === 'alertdialog' ? 'alert'
               : role === 'dialog' || el.tagName.toLowerCase() === 'dialog' ? 'dialog'
               : 'layer';
    const closers = [];
    for (const b2 of el.querySelectorAll('button, [role="button"], a[href], input[type="submit"]')) {
      if (!visible(b2)) continue;
      const lbl = labelOf(b2);
      if (!lbl) continue;
      if (!CLOSE_RE.test(lbl) && !CLOSE_RE.test(b2.getAttribute('aria-label') || '')) continue;
      const idx = refs.indexOf(b2);
      const rec = { label: lbl };
      if (idx >= 0) rec.i = idx;               // clickable by absolute index
      Object.assign(rec, box(b2));
      closers.push(rec);
      if (closers.length >= 4) break;
    }
    const rec = { kind, label: labelOf(el).slice(0, 120) };
    Object.assign(rec, b);
    if (closers.length) rec.closers = closers;
    overlays.push(rec);
    if (overlays.length >= 6) break;
  }

  // media: report, never touch. A playing video must survive a scan.
  const media = [];
  for (const m of document.querySelectorAll('video, audio')) {
    media.push({
      tag: m.tagName.toLowerCase(),
      playing: !m.paused && !m.ended,
      muted: !!m.muted,
      t: Math.round(m.currentTime || 0),
      dur: isFinite(m.duration) ? Math.round(m.duration) : null,
    });
    if (media.length >= 4) break;
  }

  return {
    url: location.href,
    scopeMissing: scopeMissed,
    // How much page is there, and where am I in it? Without this a virtualised
    // feed looks like the whole page, and an empty scan looks like an empty site.
    scroll: {
      y: Math.round(scrollY),
      h: Math.round(document.documentElement.scrollHeight),
      vh: innerHeight,
    },
    media,
    title: document.title.slice(0, 120),
    total: elements.length,
    // How many nodes the document actually holds, and whether the parser is
    // done. A React app mid-hydration and a genuinely empty page return the
    // same empty element list; the node count is what tells them apart.
    nodes: document.querySelectorAll('*').length,
    ready: document.readyState,
    overlays,
    elements,
  };
})()"#;

/// A cheap post-action check: only dialogs that appeared *because* we acted.
/// Full SCAN_JS costs ~40 ms on a real article; this is a fraction of it and
/// catches the case that actually swallows the next click.
pub const OVERLAYS_QUICK_JS: &str = r#"(() => {
  const out = [];
  const vis = (el) => {
    const r = el.getBoundingClientRect();
    if (r.width < 2 || r.height < 2) return false;
    const cs = getComputedStyle(el);
    return !(cs.visibility === 'hidden' || cs.display === 'none' || cs.opacity === '0');
  };
  for (const el of document.querySelectorAll('[role="dialog"], [role="alertdialog"], [aria-modal="true"], dialog[open]')) {
    if (!vis(el)) continue;
    const r = el.getBoundingClientRect();
    const label = String(el.getAttribute('aria-label') || el.innerText || '').trim().replace(/\s+/g, ' ').slice(0, 100);
    out.push({ kind: el.getAttribute('role') === 'alertdialog' ? 'alert' : 'dialog', label,
               x: Math.round(r.x + r.width / 2), y: Math.round(r.y + r.height / 2) });
    if (out.length >= 4) break;
  }
  return out;
})()"#;

/// What a cross-origin frame offers, in its OWN coordinate space.
///
/// Deliberately separate from `SCAN_JS`: a frame's document has no knowledge of
/// the page around it, and the caller supplies the offset. Kept small — the
/// interactive set only — because it runs once per frame on every scan.
pub const FRAME_ELEMENTS_JS: &str = r#"(() => {
  const sel = 'a[href], button, input, select, textarea, summary, [role="button"], [role="link"], [role="tab"], [role="menuitem"], [role="checkbox"], [role="radio"], [role="switch"], [role="combobox"], [contenteditable]:not([contenteditable="false"]), [onclick], [tabindex]:not([tabindex="-1"])';
  const out = [];
  const seen = new Set();
  // Same order and cap as SCAN_JS. The label decides the irreversible-click
  // guard, so a frame must not name a button differently from the page.
  const labelOf = (el) => String(
    el.getAttribute('aria-label') || el.getAttribute('placeholder') || el.getAttribute('title') ||
    el.innerText || el.value || el.getAttribute('name') || ''
  ).trim().replace(/\s+/g, ' ').slice(0, 90);
  const walk = (root) => {
    for (const el of root.querySelectorAll(sel)) {
      if (out.length >= 300) return;
      const r = el.getBoundingClientRect();
      if (r.width < 2 || r.height < 2) continue;
      // Same rule as SCAN_JS: checkVisibility sees hidden ancestors, which
      // computed style alone does not (an element under opacity:0 is invisible).
      if (el.checkVisibility && !el.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true })) continue;
      if (!el.checkVisibility) {
        const st = getComputedStyle(el);
        if (st.visibility === 'hidden' || st.display === 'none' || st.opacity === '0') continue;
      }
      const key = Math.round(r.x) + ',' + Math.round(r.y) + ',' + Math.round(r.width);
      if (seen.has(key)) continue;
      seen.add(key);
      const explicit = el.getAttribute('role');
      const tag = el.tagName.toLowerCase();
      const derived = tag === 'a' ? 'link' : tag === 'button' ? 'button'
        : tag === 'select' ? 'combobox' : tag === 'textarea' ? 'textbox'
        : tag === 'input' ? (el.type === 'checkbox' ? 'checkbox' : el.type === 'radio' ? 'radio'
                            : el.type === 'submit' ? 'button' : 'textbox')
        : el.isContentEditable ? 'textbox' : '';
      const rec = { tag, label: labelOf(el), x: Math.round(r.x), y: Math.round(r.y),
                    w: Math.round(r.width), h: Math.round(r.height) };
      if (explicit || derived) rec.role = explicit || derived;
      // :disabled is the browser's own answer: the disabled attribute and an
      // ancestor fieldset[disabled], minus the first-legend exception, and it
      // leaves a link or a [role=button] inside the fieldset clickable.
      let dis = !!el.disabled || el.getAttribute('aria-disabled') === 'true';
      try { if (!dis && el.matches && el.matches(':disabled')) dis = true; } catch (e) {}
      if (dis) rec.disabled = true;
      // Current form state, so a scoped scan answers "what is typed here"
      // without a second eval. Passwords never leave the page in plaintext.
      const tagName = el.tagName;
      if (tagName === 'INPUT') {
          const ty = (el.type || 'text').toLowerCase();
          if (ty === 'checkbox' || ty === 'radio') rec.checked = !!el.checked;
          else if (ty === 'password') { if (el.value) rec.value = '***'; }
          else if (el.value) rec.value = String(el.value).slice(0, 60);
      } else if (tagName === 'TEXTAREA') {
          if (el.value) rec.value = String(el.value).slice(0, 60);
      } else if (tagName === 'SELECT') {
          if (el.multiple) {
              // `selectedIndex` reports one entry; a multiple select can have
              // several, and hiding the rest is how a selection reads as lost.
              rec.selected = [...el.selectedOptions].map((o) => String(o.text || '').trim().slice(0, 60));
          } else {
              const opt = el.options[el.selectedIndex];
              if (opt) rec.selected = String(opt.text || '').trim().slice(0, 60);
          }
      } else if (el.isContentEditable || el.getAttribute('role') === 'textbox') {
          const v = (el.innerText || '').replace(/\s+/g, ' ').trim().slice(0, 60);
          if (v) rec.value = v;
      }
      const roleAttr = el.getAttribute('role');
      // A native control already answered: role=switch on a checked checkbox
      // usually has no aria-checked, and must not read as false.
      if (rec.checked === undefined && (roleAttr === 'checkbox' || roleAttr === 'switch' || roleAttr === 'radio')) {
          rec.checked = el.getAttribute('aria-checked') === 'true';
      }
      out.push(rec);
    }
    for (const host of root.querySelectorAll('*')) {
      if (host.shadowRoot) walk(host.shadowRoot);
    }
  };
  walk(document);
  return JSON.stringify({ elements: out, title: document.title.slice(0, 80), url: location.href.slice(0, 200) });
})()"#;

/// SCAN_JS with its scope placeholder substituted (None = whole document).
pub(crate) fn scan_js(scope: Option<&str>) -> String {
    let lit = match scope {
        Some(s) => serde_json::to_string(s).unwrap_or_else(|_| "null".into()),
        None => "null".into(),
    };
    SCAN_JS.replace("__SCOPE__", &lit).replace("__PICK__", "null")
}

/// Resolve element `i` (in `scope`) inside the page, scroll it into view if
/// asked, and report where a click on it lands. One evaluation: this used to
/// be four full scans per click.
pub(crate) fn pick(conn: &mut cdp::Conn, scope: Option<&str>, i: u64, scroll: bool, select: Option<&Value>) -> Result<Value, String> {
    let lit = match scope {
        Some(s) => serde_json::to_string(s).unwrap_or_else(|_| "null".into()),
        None => "null".into(),
    };
    let mut p = json!({ "i": i, "scroll": scroll });
    if let Some(v) = select {
        p["select"] = json!(v);
    }
    let js = SCAN_JS.replace("__SCOPE__", &lit).replace("__PICK__", &p.to_string());
    let v = conn.eval(&js)?;
    if v.get("missing").and_then(Value::as_bool) == Some(true) {
        let total = v.get("total").and_then(Value::as_u64).unwrap_or(0);
        return Err(format!(
            "no element with index {i} ({total} found{}); call elements again",
            scope.map(|s| format!(" within {s}")).unwrap_or_default()
        ));
    }
    Ok(v)
}

/// Scan the page plus every cross-origin iframe inside it.
///
/// The page's own script cannot read across a site boundary — that is site
/// isolation, not a bug in the scanner — so an embedded widget's buttons are
/// absent from a normal scan. Chromium exposes each such frame as its own
/// target with its own session, which is what `Conn::frames` collects.
///
/// Every frame's elements are appended after the page's own, so an index still
/// means one element and every page element keeps the index it already had. A
/// scoped scan is the exception: a CSS selector cannot reach into a frame.
/// Coordinates are translated into the embedding page's viewport, which is the
/// space a click is dispatched in.
pub(crate) fn scan_with_frames(conn: &mut cdp::Conn, scope: Option<&str>) -> Result<Value, String> {
    let mut scan_v = conn.eval(&scan_js(scope))?;
    // A CSS selector can only match this document. Frame elements appended to a
    // scoped scan would be indexes the caller never asked for, in a list it
    // believes is scoped.
    if scope.is_some() {
        return Ok(scan_v);
    }
    let frames = match conn.frames() {
        Ok(f) => f,
        Err(_) => return Ok(scan_v), // older browser: page-only scan is still useful
    };
    if frames.is_empty() {
        return Ok(scan_v);
    }
    let mut elements = scan_v.get("elements").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut added = 0u64;
    for (target, session, url) in frames {
        // Read the frame's own document, then find where its box sits in the
        // parent so local coordinates can be made absolute.
        // The frame returns a JSON string: one value in, one value out, and no
        // surprise about how deep the protocol will serialise.
        let body = match conn.eval_in(&session, FRAME_ELEMENTS_JS) {
            Ok(Value::String(body)) => serde_json::from_str::<Value>(&body).unwrap_or(Value::Null),
            Ok(v) => v,
            Err(_) => continue, // sandboxed, detached, or no document yet
        };
        let kids = body.get("elements").and_then(Value::as_array).cloned().unwrap_or_default();
        if kids.is_empty() {
            continue;
        }
        let origin = match conn.eval(&format!(
            "(() => {{ const f = [...document.querySelectorAll('iframe, frame')].find(f => (f.src || '') === {src}); \
               if (!f) return null; const r = f.getBoundingClientRect(); \
               if (r.width < 2 || r.height < 2) return null; \
               if (f.checkVisibility && !f.checkVisibility({{ checkOpacity: true, checkVisibilityCSS: true }})) return null; \
               return JSON.stringify({{ x: r.left + f.clientLeft, y: r.top + f.clientTop, w: f.clientWidth, h: f.clientHeight }}); }})()",
            src = serde_json::to_string(&url).unwrap_or_else(|_| "null".into())
        )) {
            Ok(Value::String(s)) => serde_json::from_str::<Value>(&s).ok(),
            _ => None,
        };
        let origin = match origin {
            Some(o) if !o.is_null() => o,
            _ => continue, // cannot place it on screen, so a click there is a guess
        };
        let (ox, oy) = (
            origin.get("x").and_then(Value::as_f64).unwrap_or(0.0),
            origin.get("y").and_then(Value::as_f64).unwrap_or(0.0),
        );
        let label = url
            .split("://").nth(1).unwrap_or(&url)
            .split('/').next().unwrap_or(&url)
            .chars().take(40).collect::<String>();
        for mut k in kids {
            // A frame can be scrolled: only what the parent actually shows is
            // clickable, so anything outside the frame's box is marked off-screen.
            let (x, y) = (
                k.get("x").and_then(Value::as_f64).unwrap_or(0.0),
                k.get("y").and_then(Value::as_f64).unwrap_or(0.0),
            );
            let (w, h) = (
                k.get("w").and_then(Value::as_f64).unwrap_or(0.0),
                k.get("h").and_then(Value::as_f64).unwrap_or(0.0),
            );
            // The click lands at the child's centre, so the centre is the point
            // that has to be inside the frame: an edge-flush element whose
            // rounded edges overshoot by a pixel is still clickable, and one
            // whose centre is outside is not.
            let (lx, ly) = ((x + w / 2.0).round(), (y + h / 2.0).round());
            if let Some(o) = k.as_object_mut() {
                o.insert("i".into(), json!(elements.len()));
                o.insert("x".into(), json!((x + ox + w / 2.0).round() as i64));
                o.insert("y".into(), json!((y + oy + h / 2.0).round() as i64));
                o.insert("frame".into(), json!(label));
                o.insert("session".into(), json!(session));
                // The target id survives a reconnect; the session id does not.
                o.insert("target".into(), json!(target));
                // Local centre, which is what a click inside the frame needs.
                o.insert("lx".into(), json!(lx as i64));
                o.insert("ly".into(), json!(ly as i64));
            }
            let fw = origin.get("w").and_then(Value::as_f64).unwrap_or(0.0);
            let fh = origin.get("h").and_then(Value::as_f64).unwrap_or(0.0);
            let inside = lx >= 0.0 && ly >= 0.0 && lx <= fw && ly <= fh;
            if !inside {
                if let Some(o) = k.as_object_mut() {
                    o.insert("off".into(), json!(true));
                }
            }
            elements.push(k);
            added += 1;
            if elements.len() >= 600 {
                break;
            }
        }
    }
    if added > 0 {
        if let Some(o) = scan_v.as_object_mut() {
            o.insert("elements".into(), Value::Array(elements));
            o.insert("framesScanned".into(), json!(added));
        }
    }
    Ok(scan_v)
}

/// If element `i` of tab `tab` lives inside a cross-origin frame, where to
/// click it.
///
/// Returns `(local x, local y, session)`. Cheap in the common case: one cached
/// scan lookup, no protocol traffic; the browser is only asked when the element
/// carries a `session`, which `scan_with_frames` sets only for frames.
///
/// The target id proves the frame is the same one, not that the document inside
/// it still shows the same element: a same-site navigation keeps both the target
/// and the old coordinates, so the click can land on whatever moved there.
pub(crate) fn frame_target(
    conn: &mut cdp::Conn,
    tab: &str,
    scope: Option<&str>,
    i: u64,
    force: bool,
) -> Result<Option<(f64, f64, String)>, String> {
    // A scoped click indexes the scoped list, which holds no frame elements: a
    // CSS selector cannot reach into a child document. Resolving it in the tab's
    // unscoped cache would click a different element than the one named.
    if scope.is_some() {
        return Ok(None);
    }
    // The cache holds one entry per tab. Reading whichever entry came first sent
    // this click into a session that belongs to a different page's connection.
    let cached = {
        let cache = scan_cache().lock().unwrap();
        cache
            .get(tab)
            .and_then(|v| v.get("elements").and_then(Value::as_array).cloned())
    };
    let el = cached
        .and_then(|els| els.get(i as usize).cloned())
        .filter(|e| e.get("session").and_then(Value::as_str).is_some());
    let el = match el {
        Some(e) => e,
        None => return Ok(None),
    };
    let mut session = el.get("session").and_then(Value::as_str).unwrap_or_default().to_string();
    if session.is_empty() {
        return Ok(None);
    }
    // Resolve the session on this connection: a cached id goes stale when the
    // page reconnects, and ids attached elsewhere are not usable here. The
    // target id survives both, which is why the scan records it.
    let want = el.get("target").and_then(Value::as_str).unwrap_or_default();
    if !want.is_empty() {
        match conn.frames() {
            Ok(frames) => match frames.iter().find(|f| f.0 == want) {
                Some((_, s, _)) => session = s.clone(),
                // The frame is gone, or it is same-origin now. Falling through to
                // the main document would act on a different element with this index.
                None => {
                    return Err(format!(
                        "element {i} was inside a frame that is no longer there; call elements again"
                    ));
                }
            },
            // Without the frame list the cached session cannot be trusted: it may
            // belong to a connection that is gone.
            Err(e) => {
                return Err(format!(
                    "cannot resolve the frame for element {i} ({e}); call elements again"
                ));
            }
        }
    }
    // The same refusals the page path makes, from the state the frame scan
    // recorded: without them a frame was a way around both. They come after the
    // resolution, so a frame that is gone does not consume the guard.
    let tag = el.get("tag").unwrap_or(&Value::Null);
    let label = el.get("label").unwrap_or(&Value::Null);
    if !force && el.get("disabled").and_then(Value::as_bool) == Some(true) {
        return Err(blocked_click("disabled", i, tag, label, Value::Null, true));
    }
    // Below the frame's own fold: what scrolls is the frame's document, not the
    // page, so a dispatch at these coordinates lands outside its viewport. An
    // unfelt click that still burns the guard is the worst of both.
    if !force && el.get("off").and_then(Value::as_bool) == Some(true) {
        return Err(blocked_click("outside_frame", i, tag, label, Value::Null, true));
    }
    if let Some(text) = label.as_str() {
        if irreversible(text) {
            guard_irreversible(tab, text, i, force)?;
        }
    }
    let x = el.get("lx").and_then(Value::as_f64).unwrap_or(0.0);
    let y = el.get("ly").and_then(Value::as_f64).unwrap_or(0.0);
    Ok(Some((x, y, session)))
}

/// Send one mouse event to a frame's own session, at frame-local coordinates.
pub(crate) fn frame_click(
    conn: &mut cdp::Conn,
    session: &str,
    x: f64,
    y: f64,
    button: &str,
    count: u64,
    mods: u32,
) -> Result<(), String> {
    // The same three events the page path sends, and the same held-button mask:
    // a right click with buttons:1 is a left-button state to the page's scripts.
    let held = match button { "right" => 2, "middle" => 4, _ => 1 };
    conn.call_in(Some(session), "Input.dispatchMouseEvent", json!({
        "type": "mouseMoved", "x": x, "y": y, "modifiers": mods,
    }))?;
    conn.call_in(Some(session), "Input.dispatchMouseEvent", json!({
        "type": "mousePressed", "x": x, "y": y, "button": button,
        "clickCount": count, "buttons": held, "modifiers": mods,
    }))?;
    conn.call_in(Some(session), "Input.dispatchMouseEvent", json!({
        "type": "mouseReleased", "x": x, "y": y, "button": button,
        "clickCount": count, "buttons": 0, "modifiers": mods,
    }))?;
    Ok(())
}

/// Is this empty scan more likely a page that has not painted yet?
///
/// A React app mid-hydration and a genuinely empty document both return no
/// elements. What tells them apart is how much DOM exists: a page still being
/// built has nodes but no interactive ones yet, while a truly empty document has
/// almost nothing at all.
pub(crate) fn suspect_render(total: u64, nodes: u64) -> bool {
    total < 3 && nodes < 400
}

pub(crate) fn fingerprint(e: &Value) -> String {
    format!(
        "{}|{}|{}|{}",
        e.get("tag").and_then(Value::as_str).unwrap_or(""),
        e.get("label").and_then(Value::as_str).unwrap_or(""),
        e.get("x").and_then(Value::as_i64).unwrap_or(0),
        e.get("y").and_then(Value::as_i64).unwrap_or(0),
    )
}

pub(crate) fn elements(cfg: &Cfg, args: &Value) -> Result<Value, String> {
    let tab = args.get("tab").and_then(Value::as_str);
    let find = args.get("find").and_then(Value::as_str).map(|s| s.to_lowercase());
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(120) as usize;
    let full = args.get("full").and_then(Value::as_bool).unwrap_or(false);
    let mode = args.get("mode").and_then(Value::as_str).unwrap_or("full").to_string();
    let unique = args.get("unique").and_then(Value::as_bool).unwrap_or(false);
    let role_filter = args.get("role").and_then(Value::as_str).map(|s| s.to_lowercase());
    let shot_mode = args.get("shot").and_then(Value::as_str).unwrap_or("none").to_string();
    let shot_fmt = args.get("shotFormat").and_then(Value::as_str).unwrap_or("jpeg").to_string();
    let max_w = args.get("shotMaxWidth").and_then(Value::as_f64).unwrap_or(1024.0);
    let region = parse_region(args.get("region"));

    let within = match args.get("within") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => nonempty_scope(s),
        Some(_) => return Err("within must be a CSS selector; integer within is not implemented yet".into()),
    };
    with_conn(&cfg.endpoint, tab, |conn, tab_id| {
        let mut scan_v = scan_with_frames(conn, within.as_deref())?;
        // Kept with the cache entry, so a delta knows whether its
        // baseline came from the same scope as this scan.
        if let Some(o) = scan_v.as_object_mut() {
            o.insert("scope".into(), json!(&within));
        }
        scope_cache().lock().unwrap().insert(tab_id.to_string(), within.clone());

        let mut all = scan_v.get("elements").and_then(Value::as_array).cloned().unwrap_or_default();
        if mode == "viewport" {
            all.retain(|e| e.get("off").and_then(Value::as_bool) != Some(true));
        }

        let previous = {
            let mut cache = scan_cache().lock().unwrap();
            let prev = cache.get(tab_id).cloned().unwrap_or(Value::Null);
            cache.insert(tab_id.to_string(), scan_v.clone());
            prev
        };
        // A baseline taken under another scope is not a baseline: every
        // element would read as added and every old one as removed.
        let scope_changed = !previous.is_null()
            && previous.get("scope").and_then(Value::as_str) != within.as_deref();
        let previous = if scope_changed { Value::Null } else { previous };

        if mode == "delta" {
            let prev_els = previous.get("elements").and_then(Value::as_array).cloned().unwrap_or_default();
            let old_set: std::collections::HashSet<String> = prev_els.iter().map(fingerprint).collect();
            let new_set: std::collections::HashSet<String> = all.iter().map(fingerprint).collect();
            let added: Vec<Value> = all.iter().filter(|e| !old_set.contains(&fingerprint(e))).cloned().collect();
            let removed: Vec<Value> = prev_els.iter().filter(|e| !new_set.contains(&fingerprint(e))).cloned().collect();
            let prev_ov = previous.get("overlays").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0) as i64;
            let now_ov = scan_v.get("overlays").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0) as i64;
            return Ok(json!({
                "tab": tab_id,
                "mode": "delta",
                // Without these, a scope that never existed reads as
                // "nothing changed" in the mode callers use to verify.
                "scope": within.clone(),
                "scopeChanged": scope_changed,
                "scopeMissing": scan_v.get("scopeMissing").and_then(Value::as_bool).unwrap_or(false),
                "baseline": !previous.is_null(),
                "added": added,
                "addedCount": added.len(),
                "removed": removed,
                "removedCount": removed.len(),
                "unchanged": all.len().saturating_sub(added.len()),
                "overlays": scan_v.get("overlays").cloned().unwrap_or(json!([])),
                "overlayDelta": now_ov - prev_ov,
                "media": scan_v.get("media").cloned().unwrap_or(json!([])),
            }));
        }

        let total = all.len();
        let mut seen_labels: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut shown: Vec<Value> = Vec::new();
        for e in &all {
            let label = e.get("label").and_then(Value::as_str).unwrap_or("");
            if let Some(f) = &find {
                if !label.to_lowercase().contains(f) {
                    continue;
                }
            }
            if let Some(rf) = &role_filter {
                let role = e.get("role").and_then(Value::as_str).unwrap_or("").to_lowercase();
                let tag = e.get("tag").and_then(Value::as_str).unwrap_or("").to_lowercase();
                if role != *rf && tag != *rf {
                    continue;
                }
            }
            if unique && !label.is_empty() && !seen_labels.insert(label.to_lowercase()) {
                continue;
            }
            let mut keep = e.clone();
            if !full {
                if let Some(o) = keep.as_object_mut() {
                    o.remove("w");
                    o.remove("h");
                }
            }
            shown.push(keep);
            if shown.len() >= limit {
                break;
            }
        }

        let url = scan_v.get("url").and_then(Value::as_str).unwrap_or("").to_string();
        let url = if url.len() > 300 { format!("{}…", clip_str(&url, 300)) } else { url };
        let scope_missing = scan_v.get("scopeMissing").and_then(Value::as_bool).unwrap_or(false);
        let payload = json!({
            "tab": tab_id,
            "url": url,
            "title": scan_v.get("title").cloned().unwrap_or(Value::Null),
            // Which scope produced this list, and whether it matched.
            // Without the echo, a caller cannot tell a scoped scan from
            // an unscoped one, and a miss from an empty scope.
            "scope": within.clone(),
            "scopeMissing": scope_missing,
            "overlays": scan_v.get("overlays").cloned().unwrap_or(json!([])),
            "media": scan_v.get("media").cloned().unwrap_or(json!([])),
            "scroll": scan_v.get("scroll").cloned().unwrap_or(Value::Null),
            "total": total,
            "shown": shown.len(),
            "truncated": shown.len() < total,
            // How many of these came from inside a cross-origin frame.
            // Absent would read as "none", which is a lie when it is
            // non-zero, so it is always present.
            "framesScanned": scan_v.get("framesScanned").cloned().unwrap_or(json!(0)),
            "nodes": scan_v.get("nodes").cloned().unwrap_or(json!(0)),
            // A near-empty scan on a document with almost nothing in it
            // is usually a page that has not painted yet, not an empty
            // page. Saying so costs one field and saves a retry. Never
            // for a scope that missed: that answer is already exact.
            "suspect": if !scope_missing && suspect_render(
                total as u64,
                scan_v.get("nodes").and_then(Value::as_u64).unwrap_or(0),
            ) {
                json!("page may still be rendering: few nodes, no elements")
            } else {
                Value::Null
            },
            "elements": shown,
        });

        if shot_mode == "none" {
            return Ok(payload);
        }
        let (bytes, clipped, mime) =
                capture_guarded(conn, tab_id, &shot_mode, region, Some(max_w), &shot_fmt, 82)?;
        Ok(json!({ "__content": [
            { "type": "text", "text": payload.to_string() },
            { "type": "image", "data": b64encode(&bytes), "mimeType": mime },
            { "type": "text", "text": format!("(attached {shot_mode} capture, {} bytes{})", bytes.len(), if clipped { ", clipped to 8000px" } else { "" }) }
        ]}))
    })
}

pub(crate) fn text(cfg: &Cfg, args: &Value) -> Result<Value, String> {
    let tab = args.get("tab").and_then(Value::as_str);
    // What the page says, as text. `elements` answers "what can I click";
    // this answers "what does it say", which is the other half of reading
    // a page and had no tool until now - every agent call was writing the
    // same innerText extraction by hand.
    let within = match args.get("within") {
        None | Some(Value::Null) => "body".to_string(),
        Some(Value::String(s)) => nonempty_scope(s).unwrap_or_else(|| "body".to_string()),
        Some(_) => return Err("within must be a CSS selector".into()),
    };
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(4000) as usize;
    with_conn(&cfg.endpoint, tab, |conn, tab_id| {
        let js = format!(
            "(() => {{ \
               let r = null; \
               try {{ r = document.querySelector({sel}); }} catch (e) {{ return {{ err: String(e) }}; }} \
               if (!r) return {{ missing: true }}; \
               const t = (r.innerText || '').replace(/\\n{{3,}}/g, '\\n\\n').trim(); \
               return {{ text: t.slice(0, {lim}) }}; \
             }})()",
            sel = serde_json::to_string(&within).unwrap_or_else(|_| "\"body\"".into()),
            lim = limit
        );
        let v = conn.eval(&js)?;
        if let Some(e) = v.get("err").and_then(Value::as_str) {
            return Err(format!("text: {e}"));
        }
        // A selector that matches nothing is not the body: saying so
        // beats handing back the whole page as if it were the scope.
        let scope_missing = v.get("missing").and_then(Value::as_bool) == Some(true);
        let text = v.get("text").and_then(Value::as_str).unwrap_or("").to_string();
        Ok(json!({
            "tab": tab_id,
            "within": within,
            "scopeMissing": scope_missing,
            "chars": text.chars().count(),
            "truncated": text.chars().count() >= limit,
            "text": text,
        }))
    })
}

#[cfg(test)]
mod moved_tests {
    use super::*;

    #[test]
    fn a_bare_document_is_suspected_but_a_loaded_one_is_not() {
        // Truly empty document: nothing to scan and nothing to hold it up.
        assert!(suspect_render(0, 2));
        assert!(suspect_render(0, 399));
        // A loaded page with no interactive elements (an article, a scoped scan
        // that matched nothing): many nodes, so this is a real answer.
        assert!(!suspect_render(0, 812));
        assert!(!suspect_render(2, 900));
        // Elements found: never suspect, regardless of node count.
        assert!(!suspect_render(5, 100));
    }

    #[test]
    fn scan_placeholders_are_always_substituted() {
        // An unsubstituted placeholder is a ReferenceError in the page.
        for js in [scan_js(None), scan_js(Some("main"))] {
            assert!(!js.contains("__SCOPE__") && !js.contains("__PICK__"));
        }
        assert!(scan_js(Some("main")).contains(r#"const SCOPE = "main";"#));
    }

}
