// The scanner: one injection that returns overlays, interactive elements and
// the menus a page keeps hidden, so a single call tells the agent "what can I
// touch", "what is in the way" and "what is behind that hamburger".
//
// Index `i` is the ABSOLUTE position in `elements` and is stable for the page
// state: `input {click:{i}}` re-runs this same scan and indexes into it, so a
// filtered view never shifts what a click lands on. A click also checks that
// `i` still names what the last scan showed (tag and label, plus which
// occurrence of a repeated label), so a page that inserted something above it
// is re-found or refused instead of clicked blind.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Map, Value};

use crate::cdp;
use crate::input::{blocked_click, check_irreversible, irreversible};
use crate::shot::{capture_guarded, parse_region};
use crate::{b64encode, clip_str, nonempty_scope, scan_cache, scope_cache, with_conn, Cfg};

/// Everything the page scan and the frame scan must agree on: which nodes
/// count as interactive, what they are called, and what state they are in.
/// One copy, so a frame can never name a button differently from the page
/// (the irreversible-click guard reads that name).
const COMMON_JS: &str = r##"
  const SEL = 'a[href], button, input:not([type="hidden"]), select, textarea, summary, [role="button"], [role="link"], [role="tab"], [role="menuitem"], [role="menuitemcheckbox"], [role="menuitemradio"], [role="option"], [role="checkbox"], [role="radio"], [role="switch"], [role="combobox"], [role="textbox"], [role="searchbox"], [role="slider"], [contenteditable]:not([contenteditable="false"]), [onclick], [tabindex]:not([tabindex="-1"]), label';
  // checkVisibility is far cheaper than resolving computed style per element,
  // and it sees hidden ancestors, which an element's own style does not.
  const VIS = { checkOpacity: true, checkVisibilityCSS: true };
  // A <label> only counts when it stands in for a checkbox or radio the page
  // hides: custom-styled toggles and checkbox-driven menus are clicked there,
  // and the input itself is invisible. Every other label duplicates its field.
  const stand = (el) => {
    if (el.tagName !== 'LABEL') return null;
    const c = el.control;
    if (!c || c.tagName !== 'INPUT' || !/^(checkbox|radio)$/.test(c.type)) return null;
    const r = c.getBoundingClientRect();
    return r.width < 2 || r.height < 2 || (c.checkVisibility && !c.checkVisibility(VIS)) ? c : null;
  };
  const wanted = (el) => el.tagName !== 'LABEL' || !!stand(el);
  const clean = (s, n) => String(s || '').replace(/\s+/g, ' ').trim().slice(0, n);
  const byIds = (el, attr) => {
    const v = el.getAttribute(attr);
    if (!v) return '';
    const root = el.getRootNode ? el.getRootNode() : document;
    return v.trim().split(/\s+/).map((id) => {
      const n = (root.getElementById && root.getElementById(id)) || document.getElementById(id);
      return n ? (n.innerText || n.textContent || '') : '';
    }).join(' ');
  };
  // Icon-only controls name themselves through classes, ids and sprite refs
  // (fa-bars, icon-search, #i-close). Only used when nothing else names them.
  const ICONS = [
    ['menu', /hamburger|burger|navicon|\bbars\b|\bmenu\b/], ['close', /close|\btimes\b|xmark/],
    ['search', /search|magnif/], ['settings', /settings|\bcog\b|\bgear\b/],
    ['more', /ellipsis|kebab|\bdots\b|\bmore\b|overflow/], ['cart', /cart|basket|\bbag\b/],
    ['user', /\buser\b|account|profile|avatar|person/], ['notifications', /\bbell\b|notif/],
    ['home', /\bhome\b|house/], ['share', /share/], ['like', /heart|\blike\b|thumbs/],
    ['bookmark', /bookmark/], ['add', /\bplus\b|\badd\b/], ['edit', /\bedit\b|pencil/],
    ['delete', /trash|delete/], ['download', /download/], ['upload', /upload/],
    ['filter', /filter|funnel/], ['sort', /\bsort\b/], ['play', /\bplay\b/], ['pause', /pause/],
    ['back', /arrow-left|chevron-left|\bback\b|\bprev/], ['next', /arrow-right|chevron-right|\bnext\b/],
    ['expand', /chevron-down|caret-down|\bexpand\b/], ['help', /\bhelp\b|question/],
    ['mail', /\bmail\b|envelope|email/], ['chat', /\bchat\b|comment|message/], ['send', /\bsend\b|paper-plane/],
    ['copy', /\bcopy\b|clipboard/], ['refresh', /refresh|reload|\bsync\b/], ['language', /globe|language|translate/],
  ];
  const iconOf = (el) => {
    const bits = [el.getAttribute('class'), el.id, el.getAttribute('data-icon'), el.getAttribute('data-testid')];
    let k = 0;
    for (const c of el.querySelectorAll('svg, i, span, use, img')) {
      bits.push(c.getAttribute('class'), c.getAttribute('data-icon'), c.getAttribute('href'),
                c.getAttribute('xlink:href'), c.tagName === 'IMG' ? c.getAttribute('src') : '');
      if (++k >= 4) break;
    }
    const hay = bits.filter(Boolean).join(' ').toLowerCase();
    if (!hay) return '';
    for (const [name, re] of ICONS) if (re.test(hay)) return 'icon:' + name;
    return '';
  };
  const isField = (el) => {
    const t = el.tagName;
    if (t === 'INPUT') return !/^(submit|button|reset|image)$/.test(el.type);
    if (t === 'TEXTAREA' || t === 'SELECT' || el.isContentEditable) return true;
    return /^(textbox|searchbox)$/.test(el.getAttribute('role') || '');
  };
  // What a person reading the screen would call it. Visible text wins over a
  // tooltip (`title` is often a sentence: "You are encouraged to create an
  // account…" for "Create account"), and a field is named by its <label>, not
  // by what has been typed into it, so the name survives typing.
  const labelOf = (el) => {
    const t = el.tagName, field = isField(el);
    let s = el.getAttribute('aria-label') || byIds(el, 'aria-labelledby');
    if (!s && field && el.labels && el.labels.length)
      s = [...el.labels].map((l) => l.innerText || l.textContent || '').join(' ');
    if (!s && t === 'INPUT' && /^(submit|button|reset)$/.test(el.type)) s = el.value || el.type;
    if (!s && t === 'INPUT' && el.type === 'image') s = el.alt;
    // Text that is only a glyph (×, ⋮, +) is not a name: `title="Delete"` on
    // a × button is, and the irreversible-click guard depends on reading it.
    let glyph = '';
    if (!s && !field) {
      let txt = el.innerText || '';
      // A clickable card: its heading names it, not its first 60 characters.
      if (txt.length > 60) {
        const h = el.querySelector('h1, h2, h3, h4, h5, h6, [role="heading"]');
        if (h) txt = h.innerText || h.textContent || txt;
      }
      if (/[\p{L}\p{N}]/u.test(txt)) s = txt;
      else {
        glyph = txt.trim();
        const n = el.querySelector('img[alt]:not([alt=""]), [aria-label], svg title');
        if (n) s = n.getAttribute('alt') || n.getAttribute('aria-label') || n.textContent || '';
      }
    }
    if (!s || !s.trim()) s = el.getAttribute('title') || el.getAttribute('placeholder') ||
      el.getAttribute('aria-placeholder') || el.getAttribute('data-placeholder') || el.getAttribute('name') || '';
    if (!s.trim()) s = glyph || iconOf(el);
    if (!s && t === 'INPUT' && el.type && el.type !== 'text') s = '[' + el.type + ']';
    return clean(s, 60);
  };
  const roleOf = (el) => {
    const r = el.getAttribute('role');
    if (r) return r;
    const t = el.tagName;
    if (t === 'LABEL') return stand(el) ? stand(el).type : '';
    if (t === 'A') return 'link';
    if (t === 'BUTTON') return 'button';
    if (t === 'SELECT') return 'combobox';
    if (t === 'TEXTAREA') return 'textbox';
    if (t === 'INPUT') {
      const ty = el.type;
      if (ty === 'checkbox' || ty === 'radio') return ty;
      if (/^(submit|button|reset|image|file)$/.test(ty)) return 'button';
      return ty === 'range' ? 'slider' : 'textbox';
    }
    return el.isContentEditable ? 'textbox' : '';
  };
  // :disabled is the browser's own answer: the disabled attribute and an
  // ancestor fieldset[disabled], minus the first-legend exception, and it
  // leaves a link or a [role=button] inside the fieldset clickable.
  const disabledOf = (el) => {
    if (el.disabled || el.getAttribute('aria-disabled') === 'true') return true;
    try { return !!(el.matches && el.matches(':disabled')); } catch (e) { return false; }
  };
  // Current form and widget state, so a scan answers "what is typed here",
  // "is this menu open" and "which tab is active" without a second eval.
  // Passwords never leave the page in plaintext.
  const stateOf = (el, rec) => {
    const t = el.tagName, role = el.getAttribute('role') || '';
    const c = stand(el);
    if (c) { rec.checked = !!c.checked; if (c.disabled) rec.disabled = true; return; }
    if (t === 'INPUT') {
      const ty = (el.type || 'text').toLowerCase();
      if (ty === 'checkbox' || ty === 'radio') rec.checked = !!el.checked;
      else if (ty === 'password') { if (el.value) rec.value = '***'; }
      else if (!/^(submit|button|reset|image)$/.test(ty) && el.value) rec.value = String(el.value).slice(0, 60);
    } else if (t === 'TEXTAREA') {
      if (el.value) rec.value = String(el.value).slice(0, 60);
    } else if (t === 'SELECT') {
      // `selectedIndex` reports one entry; a multiple select can have several,
      // and hiding the rest is how a selection reads as lost.
      if (el.multiple) rec.selected = [...el.selectedOptions].map((o) => clean(o.text, 60));
      else { const o = el.options[el.selectedIndex]; if (o) rec.selected = clean(o.text, 60); }
    } else if (el.isContentEditable || role === 'textbox' || role === 'searchbox') {
      const v = clean(el.innerText, 60);
      if (v) rec.value = v;
    }
    // A native control already answered: role=switch on a checked checkbox
    // usually has no aria-checked, and must not read as false.
    if (rec.checked === undefined && /^(checkbox|switch|radio|menuitemcheckbox|menuitemradio)$/.test(role)) {
      const c = el.getAttribute('aria-checked');
      rec.checked = c === 'mixed' ? 'mixed' : c === 'true';
    }
    const ex = el.getAttribute('aria-expanded');
    if (ex === 'true' || ex === 'false') rec.expanded = ex === 'true';
    else if (t === 'SUMMARY' && el.parentElement && el.parentElement.tagName === 'DETAILS') rec.expanded = el.parentElement.open;
    const hp = el.getAttribute('aria-haspopup');
    if (hp && hp !== 'false') rec.popup = hp === 'true' ? 'menu' : hp;
    const pr = el.getAttribute('aria-pressed');
    if (pr === 'true' || pr === 'false') rec.pressed = pr === 'true';
    const cur = el.getAttribute('aria-current');
    if ((cur && cur !== 'false') || el.getAttribute('aria-selected') === 'true') rec.current = true;
    if (el.required || el.getAttribute('aria-required') === 'true') rec.required = true;
    let bad = el.getAttribute('aria-invalid') === 'true';
    try { if (!bad && el.matches && el.matches(':user-invalid')) bad = true; } catch (e) {}
    if (bad) rec.invalid = true;
  };
  // The landmark an element sits in. Main content is the default and is not
  // named, so a page reads as "the chrome around it" plus everything else.
  const REGION = 'dialog, [role="dialog"], [role="alertdialog"], nav, [role="navigation"], header, [role="banner"], footer, [role="contentinfo"], aside, [role="complementary"], main, [role="main"], [role="search"]';
  const regionOf = (el) => {
    const n = el.closest && el.closest(REGION);
    if (!n) return '';
    const r = n.getAttribute('role') || '', t = n.tagName;
    if (t === 'DIALOG' || r === 'dialog' || r === 'alertdialog') return 'dialog';
    if (r === 'search') return 'search';
    if (t === 'NAV' || r === 'navigation') return 'nav';
    if (t === 'HEADER' || r === 'banner') return 'header';
    if (t === 'FOOTER' || r === 'contentinfo') return 'footer';
    if (t === 'ASIDE' || r === 'complementary') return 'aside';
    return '';
  };
  // Attributes that survive a label change (Follow -> Following), so a click
  // can tell "same element, new text" from "different element".
  const keyOf = (el) => clean(el.id || el.getAttribute('data-testid') ||
    (el.tagName === 'A' ? el.getAttribute('href') : '') || el.getAttribute('aria-controls') || '', 80);
  // Would a real click at a point reach this element, or something on top?
  // Hit-test through shadow roots and same-origin frames, the way the browser
  // will route the click.
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
  // Does element `el` still match what the caller last saw? Same tag and
  // label (or a stable key when only the text changed), and for a repeated
  // label, the same occurrence of it.
  const sameAs = (E, el, now, before) => {
    if (!E) return true;
    if (E.tag !== now.tag) return false;
    if (E.label !== now.label) return !!(E.key && E.key === now.key);
    if (E.count > 1 && before) {
      let n = 0;
      for (const o of before) if (o.tagName === el.tagName && labelOf(o) === now.label) n++;
      now.n = n;
      return n === E.n;
    }
    return true;
  };
  // Checked before anything changes: a request for a missing option used to
  // set the value anyway and leave the <select> with nothing selected.
  const applySelect = (el, want, out) => {
    const s = el.tagName === 'SELECT' ? el : el.closest('select');
    if (!s) { out.selectError = 'not a select'; return; }
    const opts = [...s.options];
    // The option's value, or the visible text the scan showed: "Pro" finds
    // <option value="pro">Pro</option>.
    const resolve = (w) => {
      w = String(w);
      const o = opts.find((o) => o.value === w) ||
        opts.find((o) => clean(o.text, 200).toLowerCase() === w.trim().toLowerCase());
      return o ? o.value : null;
    };
    const list = Array.isArray(want) ? want : [want];
    const vals = list.map(resolve);
    const missing = list.filter((w, k) => vals[k] === null);
    if (missing.length) {
      const known = opts.slice(0, 12).map((o) => { const t = clean(o.text, 30); return o.value === t ? t : t + '=' + o.value; });
      out.selectError = 'no option ' + missing.join(', ') + ' (options: ' + known.join(', ') + (opts.length > 12 ? ', …' : '') + ')';
      return;
    }
    if (Array.isArray(want) && !s.multiple && vals.length > 1) { out.selectError = 'this <select> takes one value; it is not multiple'; return; }
    if (Array.isArray(want)) { const set = new Set(vals); for (const o of opts) o.selected = set.has(o.value); }
    else s.value = vals[0];
    s.dispatchEvent(new Event('input', { bubbles: true }));
    s.dispatchEvent(new Event('change', { bubbles: true }));
    out.asked = Array.isArray(want) ? vals : vals[0];
    out.selected = Array.isArray(want) ? opts.filter((o) => o.selected).map((o) => o.value) : s.value;
  };
