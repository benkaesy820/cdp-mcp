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
  const root = SCOPE ? (document.querySelector(SCOPE) || document) : document;
  const sel = 'a[href], button, input, select, textarea, summary, [role="button"], [role="link"], [role="tab"], [role="menuitem"], [role="checkbox"], [role="radio"], [role="switch"], [role="combobox"], [contenteditable]:not([contenteditable="false"]), [onclick], [tabindex]:not([tabindex="-1"])';
  const elements = [];
  const refs = [];
  for (const el of root.querySelectorAll(sel)) {
    const r = el.getBoundingClientRect();
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
    if (el.disabled) rec.disabled = true;
    Object.assign(rec, b);
    // off-screen targets need scrolling before a coordinate click will land
    if (r.top < -4 || r.bottom > innerHeight + 4) rec.off = true;
    elements.push(rec);
    refs.push(el);
    if (elements.length >= 600) break;
    if (PICK && elements.length > PICK.i) break; // later elements cannot shift i
  }

  if (PICK) {
    const el = refs[PICK.i];
    if (!el) return { missing: true, total: elements.length };
    let r = el.getBoundingClientRect();
    let scrolled = false;
    if (PICK.scroll !== false && (r.top < 0 || r.bottom > innerHeight || r.left < 0 || r.right > innerWidth)) {
      // 'instant' overrides a page's `scroll-behavior: smooth`, which would
      // otherwise leave the element mid-flight when we measure it.
      el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
      r = el.getBoundingClientRect();
      scrolled = true;
    }
    const x = r.x + r.width / 2, y = r.y + r.height / 2;
    const out = { i: PICK.i, tag: elements[PICK.i].tag, label: elements[PICK.i].label,
                  x: Math.round(x), y: Math.round(y), scrolled };
    // Would a real click at the centre reach this element, or something on top?
    const top = document.elementFromPoint(x, y);
    out.hit = !!top && (top === el || el.contains(top) || top.contains(el));
    if (!out.hit && top) out.coveredBy = labelOf(top).slice(0, 60) || top.tagName.toLowerCase();
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
