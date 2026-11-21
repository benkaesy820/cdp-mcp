// The scanner: one injection that returns overlays and interactive elements
// together, so a single call tells the agent both "what can I touch" and
// "what is in the way".
//
// Index `i` is the ABSOLUTE position in `elements` and is stable for the page
// state: `input {click:{i}}` re-runs this same scan and indexes into it, so a
// filtered view never shifts what a click lands on.
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
          const opt = el.options[el.selectedIndex];
          if (opt) rec.selected = String(opt.text || '').trim().slice(0, 60);
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
  const labelOf = (el) => {
    const aria = el.getAttribute && el.getAttribute('aria-label');
    const t = (aria || el.innerText || el.value || el.getAttribute('title')
      || el.getAttribute('placeholder') || el.getAttribute('alt') || '').trim().replace(/\s+/g, ' ');
    return t.slice(0, 60);
  };
  const walk = (root) => {
    for (const el of root.querySelectorAll(sel)) {
      if (out.length >= 300) return;
      const r = el.getBoundingClientRect();
      if (r.width < 2 || r.height < 2) continue;
      const st = getComputedStyle(el);
      if (st.visibility === 'hidden' || st.display === 'none' || st.opacity === '0') continue;
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
          const opt = el.options[el.selectedIndex];
          if (opt) rec.selected = String(opt.text || '').trim().slice(0, 60);
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