"##;

/// The page scan. OPTS = {scope, pick, cap, extras}:
/// - `scope` limits elements to a subtree (overlays stay page-wide);
/// - `pick` = {i, scroll?, select?, clearAds?, expect?} resolves element i of
///   this same scan and acts on the element itself — indexing by element, not
///   by point, is what makes a click below the fold land;
/// - `extras` adds hidden-menu groups and media, which a click never needs.
const SCAN_TPL: &str = r##"(() => {
  __COMMON__
  const OPTS = __OPTS__;
  const SCOPE = OPTS.scope, PICK = OPTS.pick, CAP = OPTS.cap;
  const GROUP = !PICK && OPTS.extras;
  const scopeEl = SCOPE ? document.querySelector(SCOPE) : null;
  // A miss is not "the whole page": the element list must not silently become
  // the document. Overlays and media are page-wide by design, so they are
  // still scanned.
  const scopeMissed = !!(SCOPE && !scopeEl);
  if (scopeMissed && PICK) return { missing: true, i: PICK.i, total: 0, scopeMissing: true };
  const root = scopeEl || document;
  const elements = [];
  const refs = [];
  // The same-origin <iframe> chain each element sits in ([] = this document).
  const chains = [];
  // Interactive elements the page hides — closed menus, drawers, collapsed
  // sections — as [element, container-or-null]. Grouped after the scan.
  const hidden = [];

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
  const shown = (el, r) => r.width >= 2 && r.height >= 2 && (!el.checkVisibility || el.checkVisibility(VIS)) &&
    r.bottom >= -2000 && r.top <= innerHeight + 4000;
  // The fixed-position ancestor of an element, if any: an off-canvas drawer
  // parks its links beside the viewport, where no scroll will ever bring them.
  const fixedOf = new Map();
  const inFixed = (el) => {
    const path = [];
    let found = null;
    for (let n = el; n && n.nodeType === 1; n = n.parentElement) {
      if (fixedOf.has(n)) { found = fixedOf.get(n); break; }
      path.push(n);
      if (getComputedStyle(n).position === 'fixed') { found = n; break; }
    }
    for (const n of path) fixedOf.set(n, found);
    return found;
  };
  const groupable = (el) => {
    const t = el.tagName;
    return t === 'A' || t === 'BUTTON' || t === 'SUMMARY' ||
      /^(button|link|menuitem|menuitemcheckbox|menuitemradio|tab|option)$/.test(el.getAttribute('role') || '');
  };
  let capped = false;
  // A pick only has to see as far as its index; a scan stops at the cap.
  const full = () => refs.length >= (PICK ? Math.min(PICK.i + 1, 2000) : CAP);
  const seen = new Set();

  // Light DOM first, then open shadow roots (web components keep their
  // buttons there), then same-origin frames. querySelectorAll stops at both
  // boundaries, which is why these were invisible. The order is fixed, so an
  // index means the same element on every scan of the same page state.
  const collect = (node, chain, frameLabel) => {
    const fb = frameBox(chain);
    for (const el of node.querySelectorAll(SEL)) {
      if (!wanted(el)) continue;
      const r = placed(el, fb);
      if (r.width < 2 || r.height < 2 || (el.checkVisibility && !el.checkVisibility(VIS))) {
        if (GROUP && !chain.length && hidden.length < 3000 && groupable(el) &&
            el.checkVisibility && !el.checkVisibility(VIS)) hidden.push([el, null]);
        continue;
      }
      if (r.bottom < -2000 || r.top > innerHeight + 4000) continue;
      if (r.right <= fb.L || r.left >= fb.R) {
        const fx = inFixed(el);
        if (fx) {
          if (GROUP && !chain.length && hidden.length < 3000 && groupable(el)) hidden.push([el, fx]);
          continue;
        }
      }
      const b = { x: Math.round(r.x + r.width / 2), y: Math.round(r.y + r.height / 2),
                  w: Math.round(r.width), h: Math.round(r.height) };
      const key = b.x + ',' + b.y + ',' + b.w;
      if (seen.has(key)) continue;
      seen.add(key);
      // Checked after visibility and dedupe, so `capped` means a real matching
      // element was left out.
      if (full()) { capped = true; return; }
      refs.push(el);
      chains.push(chain);
      // A pick needs positions, not records: naming every element up to i
      // would cost innerText on each of them.
      if (PICK) { elements.push(null); continue; }
      const rec = { i: elements.length, tag: el.tagName.toLowerCase(), label: labelOf(el) };
      const role = roleOf(el);
      if (role) rec.role = role;
      if (disabledOf(el)) rec.disabled = true;
      stateOf(el, rec);
      const reg = regionOf(el);
      if (reg) rec.in = reg;
      const k = keyOf(el);
      if (k) rec.key = k;
      if (frameLabel) rec.frame = frameLabel;
      Object.assign(rec, b);
      // Off-screen (or outside its frame's visible area): needs scrolling
      // before a coordinate click will land.
      if (r.top < fb.T - 4 || r.bottom > fb.B + 4 || b.x < fb.L || b.x > fb.R) rec.off = true;
      elements.push(rec);
    }
    for (const host of node.querySelectorAll('*')) {
      if (PICK ? full() : capped) return;
      if (host.shadowRoot) collect(host.shadowRoot, chain, frameLabel);
    }
    for (const f of node.querySelectorAll('iframe, frame')) {
      if (PICK ? full() : capped) return;
      let doc = null;
      try { doc = f.contentDocument; } catch (e) { /* cross-origin */ }
      if (!doc || !doc.documentElement || !shown(f, placed(f, fb))) continue;
      collect(doc, chain.concat([f]), clean(f.getAttribute('title') || f.getAttribute('name') || f.id || 'frame', 40));
    }
  };
  collect(root, [], '');

  // A miss was scanned as the document so overlays and media are real; the
  // element list is what the caller did not ask for, so it is what gets emptied.
  if (scopeMissed) { elements.length = 0; refs.length = 0; chains.length = 0; hidden.length = 0; }

  if (PICK) {
    const el = refs[PICK.i];
    if (!el) return { missing: true, total: refs.length };
    const now = { tag: el.tagName.toLowerCase(), label: labelOf(el), key: keyOf(el) };
    if (!sameAs(PICK.expect, el, now, PICK.expect && PICK.expect.count > 1 ? refs.slice(0, PICK.i) : null))
      return { changed: true, i: PICK.i, now };
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
    const out = { i: PICK.i, tag: now.tag, label: now.label, x: Math.round(x), y: Math.round(y), scrolled };
    out.connected = el.isConnected;
    out.zero = r.width === 0 || r.height === 0;
    // Document-space bounds after any scroll: the clip for an element shot.
    // `r` is in the top document's viewport space, frames included.
    out.rect = { x: Math.round(r.x + scrollX), y: Math.round(r.y + scrollY),
                 w: Math.round(r.width), h: Math.round(r.height) };
    out.disabled = disabledOf(el);
    const top = deepAt(x, y);
    out.hit = !!top && (holds(el, top) || holds(top, el));
    if (!out.hit && top) {
      // The blocker, in the same compact shape as the wait diagnostics.
      const desc = (n) => {
        if (!n || n.nodeType !== 1) return null;
        const o = { tag: n.tagName.toLowerCase() };
        const role = n.getAttribute('role');
        if (role) o.role = role;
        const label = n.getAttribute('aria-label') || n.getAttribute('placeholder') || n.getAttribute('name');
        if (label) o.label = label.slice(0, 60);
        const t = clean(n.innerText || n.textContent, 120);
        if (t) o.text = t;
        return o;
      };
      const b = desc(top);
      if (b && !b.label && !b.text) {
        const d = top.closest && top.closest('[role="dialog"],dialog,[aria-modal="true"]');
        if (d) Object.assign(b, desc(d));
      }
      out.blocker = b;
      // Opt-in: when the only thing in the way is an advertisement, hide it
      // and re-test, so the click reaches the revealed target the way it
      // would after a person dismisses it. Anything dialog-shaped, anything
      // holding a form or a password field, and anything unclassified is left
      // alone: those refusals are the point.
      if (PICK.clearAds) {
        const dialogish = (n) => {
          if (!n || n.nodeType !== 1) return false;
          if (n.tagName === 'DIALOG') return true;
          const role = (n.getAttribute('role') || '').toLowerCase();
          if (role === 'dialog' || role === 'alertdialog') return true;
          if (n.getAttribute('aria-modal') === 'true') return true;
          try { if (n.querySelector('input[type=password],form')) return true; } catch (e) {}
          return false;
        };
        const AD = /(^|[-_\s])(ad|ads|advert|advertisement|sponsor|sponsored|promo|promotion|banner)([-_\s]|$)/i;
        const AD_HOST = /(doubleclick|googlesyndication|adservice|taboola|outbrain|adsystem|criteo|pubmatic)/i;
        const adish = (n) => {
          if (!n || n.nodeType !== 1) return false;
          if (n.tagName === 'IFRAME' && AD_HOST.test(n.src || '')) return true;
          const a = [n.id || '', n.className ? String(n.className) : '', n.getAttribute('aria-label') || ''].join(' ');
          if (AD.test(a)) return true;
          try { if (n.querySelector('[id*="google_ads"],[id^="aswift"],[class*="adsbygoogle"]')) return true; } catch (e) {}
          return false;
        };
        const line = [];
        for (let n = top, k = 0; n && k < 6; k++, n = up(n)) line.push(n);
        const dial = line.find(dialogish);
        const ad = dial ? null : line.find(adish);
        if (dial) out.clearRefused = 'dialog';
        else if (!ad) out.clearRefused = 'unclassified';
        else {
          const prev = ad.getAttribute('style');
          ad.style.setProperty('display', 'none', 'important');
          const again = deepAt(x, y);
          if (again && (holds(el, again) || holds(again, el))) {
            out.hit = true;
            out.cleared = desc(ad) || { tag: ad.tagName.toLowerCase() };
          } else {
            // It did not help, so put the page back as it was.
            if (prev === null) ad.removeAttribute('style');
            else ad.setAttribute('style', prev);
            out.clearRefused = 'still covered';
          }
        }
      }
    }
    if (PICK.clearAds && !out.hit && out.blocker) out.clearRefused = out.clearRefused || 'unclassified';
    if (PICK.select !== undefined) applySelect(el, PICK.select, out);
    return out;
  }

  const index = new Map(refs.map((e, k) => [e, k]));

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
  // How much of the viewport an element actually covers. Its own size is not
  // the answer: an off-canvas drawer is tall and wide and covers nothing.
  const onScreen = (r) => ({
    w: Math.max(0, Math.min(r.right, innerWidth) - Math.max(r.left, 0)),
    h: Math.max(0, Math.min(r.bottom, innerHeight) - Math.max(r.top, 0)),
  });
  for (const el of layerCandidates) {
    const cs = getComputedStyle(el);
    if (cs.position !== 'fixed' && cs.position !== 'absolute') continue;
    const v = onScreen(el.getBoundingClientRect());
    // A cookie wall is usually wide and short, a modal is boxy: accept either
    // a decent share of the viewport, or something that spans its width.
    const areaShare = (v.w * v.h) / (innerWidth * innerHeight);
    const spansWidth = v.w >= innerWidth * 0.7 && v.h >= 40;
    if (areaShare < 0.15 && !spansWidth) continue;
    if (cs.position === 'absolute' && Number(cs.zIndex || 0) < 10) continue;
    candidates.push(el);
  }
  // Whole words only: a bare `x` or `ok` inside "Explore" or "Book now" is not
  // a way out of a dialog.
  const CLOSE_RE = /\b(close|dismiss|cancel|accept|agree|allow|ok|okay|got it|no thanks|not now|later|continue|skip|reject|deny|decline|refuse|necessary|essential|manage|preferences|save choices)\b|[✕×]|^x$/i;
  for (const el of candidates) {
    const r = el.getBoundingClientRect();
    if (!shown(el, r)) continue;
    const v = onScreen(r);
    if (v.w < 2 || v.h < 2) continue;
    const label = clean(el.getAttribute('aria-label') || byIds(el, 'aria-labelledby') || el.innerText, 120);
    const dk = Math.round(r.y) + ':' + label.slice(0, 24);
    if (seenOverlay.has(dk)) continue;
    seenOverlay.add(dk);
    const role = el.getAttribute('role') || '';
    const kind = role === 'alertdialog' ? 'alert'
               : role === 'dialog' || el.tagName === 'DIALOG' ? 'dialog' : 'layer';
    const closers = [];
    for (const b2 of el.querySelectorAll('button, [role="button"], a[href], input[type="submit"]')) {
      const r2 = b2.getBoundingClientRect();
      if (!shown(b2, r2)) continue;
      const lbl = labelOf(b2);
      if (!lbl || !CLOSE_RE.test(lbl)) continue;
      const rec = { label: lbl };
      if (index.has(b2)) rec.i = index.get(b2); // clickable by absolute index
      Object.assign(rec, { x: Math.round(r2.x + r2.width / 2), y: Math.round(r2.y + r2.height / 2),
                           w: Math.round(r2.width), h: Math.round(r2.height) });
      closers.push(rec);
      if (closers.length >= 4) break;
    }
    const rec = { kind, label, x: Math.round(r.x + r.width / 2), y: Math.round(r.y + r.height / 2),
                  w: Math.round(r.width), h: Math.round(r.height) };
    if (closers.length) rec.closers = closers;
    overlays.push(rec);
    if (overlays.length >= 6) break;
  }

  // ---- collapsed: what the page keeps hidden, and what opens it ----
  // Each group is the outermost hidden container of some interactive elements
  // (a closed drawer, a hover menu, a <details>), with a sample of what is in
  // it and the index of the visible control that opens it, when one can be
  // told apart. The items are read without opening anything.
  const collapsed = [];
  if (GROUP && hidden.length) {
    const visible = new Map();
    const isVis = (n) => {
      if (!visible.has(n)) visible.set(n, !n.checkVisibility || n.checkVisibility(VIS));
      return visible.get(n);
    };
    const groups = new Map();
    for (const [el, fx] of hidden) {
      let g = fx;
      if (!g) {
        g = el;
        while (g.parentElement && g.parentElement !== document.body && !isVis(g.parentElement)) g = g.parentElement;
        // A closed <details> hides each child separately; the details is the group.
        if (g.parentElement && g.parentElement.tagName === 'DETAILS' && !g.parentElement.open) g = g.parentElement;
      }
      if (!groups.has(g)) groups.set(g, []);
      groups.get(g).push(el);
    }
    const attrs = (n) => String(n.getAttribute('class') || '') + ' ' + (n.id || '');
    const named = (n) => /^(NAV|ASIDE|DIALOG|UL|OL|MENU|DETAILS)$/.test(n.tagName) ||
      /^(navigation|menu|menubar|listbox|dialog|complementary|tabpanel)$/.test(n.getAttribute('role') || '') ||
      /menu|drawer|sidebar|offcanvas|dropdown|popover|flyout|submenu|\bnav/i.test(attrs(n));
    const popupish = (n) => n.hasAttribute('aria-expanded') || n.hasAttribute('aria-haspopup');
    const at = (n) => {
      if (!n) return -1;
      if (index.has(n)) return index.get(n);
      // The checkbox behind a checkbox-driven menu is hidden; its label is not.
      for (const l of n.labels || []) if (index.has(l)) return index.get(l);
      return -1;
    };
    // The trigger itself, or a wrapper holding exactly one control. A sibling
    // holding several (a whole <header>) says nothing about which one opens it.
    const soleIn = (n) => {
      if (at(n) >= 0) return at(n);
      let k = -1;
      for (const c of n.querySelectorAll(SEL)) if (index.has(c)) { if (k >= 0) return -1; k = index.get(c); }
      return k;
    };
    // Every "this opens #x" in one pass, instead of a document-wide query per group.
    const opens = new Map();
    for (const t of document.querySelectorAll('[aria-controls], [aria-owns], [data-target^="#"], [data-bs-target^="#"], a[href^="#"], [popovertarget]')) {
      const ids = [t.getAttribute('aria-controls'), t.getAttribute('aria-owns'), t.getAttribute('popovertarget'),
                   (t.getAttribute('data-target') || '').slice(1), (t.getAttribute('data-bs-target') || '').slice(1),
                   t.tagName === 'A' ? t.getAttribute('href').slice(1) : ''].join(' ');
      for (const id of ids.split(/\s+/)) if (id && !opens.has(id)) opens.set(id, t);
    }
    const toggleOf = (g) => {
      for (let n = g, depth = 0; n && n !== document.body && depth < 3; n = n.parentElement, depth++) {
        if (n.id && at(opens.get(n.id)) >= 0) return at(opens.get(n.id));
        const p = n.parentElement;
        if (n.tagName === 'DETAILS' && at(n.querySelector(':scope > summary')) >= 0) return at(n.querySelector(':scope > summary'));
        // A trigger sits just before what it opens: <button>Products</button><div class=menu>.
        // Only trusted for a named container or a trigger that says it opens something.
        for (let s = n.previousElementSibling, m = 0; s && m < 2; s = s.previousElementSibling, m++) {
          if (at(s) >= 0 && (named(g) || popupish(s) || stand(s))) return at(s);
          const k = soleIn(s);
          if (k >= 0 && (popupish(refs[k]) || stand(refs[k]))) return k;
        }
        if (p && at(p) >= 0 && (named(g) || popupish(p))) return at(p);
      }
      return -1;
    };
    // A drawer with no wiring at all still has a hamburger somewhere.
    const MENU_RE = /\bmenu\b|navigation|sidebar|drawer|icon:menu/i;
    const burger = () => {
      const e = elements.find((e) => e && e.expanded !== true && MENU_RE.test(e.label));
      return e ? e.i : -1;
    };
    const kindOf = (g, fx) => {
      const t = g.tagName, r = g.getAttribute('role') || '';
      if (t === 'DETAILS') return 'details';
      if (t === 'DIALOG' || r === 'dialog') return 'dialog';
      if (t === 'NAV' || r === 'navigation') return fx ? 'drawer' : 'nav';
      if (t === 'ASIDE' || r === 'complementary' || /drawer|sidebar|offcanvas/i.test(attrs(g)) || fx) return 'drawer';
      if (/^(menu|menubar|listbox)$/.test(r) || t === 'MENU' || /menu|dropdown|popover|flyout/i.test(attrs(g))) return 'menu';
      if (t === 'UL' || t === 'OL') return 'list';
      return 'panel';
    };
    let looked = 0;
    for (const [g, els] of groups) {
      if (collapsed.length >= 8 || looked >= 60) break;
      if (els.length < 2) continue;
      looked++;
      let t = toggleOf(g);
      if (t < 0 && !named(g)) continue;
      const kind = kindOf(g, fixedOf.get(els[0]) === g);
      // Nothing points at it, so the page's menu button is a guess, and is
      // reported as one.
      let guess = false;
      if (t < 0 && (kind === 'drawer' || kind === 'nav')) { t = burger(); guess = t >= 0; }
      const sample = [];
      for (const e of els) {
        // innerText is empty under visibility:hidden; the words are still there.
        const l = (labelOf(e) || clean(e.textContent, 30)).slice(0, 30);
        if (l && !sample.includes(l)) sample.push(l);
        if (sample.length >= 6) break;
      }
      const rec = { kind, items: els.length, sample };
      if (t >= 0) rec.toggle = t;
      if (guess) rec.guess = true;
      collapsed.push(rec);
    }
  }

  // media: report, never touch. A playing video must survive a scan.
  const media = [];
  if (OPTS.extras) for (const m of document.querySelectorAll('video, audio')) {
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
    scroll: { y: Math.round(scrollY), h: Math.round(document.documentElement.scrollHeight), vh: innerHeight },
    media,
    title: document.title.slice(0, 120),
    total: elements.length,
    // How many nodes the document actually holds. A React app mid-hydration
    // and a genuinely empty page return the same empty element list; the node
    // count is what tells them apart. A live collection's length is O(1).
    nodes: document.getElementsByTagName('*').length,
    overlays,
    collapsed,
    elements,
    capped,
  };
})()"##;

/// A cheap post-action check: only dialogs that appeared *because* we acted.
/// The full scan costs ~30 ms on a real article; this is a fraction of it and
/// catches the case that actually swallows the next click.
pub const OVERLAYS_QUICK_JS: &str = r#"(() => {
  const out = [];
  const VIS = { checkOpacity: true, checkVisibilityCSS: true };
  for (const el of document.querySelectorAll('[role="dialog"], [role="alertdialog"], [aria-modal="true"], dialog[open]')) {
    const r = el.getBoundingClientRect();
    if (r.width < 2 || r.height < 2 || (el.checkVisibility && !el.checkVisibility(VIS))) continue;
    const label = String(el.getAttribute('aria-label') || el.innerText || '').trim().replace(/\s+/g, ' ').slice(0, 100);
    out.push({ kind: el.getAttribute('role') === 'alertdialog' ? 'alert' : 'dialog', label,
               x: Math.round(r.x + r.width / 2), y: Math.round(r.y + r.height / 2) });
    if (out.length >= 4) break;
  }
  return out;
})()"#;

/// What a cross-origin frame offers, in its OWN coordinate space (top-left
/// corners; the caller adds the frame's offset and makes them centres).
///
/// Separate from the page scan because a frame's document knows nothing of
/// the page around it, but built from the same COMMON_JS, so a label, a role
/// and a form state read the same on both sides of a site boundary.
const FRAME_TPL: &str = r##"(() => {
  __COMMON__
  const PICK = __FRAME_PICK__;
  const out = [];
  const nodes = [];
  const seen = new Set();
  let capped = false;
  const walk = (root) => {
    for (const el of root.querySelectorAll(SEL)) {
      if (PICK && nodes.length > PICK.i) return;
      if (!wanted(el)) continue;
      const r = el.getBoundingClientRect();
      if (r.width < 2 || r.height < 2) continue;
      if (el.checkVisibility && !el.checkVisibility(VIS)) continue;
      const key = Math.round(r.x) + ',' + Math.round(r.y) + ',' + Math.round(r.width);
      if (seen.has(key)) continue;
      // After visibility and dedupe: a real matching element was left out.
      if (nodes.length >= 300) { capped = true; return; }
      seen.add(key);
      nodes.push(el);
      if (PICK) continue;
      const rec = { tag: el.tagName.toLowerCase(), label: labelOf(el), x: Math.round(r.x), y: Math.round(r.y),
                    w: Math.round(r.width), h: Math.round(r.height) };
      const role = roleOf(el);
      if (role) rec.role = role;
      if (disabledOf(el)) rec.disabled = true;
      stateOf(el, rec);
      const reg = regionOf(el);
      if (reg) rec.in = reg;
      const k = keyOf(el);
      if (k) rec.key = k;
      out.push(rec);
    }
    for (const host of root.querySelectorAll('*')) {
      if (capped || (PICK && nodes.length > PICK.i)) return;
      if (host.shadowRoot) walk(host.shadowRoot);
    }
  };
  walk(document);
  if (PICK) {
    const el = nodes[PICK.i];
    if (!el || !el.isConnected) return JSON.stringify({ missing: true, total: nodes.length });
    const now = { tag: el.tagName.toLowerCase(), label: labelOf(el), key: keyOf(el) };
    if (!sameAs(PICK.expect, el, now, null)) return JSON.stringify({ changed: true, now });
    if (PICK.scroll) el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
    const r = el.getBoundingClientRect();
    const rec = {
      i: PICK.i, tag: now.tag, label: now.label,
      connected: el.isConnected, zero: r.width < 2 || r.height < 2,
      lx: Math.round(r.x + r.width / 2), ly: Math.round(r.y + r.height / 2),
    };
    if (disabledOf(el)) rec.disabled = true;
    rec.outside = rec.lx < 0 || rec.ly < 0 || rec.lx > innerWidth || rec.ly > innerHeight;
    const hit = deepAt(rec.lx, rec.ly);
    rec.hit = !!hit && (holds(el, hit) || holds(hit, el));
    if (!rec.hit) {
      rec.blocker = hit ? String(hit.tagName || '').toLowerCase() : 'nothing';
      rec.blockerLabel = hit ? labelOf(hit) : '';
    }
    if (PICK.select !== undefined) applySelect(el, PICK.select, rec);
    return JSON.stringify(rec);
  }
  return JSON.stringify({ elements: out, capped, title: document.title.slice(0, 80), url: location.href.slice(0, 200) });
})()"##;

/// The page scan with its options filled in. COMMON_JS goes in first, so a
/// scope string that happens to contain a placeholder is never substituted.
fn scan_src(scope: Option<&str>, pick: Option<&Value>, cap: u64, extras: bool) -> String {
    let opts = json!({ "scope": scope, "pick": pick, "cap": cap, "extras": extras });
    SCAN_TPL.replace("__COMMON__", COMMON_JS).replace("__OPTS__", &opts.to_string())
}

/// The scan `elements` runs (None = whole document).
pub(crate) fn scan_js(scope: Option<&str>, cap: u64) -> String {
    scan_src(scope, None, cap, true)
}

/// The scan `dismiss` runs: overlays and their closer indexes, without the
/// hidden-menu grouping and media it has no use for.
pub(crate) fn dismiss_scan_js() -> String {
    scan_src(None, None, 600, false)
}

fn frame_src(pick: &Value) -> String {
    FRAME_TPL.replace("__COMMON__", COMMON_JS).replace("__FRAME_PICK__", &pick.to_string())
}

/// What the caller last saw at index `i` in `scope`, if the tab's last scan
/// was taken in that scope: tag, label, stable key, and which occurrence of a
/// repeated label it was. A click compares the live element against this.
fn expected(tab: &str, scope: Option<&str>, i: u64) -> Option<Value> {
    let cache = scan_cache().lock().ok()?;
    let scan = cache.get(tab)?;
    if scan.get("scope").and_then(Value::as_str) != scope {
        return None;
    }
    let els = scan.get("elements")?.as_array()?;
    let e = els.get(i as usize)?;
    if e.get("target").is_some() {
        return None; // a frame element is checked inside its frame
    }
    let tag = e.get("tag")?.as_str()?;
    let label = e.get("label")?.as_str()?;
    let same = |x: &&Value| {
        x.get("target").is_none()
            && x.get("tag").and_then(Value::as_str) == Some(tag)
            && x.get("label").and_then(Value::as_str) == Some(label)
    };
    let n = els[..i as usize].iter().filter(same).count();
    let count = els.iter().filter(same).count();
    Some(json!({ "tag": tag, "label": label, "key": e.get("key"), "n": n, "count": count }))
}

/// Resolve element `i` (in `scope`) inside the page, scroll it into view if
/// asked, and report where a click on it lands. One evaluation, unless the
/// element moved: then one scan finds where it went.
pub(crate) fn pick(conn: &mut cdp::Conn, scope: Option<&str>, i: u64, scroll: bool, select: Option<&Value>) -> Result<Value, String> {
    pick_inner(conn, scope, i, scroll, select, false)
}

/// The click path asks for this only when a real click would land on an overlay:
/// if that overlay is an advertisement, hide it and re-test so the click reaches
/// the revealed target, as it would after a person dismissed the ad. Dialogs,
/// anything holding a form or a password field, and anything unclassified are
/// left untouched and still refuse.
pub(crate) fn pick_clear_ads(conn: &mut cdp::Conn, scope: Option<&str>, i: u64) -> Result<Value, String> {
    pick_inner(conn, scope, i, true, None, true)
}

fn pick_once(conn: &mut cdp::Conn, scope: Option<&str>, i: u64, scroll: bool, select: Option<&Value>,
             clear_ads: bool, expect: Option<&Value>) -> Result<Value, String> {
    let mut p = json!({ "i": i, "scroll": scroll });
    if clear_ads {
        p["clearAds"] = json!(true);
    }
    if let Some(v) = select {
        p["select"] = v.clone();
    }
    if let Some(e) = expect {
        p["expect"] = e.clone();
    }
    let v = conn.eval(&scan_src(scope, Some(&p), i.saturating_add(1).min(2000), false))?;
    if v.get("missing").and_then(Value::as_bool) == Some(true) {
        let total = v.get("total").and_then(Value::as_u64).unwrap_or(0);
        return Err(format!(
            "no element with index {i} ({total} found{}); call elements again",
            scope.map(|s| format!(" within {s}")).unwrap_or_default()
        ));
    }
    Ok(v)
}

fn pick_inner(conn: &mut cdp::Conn, scope: Option<&str>, i: u64, scroll: bool, select: Option<&Value>, clear_ads: bool) -> Result<Value, String> {
    let expect = expected(&conn.tab, scope, i);
    let v = pick_once(conn, scope, i, scroll, select, clear_ads, expect.as_ref())?;
    if v.get("changed").and_then(Value::as_bool) != Some(true) {
        return Ok(v);
    }
    let e = expect.unwrap_or(Value::Null);
    let changed = || {
        let now = v.get("now").cloned().unwrap_or(Value::Null);
        format!(
            "element {i} is not what the last scan showed: it was {} {:?}, it is now {} {:?}. \
             The page changed under it; call elements again",
            e["tag"].as_str().unwrap_or("?"), e["label"].as_str().unwrap_or(""),
            now["tag"].as_str().unwrap_or("?"), now["label"].as_str().unwrap_or(""),
        )
    };
    // Something was inserted or removed above it. Find the same element in a
    // fresh scan: same tag and label, same occurrence, and only when the label
    // occurs as often as it did — otherwise which one was meant is a guess.
    let fresh = conn.eval(&scan_src(scope, None, 2000, false))?;
    let tag = e["tag"].as_str().unwrap_or("");
    let label = e["label"].as_str().unwrap_or("");
    let matches: Vec<u64> = fresh
        .get("elements").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
        .iter()
        .filter(|x| x.get("tag").and_then(Value::as_str) == Some(tag)
                 && x.get("label").and_then(Value::as_str) == Some(label))
        .filter_map(|x| x.get("i").and_then(Value::as_u64))
        .collect();
    let want = e["n"].as_u64().unwrap_or(0) as usize;
    if matches.len() as u64 != e["count"].as_u64().unwrap_or(0) || want >= matches.len() {
        return Err(changed());
    }
    let j = matches[want];
    let mut w = pick_once(conn, scope, j, scroll, select, clear_ads, Some(&e))?;
    if w.get("changed").and_then(Value::as_bool) == Some(true) {
        return Err(changed());
    }
    w["remapped"] = json!({ "from": i, "to": j });
    Ok(w)
}

/// Top-left corner and size from `DOM.getBoxModel`'s content quad: eight numbers
/// in top-viewport coordinates, four corners clockwise.
fn quad_box(quad: &[f64]) -> Option<(f64, f64, f64, f64)> {
    if quad.len() < 8 {
        return None;
    }
    let xs: Vec<f64> = quad.iter().step_by(2).copied().collect();
    let ys: Vec<f64> = quad.iter().skip(1).step_by(2).copied().collect();
    let x0 = xs.iter().cloned().fold(f64::INFINITY, f64::min);
    let y0 = ys.iter().cloned().fold(f64::INFINITY, f64::min);
    let x1 = xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let y1 = ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let (w, h) = (x1 - x0, y1 - y0);
    if w < 2.0 || h < 2.0 {
        None
    } else {
        Some((x0, y0, w, h))
    }
}

/// Where a cross-origin frame sits in the top page's viewport. The target id
/// from `Target.attachedToTarget` is accepted as the frame id, and the content
/// quad is viewport-space — measured at scrollY 0: y=1500, at scrollY 1000: y=500.
fn frame_box(conn: &mut cdp::Conn, target: &str) -> Option<(f64, f64, f64, f64)> {
    let owner = conn
        .call("DOM.getFrameOwner", json!({ "frameId": target }))
        .ok()?;
    let node = owner.get("backendNodeId").and_then(Value::as_u64)?;
    let model = conn
        .call("DOM.getBoxModel", json!({ "backendNodeId": node }))
        .ok()?;
    let quad: Vec<f64> = model
        .pointer("/model/content")?
        .as_array()?
        .iter()
        .filter_map(Value::as_f64)
        .collect();
    quad_box(&quad)
}

/// The host of a URL, cut to 40 characters: how a frame is named to the caller.
fn host_of(url: &str) -> String {
    url.split("://").nth(1).unwrap_or(url)
        .split('/').next().unwrap_or(url)
        .chars().take(40).collect()
}

/// Third-party ad and tracking frames. Their buttons are never what a task is
/// about, and each costs three round trips per scan.
fn ad_frame(url: &str) -> bool {
    const HOSTS: [&str; 9] = ["doubleclick", "googlesyndication", "adservice", "taboola", "outbrain",
                              "adsystem", "criteo", "pubmatic", "amazon-adsystem"];
    let host = host_of(url).to_lowercase();
    HOSTS.iter().any(|h| host.contains(h))
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
pub(crate) fn scan_with_frames(conn: &mut cdp::Conn, scope: Option<&str>, cap: u64) -> Result<Value, String> {
    let mut scan_v = conn.eval(&scan_js(scope, cap))?;
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
    let mut elements = match scan_v.get_mut("elements").map(Value::take) {
        Some(Value::Array(a)) => a,
        _ => Vec::new(),
    };
    let mut added = 0u64;
    let mut frame_capped = false;
    for (target, session, url) in frames {
        if elements.len() >= cap as usize {
            frame_capped = true;
            break;
        }
        if ad_frame(&url) {
            continue;
        }
        // Where the frame sits first: it is one cheap call, and a frame that
        // cannot be placed, or is too small to hold a control, is skipped
        // before its document is scanned. The frame's own target id is a frame
        // id to DOM.getFrameOwner, and its content quad is viewport-space.
        let (ox, oy, fw, fh) = match frame_box(conn, &target) {
            Some(b) if b.2 >= 16.0 && b.3 >= 16.0 => b,
            _ => continue,
        };
        // The frame returns a JSON string: one value in, one value out, and no
        // surprise about how deep the protocol will serialise.
        let body = match conn.eval_in(&session, &frame_src(&Value::Null)) {
            Ok(Value::String(body)) => serde_json::from_str::<Value>(&body).unwrap_or(Value::Null),
            Ok(v) => v,
            Err(_) => continue, // sandboxed, detached, or no document yet
        };
        if body.get("capped").and_then(Value::as_bool) == Some(true) {
            frame_capped = true;
        }
        let kids = match body.get("elements") {
            Some(Value::Array(a)) if !a.is_empty() => a.clone(),
            _ => continue,
        };
        let label = host_of(&url);
        for (fi, mut k) in kids.into_iter().enumerate() {
            if elements.len() >= cap as usize {
                frame_capped = true;
                break;
            }
            let f = |key: &str| k.get(key).and_then(Value::as_f64).unwrap_or(0.0);
            let (x, y, w, h) = (f("x"), f("y"), f("w"), f("h"));
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
                // Frame-local index: re-resolving by position is stable where a
                // cached coordinate is not.
                o.insert("fi".into(), json!(fi as u64));
                o.insert("lx".into(), json!(lx as i64));
                o.insert("ly".into(), json!(ly as i64));
                // A frame can be scrolled: only what the parent actually shows
                // is clickable, so anything outside the frame's box is off-screen.
                if !(lx >= 0.0 && ly >= 0.0 && lx <= fw && ly <= fh) {
                    o.insert("off".into(), json!(true));
                }
            }
            elements.push(k);
            added += 1;
        }
    }
    if let Some(o) = scan_v.as_object_mut() {
        o.insert("elements".into(), Value::Array(elements));
        if frame_capped {
            o.insert("capped".into(), json!(true));
        }
        if added > 0 {
            o.insert("framesScanned".into(), json!(added));
        }
    }
    Ok(scan_v)
}

/// The cached frame element at index `i`, if the last scan put one there.
fn cached_frame_element(tab: &str, i: u64) -> Option<Value> {
    let cache = scan_cache().lock().ok()?;
    cache.get(tab)?.get("elements")?.as_array()?
        .get(i as usize)
        .filter(|e| e.get("target").and_then(Value::as_str).is_some())
        .cloned()
}

/// Resolve a cached frame element's session by target id: a cached session id
/// goes stale when the page reconnects.
fn frame_session(conn: &mut cdp::Conn, i: u64, el: &Value) -> Result<String, String> {
    let want = el.get("target").and_then(Value::as_str).unwrap_or_default().to_string();
    if want.is_empty() {
        return Err(format!("element {i} is not inside a frame any more; call elements again"));
    }
    match conn.frames() {
        Ok(frames) => frames.iter().find(|f| f.0 == want).map(|f| f.1.clone()).ok_or_else(|| {
            format!("element {i} was inside a frame that is no longer there; call elements again")
        }),
        Err(e) => Err(format!("cannot resolve the frame for element {i} ({e}); call elements again")),
    }
}

/// Re-resolve element `i` inside its cross-origin frame, live: scroll it into
/// view, measure it again, and report hit-testing and form state. `None` means
/// `i` is not a frame element.
pub(crate) fn frame_pick(
    conn: &mut cdp::Conn,
    tab: &str,
    i: u64,
    scroll: bool,
    select: Option<&Value>,
) -> Result<Option<Value>, String> {
    let Some(el) = cached_frame_element(tab, i) else {
        return Ok(None);
    };
    let session = frame_session(conn, i, &el)?;
    let fi = el.get("fi").and_then(Value::as_u64).unwrap_or(0);
    let mut p = json!({ "i": fi, "scroll": scroll,
                        "expect": { "tag": el.get("tag"), "label": el.get("label"), "key": el.get("key") } });
    if let Some(v) = select {
        p["select"] = v.clone();
    }
    let body = match conn.eval_in(&session, &frame_src(&p)) {
        Ok(Value::String(body)) => body,
        Ok(v) => v.to_string(),
        Err(e) => return Err(format!("element {i} could not be resolved in its frame ({e}); call elements again")),
    };
    let mut v: Value = serde_json::from_str(&body)
        .map_err(|e| format!("the frame scan returned something unexpected: {e}"))?;
    if v.get("missing").and_then(Value::as_bool) == Some(true) {
        return Err(format!("element {i} is gone from its frame; call elements again"));
    }
    if v.get("changed").and_then(Value::as_bool) == Some(true) {
        return Err(format!(
            "element {i} is not what the last scan showed: it was {:?}, it is now {:?}; call elements again",
            el.get("label").and_then(Value::as_str).unwrap_or(""),
            v.pointer("/now/label").and_then(Value::as_str).unwrap_or(""),
        ));
    }
    v["session"] = json!(session);
    Ok(Some(v))
}

/// Resolve a frame click through `frame_pick`: the refusals come from the live
/// state, not from coordinates that may be stale, and the irreversible guard
/// runs on the live label.
/// Returns the label too, so the caller records the guard only after dispatch.
pub(crate) fn frame_target(
    conn: &mut cdp::Conn,
    tab: &str,
    scope: Option<&str>,
    i: u64,
    force: bool,
) -> Result<Option<(f64, f64, String, String)>, String> {
    // A scoped click indexes the scoped list, which holds no frame elements: a
    // CSS selector cannot reach into a child document.
    if scope.is_some() {
        return Ok(None);
    }
    let Some(el) = frame_pick(conn, tab, i, true, None)? else {
        return Ok(None);
    };
    let tag = el.get("tag").unwrap_or(&Value::Null);
    let label = el.get("label").unwrap_or(&Value::Null);
    if !force {
        let reason = if el.get("disabled").and_then(Value::as_bool) == Some(true) {
            Some("disabled")
        } else if el.get("zero").and_then(Value::as_bool) == Some(true) {
            Some("zero_size")
        // The frame's own viewport: what is below its fold needs a scroll
        // inside the frame, and a dispatch there would land outside it.
        } else if el.get("outside").and_then(Value::as_bool) == Some(true) {
            Some("outside_frame")
        } else if el.get("hit").and_then(Value::as_bool) == Some(false) {
            Some("obscured")
        } else {
            None
        };
        if let Some(reason) = reason {
            let obscured = if reason == "obscured" {
                json!({ "tag": el.get("blocker"), "label": el.get("blockerLabel") })
            } else {
                Value::Null
            };
            return Err(blocked_click(reason, i, tag, label, obscured, true));
        }
    }
    let text = label.as_str().unwrap_or("").to_string();
    if !text.is_empty() && irreversible(&text) {
        check_irreversible(tab, &text, i, force)?;
    }
    let x = el.get("lx").and_then(Value::as_f64).unwrap_or(0.0);
    let y = el.get("ly").and_then(Value::as_f64).unwrap_or(0.0);
    let session = el.get("session").and_then(Value::as_str).unwrap_or_default().to_string();
    Ok(Some((x, y, session, text)))
}

/// The same three events the page path sends, dispatched inside a frame session.
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
/// almost nothing at all. Only an empty list is suspect: a small page with one
/// or two controls is real, and calling it "still rendering" sent agents into
/// wait-and-rescan loops.
pub(crate) fn suspect_render(total: u64, nodes: u64) -> bool {
    total == 0 && nodes < 400
}

/// Keys `scan_with_frames` and the scanner keep for the click path. They are
/// cache state, not answers: `session` and `target` are 32-char ids, `fi`/
/// `lx`/`ly` are the frame-local index and point, `key` is what lets a click
/// recognise an element whose text changed.
const INTERNAL_KEYS: [&str; 6] = ["session", "target", "fi", "lx", "ly", "key"];

/// One element as JSON for `full: true`: the record minus the cache-only keys.
fn public_element(e: &Value) -> Value {
    let mut keep = e.clone();
    if let Some(o) = keep.as_object_mut() {
        for k in INTERNAL_KEYS {
            o.remove(k);
        }
    }
    keep
}

fn s<'a>(e: &'a Value, k: &str) -> &'a str {
    e.get(k).and_then(Value::as_str).unwrap_or("")
}

/// One element as one line: `i role label · flags`. Every action takes `i`,
/// so coordinates are left to `full: true`; what stays is what decides which
/// element to use. Main content carries no region flag.
pub(crate) fn element_line(e: &Value, dupes: usize) -> String {
    let role = match s(e, "role") { "" => s(e, "tag"), r => r };
    let label = match s(e, "label") { "" => "(no label)", l => l };
    let mut line = format!("{} {role} {label}", e.get("i").and_then(Value::as_u64).unwrap_or(0));
    let mut flags: Vec<String> = Vec::new();
    let on = |k: &str| e.get(k).and_then(Value::as_bool) == Some(true);
    let off = |k: &str| e.get(k).and_then(Value::as_bool) == Some(false);
    if !s(e, "in").is_empty() {
        flags.push(s(e, "in").to_string());
    }
    if !s(e, "frame").is_empty() {
        flags.push(format!("frame={}", s(e, "frame")));
    }
    if on("off") {
        flags.push("off".into());
    }
    if on("disabled") {
        flags.push("disabled".into());
    }
    match e.get("checked") {
        Some(Value::Bool(true)) => flags.push("checked".into()),
        Some(Value::Bool(false)) => flags.push("unchecked".into()),
        Some(Value::String(m)) => flags.push(m.clone()),
        _ => {}
    }
    if on("expanded") {
        flags.push("expanded".into());
    } else if off("expanded") {
        flags.push("collapsed".into());
    }
    if !s(e, "popup").is_empty() {
        flags.push(format!("popup={}", s(e, "popup")));
    }
    if on("pressed") {
        flags.push("pressed".into());
    } else if off("pressed") {
        flags.push("unpressed".into());
    }
    for k in ["current", "required", "invalid"] {
        if on(k) {
            flags.push(k.into());
        }
    }
    match e.get("selected") {
        Some(Value::String(v)) if !v.is_empty() => flags.push(format!("selected={v}")),
        Some(Value::Array(a)) if !a.is_empty() => flags.push(format!(
            "selected={}",
            a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(",")
        )),
        _ => {}
    }
    if dupes > 1 {
        flags.push(format!("×{dupes}"));
    }
    // Free text goes last, so it cannot be mistaken for a flag.
    if !s(e, "value").is_empty() {
        flags.push(format!("value={}", s(e, "value")));
    }
    if !flags.is_empty() {
        line.push_str(" · ");
        line.push_str(&flags.join(" "));
    }
    line
}

fn render(e: &Value, full: bool, dupes: usize) -> Value {
    if full {
        let mut v = public_element(e);
        if dupes > 1 {
            v["dupes"] = json!(dupes);
        }
        v
    } else {
        Value::String(element_line(e, dupes))
    }
}

/// An overlay as one line: what it is, what it says, and the indexes that close it.
fn overlay_line(o: &Value) -> Value {
    let mut line = format!("{} {}", s(o, "kind"), clip_str(s(o, "label"), 80));
    if let Some(cs) = o.get("closers").and_then(Value::as_array) {
        let parts: Vec<String> = cs.iter().map(|c| match c.get("i").and_then(Value::as_u64) {
            Some(i) => format!("{i} {}", s(c, "label")),
            None => s(c, "label").to_string(),
        }).collect();
        line.push_str(" · closers: ");
        line.push_str(&parts.join(" | "));
    }
    Value::String(line)
}

/// A hidden group as one line: what it is, what is in it, and what opens it.
fn collapsed_line(g: &Value, els: &[Value]) -> Value {
    let sample: Vec<&str> = g.get("sample").and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let items = g.get("items").and_then(Value::as_u64).unwrap_or(0);
    let more = if items as usize > sample.len() { ", …" } else { "" };
    let opener = match g.get("toggle").and_then(Value::as_u64).and_then(|t| els.get(t as usize)) {
        Some(t) => {
            let role = match s(t, "role") { "" => s(t, "tag"), r => r };
            let how = if g.get("guess").and_then(Value::as_bool) == Some(true) { "maybe opened by" } else { "opened by" };
            format!("{how} {} {role} {}", t.get("i").and_then(Value::as_u64).unwrap_or(0), s(t, "label"))
        }
        None => "no opener found".into(),
    };
    Value::String(format!("{} of {items} ({}{more}) — {opener}", s(g, "kind"), sample.join(" | ")))
}

/// Delta identity: tag, label and which occurrence of that pair it is. Not the
/// position: a 40 px scroll moved every coordinate and made a whole page read
/// as removed and re-added.
fn fingerprints(list: &[&Value]) -> Vec<String> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    list.iter().map(|e| {
        let base = format!("{}|{}", s(e, "tag"), s(e, "label"));
        let n = seen.entry(base.clone()).or_insert(0);
        *n += 1;
        format!("{base}|{n}")
    }).collect()
}

/// Insert only what carries information: a missing key reads as "no", and
/// every `null`, `false` and `[]` that used to ride along cost bytes on every
/// call without telling the caller anything.
fn put(o: &mut Map<String, Value>, k: &str, v: Value) {
    let empty = match &v {
        Value::Null | Value::Bool(false) => true,
        Value::Array(a) => a.is_empty(),
        Value::Number(n) => n.as_u64() == Some(0),
        Value::String(s) => s.is_empty(),
        _ => false,
    };
    if !empty {
        o.insert(k.into(), v);
    }
}

pub(crate) fn elements(cfg: &Cfg, args: &Value) -> Result<Value, String> {
    let tab = args.get("tab").and_then(Value::as_str);
    let find = args.get("find").and_then(Value::as_str).map(|s| s.to_lowercase());
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(120) as usize;
    // The scan collects at most `cap` elements; `limit` above 600 lifts it.
    let cap = if limit > 600 { limit.min(2000) as u64 } else { 600 };
    let full = args.get("full").and_then(Value::as_bool).unwrap_or(false);
    let mode = args.get("mode").and_then(Value::as_str).unwrap_or("full").to_string();
    // Repeated labels collapse by default; a search lists every match.
    let unique = args.get("unique").and_then(Value::as_bool).unwrap_or(find.is_none());
    let role_filter = args.get("role").and_then(Value::as_str).map(|s| s.to_lowercase());
    let shot_mode = args.get("shot").and_then(Value::as_str).unwrap_or("none").to_string();
    if mode == "delta" && shot_mode != "none" {
        // The delta branch returns before the capture tail; it used to drop the
        // request silently.
        return Err("elements: mode \"delta\" cannot attach a capture; use mode viewport or full".into());
    }
    let shot_fmt = args.get("shotFormat").and_then(Value::as_str).unwrap_or("jpeg").to_string();
    let max_w = args.get("shotMaxWidth").and_then(Value::as_f64).unwrap_or(1024.0);
    let region = parse_region(args.get("region"))?;

    let within = match args.get("within") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => nonempty_scope(s),
        Some(_) => return Err("within must be a CSS selector; integer within is not implemented yet".into()),
    };
    with_conn(&cfg.endpoint, tab, |conn, tab_id| {
        let mut scan_v = scan_with_frames(conn, within.as_deref(), cap)?;
        // Kept with the cache entry, so a delta knows whether its baseline came
        // from the same scope as this scan, and a click whether its index did.
        scan_v["scope"] = json!(&within);
        scope_cache().lock().unwrap().insert(tab_id.to_string(), within.clone());
        let previous = scan_cache().lock().unwrap().remove(tab_id).unwrap_or(Value::Null);
        let payload = build(&scan_v, &previous, Shape {
            tab: tab_id, within: within.as_deref(), mode: &mode, full, unique, limit, cap,
            find: find.as_deref(), role: role_filter.as_deref(),
        });
        scan_cache().lock().unwrap().insert(tab_id.to_string(), scan_v);

        if shot_mode == "none" || mode == "delta" {
            return Ok(payload);
        }
        let (bytes, clipped, mime) =
            capture_guarded(conn, tab_id, &shot_mode, region, Some(max_w), &shot_fmt, 82)?;
        Ok(json!({ "__content": [
            { "type": "text", "text": payload.to_string() },
            { "type": "image", "data": b64encode(&bytes), "mimeType": mime },
            { "type": "text", "text": format!("(attached {shot_mode} capture, {} bytes inline as base64{}; shot {{path}} answers ~250 B instead)", bytes.len(), if clipped { ", clipped to 8000px" } else { "" }) }
        ]}))
    })
}

/// How the caller asked for the answer.
struct Shape<'a> {
    tab: &'a str,
    within: Option<&'a str>,
    mode: &'a str,
    full: bool,
    unique: bool,
    limit: usize,
    cap: u64,
    find: Option<&'a str>,
    role: Option<&'a str>,
}

/// The `elements` answer, from a scan and the scan before it. Pure, so the
/// output shape is tested without a browser.
fn build(scan_v: &Value, previous: &Value, a: Shape) -> Value {
    let raw: &[Value] = scan_v.get("elements").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let empty = Vec::new();
    let arr = |v: &Value, k: &str| -> Vec<Value> { v.get(k).and_then(Value::as_array).unwrap_or(&empty).clone() };
    let scope_missing = scan_v.get("scopeMissing").and_then(Value::as_bool).unwrap_or(false);
    let overlays: Vec<Value> = if a.full { arr(scan_v, "overlays") } else { arr(scan_v, "overlays").iter().map(overlay_line).collect() };
    let mut o = Map::new();
    o.insert("tab".into(), json!(a.tab));

    if a.mode == "delta" {
        // A baseline taken under another scope is not a baseline: every
        // element would read as added and every old one as removed.
        let scope_changed = !previous.is_null() && previous.get("scope").and_then(Value::as_str) != a.within;
        let prev: &[Value] = if scope_changed { &[] } else {
            previous.get("elements").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
        };
        let now: Vec<&Value> = raw.iter().collect();
        let before: Vec<&Value> = prev.iter().collect();
        let (fp_now, fp_before) = (fingerprints(&now), fingerprints(&before));
        let old: HashSet<&String> = fp_before.iter().collect();
        let new: HashSet<&String> = fp_now.iter().collect();
        let added: Vec<&Value> = now.iter().zip(&fp_now).filter(|(_, f)| !old.contains(f)).map(|(e, _)| *e).collect();
        let removed: Vec<&Value> = before.iter().zip(&fp_before).filter(|(_, f)| !new.contains(f)).map(|(e, _)| *e).collect();
        o.insert("mode".into(), json!("delta"));
        put(&mut o, "scope", json!(a.within));
        put(&mut o, "scopeChanged", json!(scope_changed));
        put(&mut o, "scopeMissing", json!(scope_missing));
        o.insert("baseline".into(), json!(!previous.is_null() && !scope_changed));
        // A route swap can replace the whole page: the counts stay exact, but
        // the lists obey the same `limit` as a normal scan.
        o.insert("added".into(), Value::Array(added.iter().take(a.limit).map(|e| render(e, a.full, 1)).collect()));
        o.insert("addedCount".into(), json!(added.len()));
        o.insert("removed".into(), Value::Array(removed.iter().take(a.limit).map(|e| render(e, a.full, 1)).collect()));
        o.insert("removedCount".into(), json!(removed.len()));
        o.insert("unchanged".into(), json!(now.len().saturating_sub(added.len())));
        put(&mut o, "overlays", Value::Array(overlays));
        let count = |v: &Value| v.get("overlays").and_then(Value::as_array).map_or(0, Vec::len) as i64;
        put(&mut o, "overlayDelta", json!(count(scan_v) - count(previous)));
        put(&mut o, "media", json!(arr(scan_v, "media")));
        return Value::Object(o);
    }

    let all: Vec<&Value> = raw.iter()
        .filter(|e| a.mode != "viewport" || e.get("off").and_then(Value::as_bool) != Some(true))
        .collect();
    let total = all.len();
    let matched: Vec<&Value> = all.iter().copied().filter(|e| {
        let label = s(e, "label").to_lowercase();
        a.find.is_none_or(|f| label.contains(f))
            && a.role.is_none_or(|r| s(e, "role").eq_ignore_ascii_case(r) || s(e, "tag").eq_ignore_ascii_case(r))
    }).collect();
    // Per region: ten "Edit" links in the article and one in the nav bar are
    // two different things to click.
    let group = |e: &Value| format!("{}|{}|{}", match s(e, "role") { "" => s(e, "tag"), r => r }, s(e, "in"), s(e, "label").to_lowercase());
    let mut counts: HashMap<String, usize> = HashMap::new();
    if a.unique {
        for e in &matched {
            if !s(e, "label").is_empty() {
                *counts.entry(group(e)).or_insert(0) += 1;
            }
        }
    }
    let mut emitted: HashSet<String> = HashSet::new();
    let mut shown: Vec<Value> = Vec::new();
    for e in &matched {
        let mut dupes = 1;
        if a.unique && !s(e, "label").is_empty() {
            let g = group(e);
            if !emitted.insert(g.clone()) {
                continue;
            }
            dupes = counts.get(&g).copied().unwrap_or(1);
        }
        shown.push(render(e, a.full, dupes));
        if shown.len() >= a.limit {
            break;
        }
    }

    let url = s(scan_v, "url");
    let url = if url.len() > 300 { format!("{}…", clip_str(url, 300)) } else { url.to_string() };
    let capped = scan_v.get("capped").and_then(Value::as_bool).unwrap_or(false);
    let collapsed: Vec<Value> = arr(scan_v, "collapsed").iter()
        .map(|g| if a.full { g.clone() } else { collapsed_line(g, raw) }).collect();
    o.insert("url".into(), json!(url));
    put(&mut o, "title", scan_v.get("title").cloned().unwrap_or(Value::Null));
    // Which scope produced this list, and whether it matched: a miss must not
    // read as an empty scope.
    put(&mut o, "scope", json!(a.within));
    put(&mut o, "scopeMissing", json!(scope_missing));
    put(&mut o, "overlays", Value::Array(overlays));
    put(&mut o, "collapsed", Value::Array(collapsed));
    put(&mut o, "media", json!(arr(scan_v, "media")));
    put(&mut o, "scroll", scan_v.get("scroll").cloned().unwrap_or(Value::Null));
    o.insert("total".into(), json!(total));
    if a.find.is_some() || a.role.is_some() {
        o.insert("matched".into(), json!(matched.len()));
    }
    o.insert("shown".into(), json!(shown.len()));
    if capped {
        // Matching elements were dropped. Saying so is cheap; making the
        // caller guess at `limit` is not.
        o.insert("capped".into(), json!(true));
        o.insert("cap".into(), json!(a.cap));
        o.insert("hint".into(), json!(format!("capped at {}: narrow with find:/within:, or raise limit (max 2000)", a.cap)));
    }
    // How many of these came from inside a cross-origin frame.
    put(&mut o, "framesScanned", scan_v.get("framesScanned").cloned().unwrap_or(Value::Null));
    // A near-empty scan on a document with almost nothing in it is usually a
    // page that has not painted yet, not an empty page. Never for a scope that
    // missed: that answer is already exact.
    // Judged on the whole scan, not the viewport slice: nothing on screen
    // after a scroll is an answer, not a page still loading.
    if !scope_missing && suspect_render(raw.len() as u64, scan_v.get("nodes").and_then(Value::as_u64).unwrap_or(0)) {
        o.insert("suspect".into(), json!("page may still be rendering: few nodes, no elements"));
    }
    o.insert("elements".into(), Value::Array(shown));
    Value::Object(o)
}

pub(crate) fn text(cfg: &Cfg, args: &Value) -> Result<Value, String> {
    let tab = args.get("tab").and_then(Value::as_str);
    // What the page says, as text. `elements` answers "what can I click"; this
    // answers "what does it say". With no `within`, the page's <main> is read
    // when it holds real content — the navigation, header and footer around
    // it are what used to fill the character budget — plus any open dialog,
    // which sits outside <main> and is the first thing a person reads.
    let within = match args.get("within") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => nonempty_scope(s),
        Some(_) => return Err("within must be a CSS selector".into()),
    };
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(4000) as usize;
    let frames = args.get("frames").and_then(Value::as_bool).unwrap_or(false);
    with_conn(&cfg.endpoint, tab, |conn, tab_id| {
        let js = format!(
            r#"(() => {{
              const SEL = {sel}, LIM = {lim};
              const tidy = (s) => String(s || '').replace(/\n{{3,}}/g, '\n\n').trim();
              let r = null, name = SEL;
              if (SEL === null) {{
                const m = document.querySelector('main, [role="main"]');
                r = m && tidy(m.innerText).length >= 200 ? m : document.body;
                name = r === document.body ? 'body' : (r.tagName === 'MAIN' ? 'main' : '[role=main]');
              }} else {{
                try {{ r = document.querySelector(SEL); }} catch (e) {{ return {{ err: String(e) }}; }}
              }}
              if (!r) return {{ missing: true, name }};
              let t = tidy(r.innerText);
              if (SEL === null && r !== document.body) {{
                for (const d of document.querySelectorAll('dialog[open], [role="dialog"], [role="alertdialog"], [aria-modal="true"]')) {{
                  if (r.contains(d) || (d.checkVisibility && !d.checkVisibility({{ checkOpacity: true, checkVisibilityCSS: true }}))) continue;
                  const x = tidy(d.innerText);
                  if (x) t = '[dialog]\n' + x.slice(0, 1500) + '\n\n' + t;
                }}
              }}
              return {{ text: t.slice(0, LIM), truncated: t.length > LIM, name }};
            }})()"#,
            sel = within.as_ref().map_or("null".to_string(), |w| serde_json::to_string(w).unwrap_or_else(|_| "null".into())),
            lim = limit
        );
        let v = conn.eval(&js)?;
        if let Some(e) = v.get("err").and_then(Value::as_str) {
            return Err(format!("text: {e}"));
        }
        // A selector that matches nothing is not the body: saying so beats
        // handing back the whole page as if it were the scope.
        let scope_missing = v.get("missing").and_then(Value::as_bool) == Some(true);
        let mut text = s(&v, "text").to_string();
        // Measured in the page: JS counts UTF-16 code units and the cut is by
        // those, so asking Rust to compare `chars` against `limit` was wrong for
        // an exact-length paragraph and for anything with an emoji in it.
        let doc_truncated = v.get("truncated").and_then(Value::as_bool).unwrap_or(false);
        let mut more_frames = false;
        // Cross-origin frames are separate documents; their words are only
        // reachable through their own session. Off unless asked: it costs a frame
        // list and one evaluation per visible frame.
        if frames && !scope_missing {
            for (target, session, url) in conn.frames().unwrap_or_default() {
                if ad_frame(&url) || frame_box(conn, &target).is_none() {
                    continue; // not on screen: its text is not part of the page you see
                }
                let js = "JSON.stringify((document.body && document.body.innerText || '') \
                          .replace(/\\n{3,}/g, '\\n\\n').trim().slice(0, 2000))";
                let Ok(Value::String(inner)) = conn.eval_in(&session, js) else { continue };
                let Ok(inner) = serde_json::from_str::<String>(&inner) else { continue };
                let inner = inner.trim();
                if inner.is_empty() {
                    continue;
                }
                text.push_str(&format!("\n\n[frame {}]\n{inner}", host_of(&url)));
                if text.chars().count() >= limit {
                    more_frames = true;
                    break;
                }
            }
        }
        let n = text.chars().count();
        let truncated = doc_truncated || more_frames || n > limit;
        let text: String = text.chars().take(limit).collect();
        let mut o = Map::new();
        o.insert("tab".into(), json!(tab_id));
        o.insert("within".into(), json!(s(&v, "name")));
        put(&mut o, "scopeMissing", json!(scope_missing));
        o.insert("chars".into(), json!(text.chars().count()));
        put(&mut o, "truncated", json!(truncated));
        put(&mut o, "frames", json!(frames));
        o.insert("text".into(), json!(text));
        Ok(Value::Object(o))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_are_always_filled() {
        // An unfilled placeholder is a ReferenceError in the page.
        for js in [scan_js(None, 600), scan_js(Some("main"), 600), dismiss_scan_js(),
                   frame_src(&Value::Null), frame_src(&json!({ "i": 3 }))] {
            assert!(!js.contains("__COMMON__") && !js.contains("__OPTS__") && !js.contains("__FRAME_PICK__"));
            assert!(js.contains("const labelOf"));
        }
        assert!(scan_js(Some("main"), 600).contains(r#""scope":"main""#));
        assert!(scan_js(None, 2000).contains(r#""cap":2000"#));
        assert!(dismiss_scan_js().contains(r#""extras":false"#));
        // A scope that spells a placeholder is data, not a slot.
        assert!(scan_js(Some("__COMMON__"), 600).contains(r#""scope":"__COMMON__""#));
    }

    #[test]
    fn page_and_frame_scans_share_one_definition() {
        // The frame used to carry its own copy of the selector, the label rule
        // and the form state, and the two drifted.
        for tpl in [SCAN_TPL, FRAME_TPL] {
            assert!(tpl.contains("__COMMON__"));
            assert!(!tpl.contains("const labelOf"), "a scanner grew its own labelOf again");
        }
        assert!(FRAME_TPL.contains("nodes.length >= 300"));
    }

    #[test]
    fn a_box_model_quad_becomes_an_origin_and_a_size() {
        // Content quad at scroll 0: y 1500, size 600x400.
        assert_eq!(
            quad_box(&[0.0, 1500.0, 600.0, 1500.0, 600.0, 1900.0, 0.0, 1900.0]),
            Some((0.0, 1500.0, 600.0, 400.0))
        );
        // At scroll 1000 the same frame is at y 500: the quad is viewport-space.
        assert_eq!(
            quad_box(&[0.0, 500.0, 600.0, 500.0, 600.0, 900.0, 0.0, 900.0]),
            Some((0.0, 500.0, 600.0, 400.0))
        );
        assert_eq!(quad_box(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0]), None);
        assert_eq!(quad_box(&[1.0, 2.0]), None);
    }

    #[test]
    fn a_bare_document_is_suspected_but_a_loaded_one_is_not() {
        assert!(suspect_render(0, 2));
        assert!(suspect_render(0, 399));
        assert!(!suspect_render(0, 812));
        assert!(!suspect_render(2, 900));
        assert!(!suspect_render(5, 100));
        assert!(!suspect_render(2, 100), "a two-button page is a page");
    }

    #[test]
    fn an_element_is_one_line_with_only_the_flags_it_has() {
        let e = json!({ "i": 0, "tag": "button", "role": "button", "label": "icon:menu", "in": "header",
                        "expanded": false, "popup": "menu", "x": 10, "y": 20, "key": "burger" });
        assert_eq!(element_line(&e, 1), "0 button icon:menu · header collapsed popup=menu");
        let f = json!({ "i": 12, "tag": "input", "role": "textbox", "label": "Email", "required": true,
                        "value": "a · b" });
        assert_eq!(element_line(&f, 1), "12 textbox Email · required value=a · b");
        assert_eq!(element_line(&json!({ "i": 3, "tag": "a", "role": "link", "label": "Reply" }), 7),
                   "3 link Reply · ×7");
        assert_eq!(element_line(&json!({ "i": 4, "tag": "summary", "label": "" }), 1), "4 summary (no label)");
        assert_eq!(element_line(&json!({ "i": 5, "tag": "select", "role": "combobox", "label": "Tags", "selected": [] }), 1),
                   "5 combobox Tags", "an empty selection is no flag");
    }

    #[test]
    fn delta_identity_ignores_position() {
        let a = [json!({ "tag": "a", "label": "Pricing", "x": 1, "y": 100 }),
                 json!({ "tag": "a", "label": "Pricing", "x": 1, "y": 300 })];
        let b = [json!({ "tag": "a", "label": "Pricing", "x": 1, "y": 60 }),
                 json!({ "tag": "a", "label": "Pricing", "x": 1, "y": 260 })];
        let fa = fingerprints(&a.iter().collect::<Vec<_>>());
        let fb = fingerprints(&b.iter().collect::<Vec<_>>());
        assert_eq!(fa, fb, "a scroll is not a change");
        assert_ne!(fa[0], fa[1], "two equal labels are still two elements");
    }

    fn scan(elements: Value) -> Value {
        json!({ "url": "https://x.test/", "title": "t", "scopeMissing": false, "media": [],
                "scroll": { "y": 0, "h": 900, "vh": 800 }, "nodes": 900, "overlays": [],
                "collapsed": [], "capped": false, "elements": elements })
    }

    fn shape<'a>(full: bool, unique: bool, find: Option<&'a str>) -> Shape<'a> {
        Shape { tab: "t1", within: None, mode: "full", full, unique, limit: 120, cap: 600, find, role: None }
    }

    #[test]
    fn the_answer_carries_no_empty_fields() {
        let v = build(&scan(json!([{ "i": 0, "tag": "a", "role": "link", "label": "Home", "x": 5, "y": 5 }])),
                      &Value::Null, shape(false, true, None));
        for gone in ["scope", "scopeMissing", "overlays", "collapsed", "media", "capped", "cap", "hint",
                     "framesScanned", "suspect", "truncated", "nodes"] {
            assert!(v.get(gone).is_none(), "{gone} should be absent: {v}");
        }
        assert_eq!(v["elements"], json!(["0 link Home"]));
    }

    #[test]
    fn repeated_labels_collapse_unless_searched() {
        let els = json!([
            { "i": 0, "tag": "button", "role": "button", "label": "Reply" },
            { "i": 1, "tag": "a", "role": "link", "label": "Alice" },
            { "i": 2, "tag": "button", "role": "button", "label": "Reply" },
            { "i": 3, "tag": "button", "role": "button", "label": "Reply" },
        ]);
        let v = build(&scan(els.clone()), &Value::Null, shape(false, true, None));
        assert_eq!(v["elements"], json!(["0 button Reply · ×3", "1 link Alice"]));
        let f = build(&scan(els), &Value::Null, shape(false, false, Some("reply")));
        assert_eq!(f["matched"], json!(3));
        assert_eq!(f["elements"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn full_records_keep_coordinates_but_not_cache_keys() {
        let v = build(&scan(json!([{ "i": 0, "tag": "a", "label": "Home", "x": 5, "y": 6, "w": 40, "h": 20,
                                     "key": "/home", "session": "S", "target": "T", "fi": 0, "lx": 1, "ly": 1 }])),
                      &Value::Null, shape(true, true, None));
        let e = &v["elements"][0];
        assert_eq!((e["x"].as_i64(), e["w"].as_i64()), (Some(5), Some(40)));
        for k in INTERNAL_KEYS {
            assert!(e.get(k).is_none(), "{k} leaked");
        }
    }

    #[test]
    fn a_hidden_menu_names_its_opener() {
        let mut sv = scan(json!([{ "i": 0, "tag": "button", "role": "button", "label": "icon:menu" }]));
        sv["collapsed"] = json!([{ "kind": "drawer", "items": 4, "sample": ["Home", "Settings"], "toggle": 0 },
                                 { "kind": "nav", "items": 2, "sample": ["A", "B"] }]);
        let v = build(&sv, &Value::Null, shape(false, true, None));
        assert_eq!(v["collapsed"], json!([
            "drawer of 4 (Home | Settings, …) — opened by 0 button icon:menu",
            "nav of 2 (A | B) — no opener found",
        ]));
    }

    #[test]
    fn ad_frames_are_recognised_by_host() {
        assert!(ad_frame("https://tpc.googlesyndication.com/safeframe/1-0-40/html/container.html"));
        assert!(!ad_frame("https://checkout.stripe.com/pay"));
        assert_eq!(host_of("https://a.example.com/x/y"), "a.example.com");
    }
}
