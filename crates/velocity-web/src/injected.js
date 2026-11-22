// Velocity's in-page library. Installed once per document, in Velocity's
// own isolated world (not the page's), and called through
// `Runtime.callFunctionOn` with its arguments as JSON values: nothing from a
// selector is ever pasted into script text, and the page can neither see
// this state nor break it by patching DOM prototypes (Playwright's injected
// script works the same way). See `cdp::Tab::call`.
(function () {
  'use strict';
  if (globalThis.__velocity) return;

  // ── Identity ──────────────────────────────────────────────────────────
  // Ids live in a WeakMap, outside the DOM. A `data-vel-id` attribute was
  // copied by cloneNode/innerHTML (and pages could set it), so a tap could
  // land on a clone of the element it resolved (VEL-118). The id -> node map
  // holds WeakRefs, and a FinalizationRegistry drops collected ids.
  var ids = new WeakMap(), nodes = new Map(), seq = 0;
  var fin = typeof FinalizationRegistry === 'function'
    ? new FinalizationRegistry(function (i) { nodes.delete(i); }) : null;
  function vid(n) {
    var i = ids.get(n);
    if (!i) {
      i = (++seq).toString(36) + '.' + Math.random().toString(36).slice(2, 8);
      ids.set(n, i);
      nodes.set(i, new WeakRef(n));
      if (fin) fin.register(n, i);
    }
    return i;
  }
  function byVid(i) {
    var r = nodes.get(i), n = r && r.deref();
    if (!n) { nodes.delete(i); return null; }
    return n.isConnected ? n : null;
  }

  // ── Text ──────────────────────────────────────────────────────────────
  // One definition of an element's text, used to capture it, to match
  // `text:` and to read it for assertText (VEL-130/273/267/128):
  // - a field is its value (a select its selected options' labels), as on
  //   Android and iOS; a password or a checkbox's "on" is never read out;
  // - anything else is its rendered text (innerText): no svg <title>, no
  //   <script>/<style>, no source indentation;
  // - normalised: NFC, zero-width characters dropped, whitespace runs
  //   collapsed to one space, trimmed.
  var NO_VALUE = { password: 1, checkbox: 1, radio: 1, file: 1, hidden: 1, image: 1, range: 1, color: 1 };
  var ZERO_WIDTH = /[​-‍⁠﻿]/g;
  function norm(s) {
    s = s == null ? '' : String(s);
    if (s.normalize) s = s.normalize('NFC');
    return s.replace(ZERO_WIDTH, '').replace(/\s+/g, ' ').trim();
  }
  function textOf(n) {
    var tag = n.tagName;
    if (tag === 'INPUT') return NO_VALUE[(n.type || 'text').toLowerCase()] ? '' : norm(n.value);
    if (tag === 'TEXTAREA') return norm(n.value);
    if (tag === 'SELECT') {
      var o = [];
      for (var i = 0; i < n.selectedOptions.length; i++) o.push(n.selectedOptions[i].label);
      return norm(o.join(', '));
    }
    if (holdsAriaHidden(n)) return norm(shownText(n));
    var t = n.innerText;
    return norm(typeof t === 'string' ? t : n.textContent);
  }
  // `aria-hidden="true"` content is decoration a screen reader skips (an
  // icon-font ligature: `<i aria-hidden>delete</i> Delete`), and so does
  // the accessible name; innerText kept it, so the button read
  // "delete Delete" (review 10 #3). The elements holding such content are
  // found once per evaluation, so a text search stays linear.
  function holdsAriaHidden(n) {
    var st = state();
    if (!st.ariaHolders) {
      var set = new Set(), hidden = qall('[aria-hidden="true"]');
      for (var i = 0; i < hidden.length; i++) {
        for (var p = parentOf(hidden[i]); p && !set.has(p); p = parentOf(p)) set.add(p);
      }
      st.ariaHolders = set;
    }
    return st.ariaHolders.has(n);
  }
  // innerText without the aria-hidden subtrees: a block child's text is
  // set apart by a space (innerText's line break, which `norm` makes a
  // space), an inline one's runs on.
  function shownText(n) {
    var out = '';
    for (var c = n.firstChild; c; c = c.nextSibling) {
      if (c.nodeType === 3) { out += c.data; continue; }
      if (c.nodeType !== 1 || c.getAttribute('aria-hidden') === 'true') continue;
      var tag = c.tagName;
      if (tag === 'SCRIPT' || tag === 'STYLE' || tag === 'TEMPLATE' || tag === 'NOSCRIPT') continue;
      if (tag === 'BR') { out += ' '; continue; }
      var cs = styleOf(c);
      if (cs.display === 'none') continue;
      var inner;
      if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') inner = '';
      else if (holdsAriaHidden(c)) inner = shownText(c);
      else if (cs.visibility === 'hidden') inner = '';
      else inner = typeof c.innerText === 'string' ? c.innerText : c.textContent;
      out += /^inline/.test(cs.display) || cs.display === 'contents' ? inner : ' ' + inner + ' ';
    }
    return out;
  }
  // The other reading of a non-field element's text: its source text
  // (textContent), normalised the same way, or null for a field. innerText
  // applies CSS `text-transform`, so a MUI / Bootstrap button styled
  // uppercase reads "SUBMIT" while the page's source (and every suite
  // written against it) says "Submit": a match on either form counts.
  function sourceTextOf(n) {
    var tag = n.tagName;
    if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') return null;
    return norm(n.textContent);
  }
  // Whether `n`'s text equals (or, `contains`, holds) the normalised `want`,
  // as rendered or as written. The source form is only read when the
  // rendered one doesn't match, so a search stays one innerText per node.
  // `fold`: compare ignoring case (`want` is then lower-cased already).
  function textIs(n, want, contains, fold) {
    var hit = function (s) {
      if (fold) s = s.toLowerCase();
      return contains ? s.indexOf(want) !== -1 : s === want;
    };
    if (hit(textOf(n))) return true;
    var src = sourceTextOf(n);
    return src !== null && hit(src);
  }

  // ── Geometry and state ───────────────────────────────────────────────
  function styleOf(n) { return n.ownerDocument.defaultView.getComputedStyle(n); }
  // Offset of n's frame within the top page (same-origin frames).
  function frameOffset(n) {
    var ox = 0, oy = 0, w = n.ownerDocument.defaultView;
    while (w && w.frameElement) {
      var f = w.frameElement.getBoundingClientRect();
      ox += f.left + w.frameElement.clientLeft; oy += f.top + w.frameElement.clientTop;
      w = w.parent;
    }
    return [ox, oy];
  }
  function isVisible(n) {
    if (n.checkVisibility) return n.checkVisibility({ opacityProperty: true, visibilityProperty: true });
    var s = styleOf(n);
    return s.display !== 'none' && s.visibility !== 'hidden' && parseFloat(s.opacity) > 0;
  }
  function hasBox(n) { var r = n.getBoundingClientRect(); return r.width > 0 && r.height > 0; }
  // D-1: the share (0..1) of n's box that is on screen: inside the top
  // viewport and inside every ancestor that clips its overflow (a carousel
  // slide, a scrolled list), across same-origin frames. An ancestor clips
  // only what it contains: a fixed element escapes them all, an absolute
  // one every unpositioned ancestor (Cypress's rule). <html> and <body>
  // pass their overflow to the viewport, which is clipped anyway.
  function shownRatio(n) {
    var r = n.getBoundingClientRect(), area = r.width * r.height;
    if (!(area > 0)) return 0;
    var o = frameOffset(n);
    var l = r.left + o[0], t = r.top + o[1], rt = r.right + o[0], b = r.bottom + o[1];
    var cl = 0, ct = 0, cr = window.innerWidth, cb = window.innerHeight;
    var node = n, pos = styleOf(n).position;
    for (;;) {
      var p = parentOf(node);
      if (!p) {
        // Out of a frame's document: its <iframe> box (the frame's
        // viewport) clips it, fixed or not.
        var fe = node.ownerDocument.defaultView.frameElement;
        if (!fe) break;
        var fr = fe.getBoundingClientRect(), fo = frameOffset(fe);
        cl = Math.max(cl, fr.left + fo[0]); ct = Math.max(ct, fr.top + fo[1]);
        cr = Math.min(cr, fr.right + fo[0]); cb = Math.min(cb, fr.bottom + fo[1]);
        node = fe; pos = styleOf(fe).position;
        continue;
      }
      node = p;
      if (pos === 'fixed') continue;
      var s = styleOf(p), d = p.ownerDocument;
      var contains = !(pos === 'absolute' && s.position === 'static');
      if (contains && p !== d.body && p !== d.documentElement) {
        var pr = p.getBoundingClientRect(), po = frameOffset(p);
        if (s.overflowX !== 'visible') { cl = Math.max(cl, pr.left + po[0]); cr = Math.min(cr, pr.right + po[0]); }
        if (s.overflowY !== 'visible') { ct = Math.max(ct, pr.top + po[1]); cb = Math.min(cb, pr.bottom + po[1]); }
      }
      if (pos === 'absolute') { if (s.position !== 'static') pos = s.position; }
      else if (s.position === 'fixed' || s.position === 'absolute') pos = s.position;
    }
    var w = Math.min(rt, cr) - Math.max(l, cl), h = Math.min(b, cb) - Math.max(t, ct);
    return w > 0 && h > 0 ? Math.min(1, (w * h) / area) : 0;
  }
  // Visible for an assertion (D-1): rendered (opacity 0 is hidden), a
  // non-empty box, and on screen by at least `minArea` of it (any pixel by
  // default). `page: true` is the old rule, anywhere on the page
  // (`web.visibility: page`).
  function shown(n, opts) {
    opts = opts || {};
    if (!isVisible(n) || !hasBox(n)) return false;
    if (opts.page) return true;
    var ratio = shownRatio(n), min = Number(opts.minArea) || 0;
    return ratio > 0 && ratio >= min;
  }
  function ownDisabled(n) {
    return n.disabled === true || (n.matches && n.matches(':disabled')) || n.getAttribute('aria-disabled') === 'true';
  }
  // The parent across shadow roots (a shadow root's host).
  function parentOf(n) {
    if (n.parentElement) return n.parentElement;
    var p = n.parentNode;
    return p && p.host ? p.host : null;
  }

  // D-39 state: input focus (the deepest active element, across shadow
  // roots and frames), password, checkbox / switch / radio state (native or
  // ARIA; "mixed" is unknown), and the part of the box inside the viewport
  // of its frame. The active element and each frame's viewport are read
  // once per evaluation (a walk captures thousands of nodes), and forgotten
  // when it ends.
  var stateCache = null;
  function state() {
    if (!stateCache) {
      stateCache = { active: focused(), views: [] };
      Promise.resolve().then(function () { stateCache = null; });
    }
    return stateCache;
  }
  function viewport(doc) {
    var views = state().views;
    for (var i = 0; i < views.length; i++) if (views[i][0] === doc) return views[i];
    var v = doc.defaultView, vp = [doc, v ? v.innerWidth : 0, v ? v.innerHeight : 0];
    views.push(vp);
    return vp;
  }
  function checkedOf(n) {
    if (n.tagName === 'INPUT') {
      var t = (n.type || '').toLowerCase();
      if (t === 'checkbox' || t === 'radio') return !!n.checked;
    }
    var role = n.getAttribute('role');
    if (role && /^(checkbox|switch|radio|menuitemcheckbox|menuitemradio)$/.test(role)) {
      var a = n.getAttribute('aria-checked');
      if (a === 'true') return true;
      if (a === 'false' || a === null) return false;
    }
    return null;
  }
  function visibleRect(n, r, ox, oy) {
    var vp = viewport(n.ownerDocument);
    var x1 = Math.max(r.left, 0), y1 = Math.max(r.top, 0);
    var x2 = Math.min(r.right, vp[1]), y2 = Math.min(r.bottom, vp[2]);
    return [Math.round(x1 + ox), Math.round(y1 + oy), Math.round(Math.max(x2 - x1, 0)), Math.round(Math.max(y2 - y1, 0))];
  }

  // A name for a clickable element with no text, label or id of its own
  // (an icon: `<span class="trash">` drawn by CSS, `<i class="fa-star">`,
  // `<button class="p-2 rounded"><svg><title>Delete</title></svg></button>`),
  // so the agent can see and name it: its title or alt, else a name one
  // level down (an svg <title>, an <img alt>, a child's aria-label, an
  // svg `<use href="#icon-trash">`, an icon-font child's class), else its
  // class words that aren't utility classes, else its image's file name.
  // Velocity's `hint` carries it (the placeholder role on devices: what an
  // unlabelled control is for). Tailwind and CSS-module class names read
  // as "p 2 rounded hover:bg gray 100" and "Button root x3F9a" (review 10 #8).
  var ICON_NOISE = /^(fa|fas|far|fab|fal|fad|bi|glyphicon|material-icons|material-symbols-outlined|material-symbols-rounded|icon|icons|ico|btn|button|svg|mdi|ion|la|lucide|feather|octicon|heroicon)$/;
  var STATE_WORD = /^(active|clicked|selected|hover|disabled|on|off|open|closed|show|hide|is|has)$/;
  var UTILITY = new RegExp('^-?(p|px|py|pt|pb|pl|pr|ps|pe|m|mx|my|mt|mb|ml|mr|ms|me|w|h|size|min-w|min-h|max-w|max-h|' +
    'gap|gap-x|gap-y|space-x|space-y|text|bg|border|border-[trblxy]|rounded|rounded-[a-z]+|shadow|font|leading|tracking|' +
    'z|top|left|right|bottom|inset|inset-x|inset-y|opacity|ring|ring-offset|outline|flex|grid|grid-cols|grid-rows|col|' +
    'col-span|row|row-span|items|justify|content|self|place|order|overflow|overflow-x|overflow-y|cursor|transition|' +
    'duration|delay|ease|transform|translate-x|translate-y|scale|rotate|fill|stroke|sr|select|pointer-events|align|' +
    'whitespace|break|truncate|block|inline|inline-block|inline-flex|hidden|relative|absolute|fixed|sticky|static|' +
    'visible|invisible|container|aspect|object|underline|no-underline|uppercase|lowercase|capitalize|italic|' +
    'antialiased|shrink|grow|basis|divide|from|to|via|animate|decoration|backdrop|blur|filter|drop-shadow|line-clamp|' +
    'appearance|resize|list|float|clear|isolate|mix-blend|will-change|accent|caret|scroll|snap|touch|d|col-auto|' +
    'sm|md|lg|xl|xxl|xs)(-|$)');
  function classWords(cls) {
    var words = [];
    (cls || '').split(/\s+/).forEach(function (w) {
      if (!w) return;
      // Utilities with a variant or arbitrary value (`hover:bg-x`,
      // `w-[3px]`, `md/2`), CSS-module and CSS-in-JS hashes
      // (`Button_root__x3F9a`, `css-1x2y3z`, `sc-bdfBwQ`).
      if (/[:\[\]\/!@]/.test(w) || /__/.test(w) || /^(css|sc|jsx|svelte|emotion|tw)-/.test(w)) return;
      if (UTILITY.test(w)) return;
      w = w.replace(/^(fa|fas|far|fab|bi|icon|icons|glyphicon|mdi|ion|lucide|feather|octicon|ti|ri|bx|gg)-/, '');
      if (!w || ICON_NOISE.test(w) || STATE_WORD.test(w)) return;
      // A hash-like token: letters and digits mixed with no separator
      // (`x3F9a`, `a1b2c3`); `trash-2` and `h1` style names stay.
      if (/^[A-Za-z0-9]{5,}$/.test(w) && /\d/.test(w) && /[A-Za-z]/.test(w) && !/^[a-z]+\d$/.test(w)) return;
      words.push(w);
    });
    return words.join(' ').replace(/[-_]+/g, ' ').trim();
  }
  // A name one level down: what an icon button's child says it is.
  function childName(n) {
    var tries = n.querySelectorAll('[aria-label]:not([aria-hidden="true"]), svg title, img[alt], use');
    for (var i = 0; i < tries.length; i++) {
      var c = tries[i], v = '';
      if (c.tagName.toLowerCase() === 'title') v = c.textContent;
      else if (c.tagName === 'IMG') v = c.getAttribute('alt');
      else if (c.tagName.toLowerCase() === 'use') {
        var href = c.getAttribute('href') || c.getAttribute('xlink:href') || '';
        var frag = href.split('#')[1] || '';
        v = frag.replace(/^(icon|icons|i|ico|svg|sprite|glyph)[-_]/, '');
      } else v = c.getAttribute('aria-label');
      v = norm(v).replace(/[-_]+/g, ' ');
      if (v) return v;
    }
    // An icon-font child (`<i class="fa fa-trash">`, `<span class="bi-x">`,
    // a Material ligature is text and was caught before).
    var kids = n.querySelectorAll('i[class], span[class], svg[class]');
    for (var j = 0; j < kids.length && j < 4; j++) {
      var kc = kids[j].getAttribute('class') || '';
      if (!/(^|\s)(fa|fas|far|fab|bi|icon|glyphicon|mdi|ion|lucide|feather|octicon|ti|ri|bx)[-\s]|(^|\s)(fa|bi|icon)-/.test(kc)) continue;
      var w = classWords(kc);
      if (w) return w;
    }
    return '';
  }
  function iconName(n, text) {
    if (text || n.id || n.getAttribute('aria-label') || n.getAttribute('aria-labelledby')) return null;
    var r = n.getBoundingClientRect();
    if (r.width < 4 || r.height < 4 || r.width > 160 || r.height > 160) return null;
    var cs = window.getComputedStyle(n);
    var t = (n.type || '').toLowerCase();
    var control = n.tagName === 'BUTTON' || n.tagName === 'SUMMARY' || (n.tagName === 'A' && n.hasAttribute('href')) ||
      (n.tagName === 'INPUT' && /^(button|submit|reset|image)$/.test(t));
    var clickable = control || cs.cursor === 'pointer' || n.hasAttribute('onclick') ||
      /^(button|link|menuitem|tab|checkbox|switch)$/.test(n.getAttribute('role') || '');
    // A small element drawn only by a CSS image is an icon even when its
    // pointer cursor appears on hover only (`.controls span:hover`).
    var drawn = !n.children.length && (/url\(/.test(cs.content || '') || /url\(/.test(cs.backgroundImage || ''));
    if (!clickable && !drawn) return null;
    var named = n.getAttribute('title') || n.getAttribute('alt');
    if (named && named.trim()) return named.trim();
    var down = childName(n);
    if (down) return down;
    var cls = (typeof n.className === 'string' ? n.className : (n.getAttribute('class') || ''));
    var words = classWords(cls);
    if (words) return words;
    var img = (cs.content && cs.content.indexOf('url(') >= 0 ? cs.content : cs.backgroundImage) || '';
    var m = img.match(/\/([^\/?#"')]+?)\.(png|svg|jpe?g|gif|webp)/i);
    return m ? m[1].replace(/[-_]+/g, ' ') : null;
  }

  // The capture object `dom::json_to_element` reads. `lim` caps the text.
  function cap(n, lim, ox, oy) {
    if (!n) return null;
    var r = n.getBoundingClientRect();
    if (ox === undefined) { var o = frameOffset(n); ox = o[0]; oy = o[1]; }
    var text = textOf(n);
    if (lim && text.length > lim) text = text.slice(0, lim);
    var c = {
      velId: vid(n),
      tag: n.tagName.toLowerCase(),
      domId: n.id || null,
      testId: n.getAttribute('data-testid') || n.getAttribute('flt-semantics-identifier') || null,
      text: text || null,
      ariaLabel: n.getAttribute('aria-label'),
      hint: n.getAttribute('placeholder') || iconName(n, text) || null,
      role: roleOf(n) || null,
      className: typeof n.className === 'string' ? (n.className || null) : (n.getAttribute('class') || null),
      x: Math.round(r.x + ox), y: Math.round(r.y + oy),
      w: Math.round(r.width), h: Math.round(r.height),
      visible: isVisible(n),
      // The control the node stands for: a span in a disabled button read
      // as enabled, so the executor's enabled-wait never ran and only the
      // driver's 1 s actionability budget applied (review M-4).
      enabled: !ownDisabled(n) && !ownDisabled(retarget(n))
    };
    // Only what differs from the default, to keep a big walk's reply
    // small: `vb` only when part of the box is outside the viewport.
    if (n === state().active && n !== n.ownerDocument.body) c.focused = true;
    if (n.tagName === 'INPUT' && (n.type || '').toLowerCase() === 'password') c.obscured = true;
    var checked = checkedOf(n);
    if (checked !== null) c.checked = checked;
    if (selectedOf(n)) c.selected = true;
    var vb = visibleRect(n, r, ox, oy);
    if (vb[0] !== c.x || vb[1] !== c.y || vb[2] !== c.w || vb[3] !== c.h) c.vb = vb;
    return c;
  }

  // ── Queries ───────────────────────────────────────────────────────────
  // The document, every open shadow root and every same-origin frame: a
  // button inside a web component or an embedded frame is part of the page.
  function roots() {
    var out = [document];
    for (var r = 0; r < out.length; r++) {
      var els = out[r].querySelectorAll('*');
      for (var i = 0; i < els.length; i++) {
        var e = els[i];
        if (e.shadowRoot) out.push(e.shadowRoot);
        if (e.tagName === 'IFRAME' || e.tagName === 'FRAME') {
          try { var d = e.contentDocument; if (d && d.body) out.push(d); } catch (x) { /* cross-origin */ }
        }
      }
    }
    return out;
  }
  function invalid(what, s, x) {
    return { velInvalid: 'invalid ' + what + ' ' + JSON.stringify(s) + ': ' + (x && x.message || x) };
  }
  // A syntax error is thrown as {velInvalid}, never swallowed: a typo that
  // matched nothing made assertNotVisible pass forever (VEL-338).
  function qall(css) {
    try { document.createDocumentFragment().querySelector(css); } catch (x) { throw invalid('CSS selector', css, x); }
    var rs = roots(), o = [];
    for (var i = 0; i < rs.length; i++) {
      var q = rs[i].nodeType === 9 && css === '*' ? 'body *' : css;
      var found = rs[i].querySelectorAll(q);
      for (var j = 0; j < found.length; j++) o.push(found[j]);
    }
    return o;
  }
  function xall(xp) {
    var r;
    try { r = document.evaluate(xp, document, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null); }
    catch (x) { throw invalid('XPath', xp, x); }
    var o = [];
    for (var i = 0; i < r.snapshotLength; i++) { var n = r.snapshotItem(i); if (n.nodeType === 1) o.push(n); }
    return o;
  }
  function textHit(n, t) {
    return textIs(n, t.norm, !t.exact, t.fold);
  }
  // Of `list` (deep-query order: each root in document order), those that
  // satisfy every text part with no such element inside them: the button,
  // not the <body> around it. One pass, children before parents, so it is
  // linear; comparing every match with every other took 14-23 s on 20 000
  // rows (VEL-120). An element with a match inside is never read.
  function innermost(list, texts) {
    return innermostBy(list, function (n) {
      for (var k = 0; k < texts.length; k++) if (!textHit(n, texts[k])) return false;
      return true;
    });
  }
  function innermostBy(list, ok) {
    var inside = new Set(), out = [];
    for (var i = list.length - 1; i >= 0; i--) {
      var n = list[i], p;
      if (inside.has(n)) {
        p = parentOf(n); if (p) inside.add(p);
        continue;
      }
      if (ok(n)) { out.push(n); p = parentOf(n); if (p) inside.add(p); }
    }
    return out.reverse();
  }

  // ── Accessible name (D-W2) ───────────────────────────────────────────
  // `accessibilityId:` on the web is the element's accessible name, as a
  // screen reader announces it (what the label is on Android and iOS):
  // `aria-label` matches as before, and so does the name computed the
  // accname way: aria-labelledby, then aria-label, then the host language
  // (a field's <label>s, alt, a button's value, a fieldset's legend …),
  // then the content for roles named by it (buttons, links, headings,
  // cells …), then title (and a field's placeholder).
  var NAMED_BY_CONTENT = {
    button: 1, link: 1, heading: 1, cell: 1, gridcell: 1, columnheader: 1, rowheader: 1,
    tab: 1, menuitem: 1, menuitemcheckbox: 1, menuitemradio: 1, option: 1, radio: 1,
    checkbox: 1, 'switch': 1, treeitem: 1, tooltip: 1, row: 1
  };
  // The element's role (S1), by Playwright's rules: the first known token
  // of an explicit `role=`, else the tag's implicit ARIA role (html-aam, as
  // Playwright's `getImplicitAriaRole`). '' when it has none.
  var ROLES = ('alert alertdialog application article banner blockquote button caption cell checkbox code ' +
    'columnheader combobox complementary contentinfo definition deletion dialog directory document emphasis ' +
    'feed figure form generic grid gridcell group heading img insertion link list listbox listitem log main ' +
    'marquee math meter menu menubar menuitem menuitemcheckbox menuitemradio navigation none note option ' +
    'paragraph presentation progressbar radio radiogroup region row rowgroup rowheader scrollbar search ' +
    'searchbox separator slider spinbutton status strong subscript superscript switch tab table tablist ' +
    'tabpanel term textbox time timer toolbar tooltip tree treegrid treeitem').split(' ');
  var ROLE_SET = {};
  ROLES.forEach(function (r) { ROLE_SET[r] = 1; });
  var INPUT_ROLES = { button: 'button', image: 'button', reset: 'button', submit: 'button', file: 'button',
    checkbox: 'checkbox', radio: 'radio', range: 'slider', number: 'spinbutton' };
  var TAG_ROLES = { ARTICLE: 'article', ASIDE: 'complementary', BLOCKQUOTE: 'blockquote', BUTTON: 'button',
    CAPTION: 'caption', CODE: 'code', DATALIST: 'listbox', DD: 'definition', DEL: 'deletion', DETAILS: 'group',
    DFN: 'term', DIALOG: 'dialog', DT: 'term', EM: 'emphasis', FIELDSET: 'group', FIGURE: 'figure',
    HR: 'separator', HTML: 'document', INS: 'insertion', LI: 'listitem', MAIN: 'main', MATH: 'math',
    MENU: 'list', METER: 'meter', NAV: 'navigation', OL: 'list', OPTGROUP: 'group', OPTION: 'option',
    OUTPUT: 'status', P: 'paragraph', PROGRESS: 'progressbar', SEARCH: 'search', STRONG: 'strong',
    SUB: 'subscript', SUMMARY: 'button', SUP: 'superscript', TABLE: 'table', TBODY: 'rowgroup',
    TEXTAREA: 'textbox', TFOOT: 'rowgroup', THEAD: 'rowgroup', TIME: 'time', TR: 'row', UL: 'list',
    ADDRESS: 'group', svg: 'img', SVG: 'img' };
  function closestTag(n, re) {
    for (var p = parentOf(n); p; p = parentOf(p)) if (re.test(p.tagName)) return p;
    return null;
  }
  function implicitRole(n) {
    var tag = n.tagName;
    if ((tag === 'A' || tag === 'AREA') && n.hasAttribute('href')) return 'link';
    if (/^H[1-6]$/.test(tag)) return 'heading';
    if (tag === 'INPUT') {
      var t = (n.type || 'text').toLowerCase();
      if (t === 'hidden') return '';
      if (t === 'search') return n.hasAttribute('list') ? 'combobox' : 'searchbox';
      if (t === 'email' || t === 'tel' || t === 'text' || t === 'url' || t === '') {
        return n.list ? 'combobox' : 'textbox';
      }
      return INPUT_ROLES[t] || 'textbox';
    }
    if (tag === 'SELECT') return (n.multiple || n.size > 1) ? 'listbox' : 'combobox';
    if (tag === 'IMG') {
      return (n.getAttribute('alt') === '' && !n.getAttribute('title') && !n.hasAttribute('aria-label') &&
        !n.hasAttribute('aria-labelledby')) ? 'presentation' : 'img';
    }
    if (tag === 'TD') {
      var tbl = closestTag(n, /^TABLE$/);
      var tr = tbl && (tbl.getAttribute('role') || '').trim();
      return (tr === 'grid' || tr === 'treegrid') ? 'gridcell' : 'cell';
    }
    if (tag === 'TH') {
      var scope = (n.getAttribute('scope') || '').toLowerCase();
      return (scope === 'row' || scope === 'rowgroup') ? 'rowheader' : 'columnheader';
    }
    if (tag === 'HEADER' || tag === 'FOOTER') {
      return closestTag(n, /^(ARTICLE|ASIDE|MAIN|NAV|SECTION)$/) ? '' : (tag === 'HEADER' ? 'banner' : 'contentinfo');
    }
    if (tag === 'FORM' || tag === 'SECTION') {
      var named = n.hasAttribute('aria-label') || n.hasAttribute('aria-labelledby') || n.hasAttribute('title');
      return named ? (tag === 'FORM' ? 'form' : 'region') : '';
    }
    return TAG_ROLES[tag] || '';
  }
  function roleOf(n) {
    var explicit = (n.getAttribute('role') || '').trim().toLowerCase().split(/\s+/);
    for (var i = 0; i < explicit.length; i++) if (ROLE_SET[explicit[i]]) return explicit[i];
    return implicitRole(n);
  }
  function labelledBy(n) {
    var ids = (n.getAttribute('aria-labelledby') || '').trim();
    if (!ids) return '';
    var d = n.getRootNode(), parts = [];
    ids.split(/\s+/).forEach(function (id) {
      var e = d.getElementById ? d.getElementById(id) : null;
      if (e) parts.push(norm(e.getAttribute('aria-label') || e.textContent));
    });
    return norm(parts.join(' '));
  }
  function hostName(n) {
    var tag = n.tagName, t = (n.type || '').toLowerCase();
    if (tag === 'INPUT' && (t === 'button' || t === 'submit' || t === 'reset')) {
      return norm(n.value || (t === 'submit' ? 'Submit' : t === 'reset' ? 'Reset' : ''));
    }
    if (tag === 'INPUT' && t === 'image') return norm(n.getAttribute('alt'));
    if ((tag === 'INPUT' || tag === 'SELECT' || tag === 'TEXTAREA') && n.labels && n.labels.length) {
      var l = [];
      for (var i = 0; i < n.labels.length; i++) l.push(textOf(n.labels[i]) || n.labels[i].textContent);
      return norm(l.join(' '));
    }
    if (tag === 'IMG' || tag === 'AREA') return norm(n.getAttribute('alt'));
    var first = function (sel) { var c = n.querySelector(':scope > ' + sel); return c ? norm(c.textContent) : ''; };
    if (tag === 'FIELDSET') return first('legend');
    if (tag === 'FIGURE') return first('figcaption');
    if (tag === 'TABLE') return first('caption');
    return '';
  }
  // Name from content that innerText can't see: an <svg>'s <title>, an
  // <img alt> or a child's aria-label, outside aria-hidden subtrees (an
  // icon button `<button><svg><title>Delete</title>…</svg></button>` is
  // "Delete" to a screen reader; review 10 #3 and #8).
  function contentName(n) {
    var c = n.querySelectorAll('svg title, img[alt], [aria-label]');
    for (var i = 0; i < c.length; i++) {
      if (c[i].closest('[aria-hidden="true"]')) continue;
      var tag = c[i].tagName.toLowerCase();
      var v = norm(tag === 'title' ? c[i].textContent : tag === 'img' ? c[i].getAttribute('alt') : c[i].getAttribute('aria-label'));
      if (v) return v;
    }
    return '';
  }
  function accName(n) {
    var name = labelledBy(n) || norm(n.getAttribute('aria-label')) || hostName(n);
    if (!name && NAMED_BY_CONTENT[roleOf(n)]) name = textOf(n) || contentName(n);
    if (!name) name = norm(n.getAttribute('title'));
    if (!name && (n.tagName === 'INPUT' || n.tagName === 'TEXTAREA')) name = norm(n.getAttribute('placeholder'));
    return name;
  }
  function byName(want) {
    var w = norm(want);
    if (!w) return [];
    // D-W2: `aria-label` first (the old `[aria-label=…]` rule, so an
    // earlier <button>Submit</button> never takes over), then the computed
    // name only when no aria-label matches (review MAJOR 7).
    // The priority holds among elements a user can see: a closed dialog's
    // hidden `aria-label="Close"` never hides a visible <button>Close</button>
    // (review-fixes #4). With no visible aria-label match, both lists go to
    // `pick` together, aria-label matches first, as before D-W2.
    var all = qall('*');
    var labelled = innermostBy(all, function (n) { return norm(n.getAttribute('aria-label')) === w; });
    if (labelled.some(function (n) { return isVisible(n) && hasBox(n); })) return labelled;
    var named = innermostBy(all, function (n) { return accName(n) === w; });
    if (!labelled.length) return named;
    var seen = new Set(labelled);
    return labelled.concat(named.filter(function (n) { return !seen.has(n); }));
  }
  // `name:` (S1): the computed accessible name, compared as `text:` is
  // (exactly, then ignoring case on the fold tier).
  function nameHit(n, t) {
    var a = accName(n);
    return t.fold ? a.toLowerCase() === t.norm : a === t.norm;
  }
  function selectedOf(n) {
    if (n.tagName === 'OPTION') return !!n.selected;
    return n.getAttribute('aria-selected') === 'true';
  }
  // A role or state part (S1/S2) as a test of one element.
  function partHit(n, p) {
    if (p.role != null) return roleOf(n) === p.role;
    switch (p.state) {
      case 'checked': return checkedOf(n) === p.value;
      case 'focused': return (n === state().active && n !== n.ownerDocument.body) === p.value;
      case 'selected': return selectedOf(n) === p.value;
      case 'enabled': return (!ownDisabled(n) && !ownDisabled(retarget(n))) === p.value;
    }
    return false;
  }
  // Matching tiers, as on the devices (`selector_match`, D-10): `text:`
  // exactly first; only when the whole selector finds nothing, once more
  // with every `text:` part ignoring case. `textContains` always ignores
  // case (Playwright's default).
  var foldTier = false;
  function prepText(sel) {
    var fold = foldTier || !sel.exact, w = norm(sel.text);
    return { exact: !!sel.exact, fold: fold, norm: fold ? w.toLowerCase() : w };
  }
  function hasExactText(sel) {
    if (sel.text != null) return !!sel.exact;
    if (sel.accName != null) return true;
    // An index is tiered on its own selector (in `matchAll`).
    if (sel.and) return sel.and.some(hasExactText);
    return false;
  }
  function matchTiered(sel) {
    var r = matchAll(sel);
    if (r.length || foldTier || !hasExactText(sel)) return r;
    foldTier = true;
    try { return matchAll(sel); } finally { foldTier = false; }
  }
  // Reading order, as `selector_match` counts `index:` on the devices: rows
  // top to bottom, left to right within a row.
  function readingOrder(list) {
    var boxes = list.map(function (n, i) {
      var r = n.getBoundingClientRect(), o = frameOffset(n);
      return { n: n, i: i, x: r.left + o[0], y: r.top + o[1], h: r.height };
    });
    boxes.sort(function (a, b) { return a.y - b.y || a.x - b.x || a.i - b.i; });
    var rows = [];
    boxes.forEach(function (b) {
      var row = rows[rows.length - 1], f = row && row[0];
      var within = function (r, cy) { return cy >= r.y && cy < r.y + Math.max(r.h, 1); };
      if (f && (within(f, b.y + b.h / 2) || within(b, f.y + f.h / 2))) row.push(b); else rows.push([b]);
    });
    var out = [];
    rows.forEach(function (row) {
      row.sort(function (a, b) { return a.x - b.x || a.i - b.i; });
      row.forEach(function (b) { out.push(b.n); });
    });
    return out;
  }
  // A match `index:` counts: rendered, a non-empty box, and that box (in
  // top-page px, rounded as `cap` reports it) meets the top viewport, which
  // is the rect explore matches the tree against.
  function countable(n) {
    if (!isVisible(n) || !hasBox(n)) return false;
    var r = n.getBoundingClientRect(), o = frameOffset(n);
    var x = Math.round(r.x + o[0]), y = Math.round(r.y + o[1]);
    var w = Math.round(r.width), h = Math.round(r.height);
    return w > 0 && h > 0 && x < window.innerWidth && x + w > 0 && y < window.innerHeight && y + h > 0;
  }
  // Every element a selector (built by `selector::selector_json`) matches,
  // in deep-query order.
  //   {css}  {xpath}  {text, exact}  {name}  {and: [...]}  {nth: sel, i, dom}
  //   {role}  {accName}  {state, value} (S1/S2)
  function matchAll(sel) {
    if (sel.css != null) return qall(sel.css);
    if (sel.xpath != null) return xall(sel.xpath);
    if (sel.text != null) return innermost(qall('*'), [prepText(sel)]);
    if (sel.name != null) return byName(sel.name);
    if (sel.role != null || sel.state != null) {
      return qall('*').filter(function (n) { return partHit(n, sel); });
    }
    if (sel.accName != null) {
      var nt = prepText({ text: sel.accName, exact: true });
      return nt.norm ? innermostBy(qall('*'), function (n) { return nameHit(n, nt); }) : [];
    }
    if (sel.nth) {
      // The index takes its tier from its own selector: two exact "Add"
      // rows and `index: 2` find nothing rather than a folded "ADD".
      // Counted the way explore counts it when it writes the index
      // (`selector_build::robust_selector` over the walked tree) and as the
      // devices count it: rendered matches with a box that meets the
      // viewport, in reading order. A CSS list counted every match in
      // document order, hidden ones too, and text page-wide, so a recorded
      // `index: 1` tapped another element on replay (review 10 #1).
      var c = readingOrder(matchTiered(sel.nth).filter(countable));
      return c[sel.i] ? [c[sel.i]] : [];
    }
    if (sel.and) {
      // Every part on the same element. CSS, XPath and index parts narrow
      // the candidates; text parts must hold for the element itself, and
      // the innermost such element wins.
      // Role and state parts filter the candidates; name parts, like text
      // parts, hold for the element itself, innermost first.
      var base = null, texts = [], parts = [], names = [];
      sel.and.forEach(function (p) {
        if (p.text != null) { texts.push(prepText(p)); return; }
        if (p.role != null || p.state != null) { parts.push(p); return; }
        if (p.accName != null) { names.push(prepText({ text: p.accName, exact: true })); return; }
        var m = matchAll(p);
        if (base === null) base = m;
        else { var s = new Set(m); base = base.filter(function (n) { return s.has(n); }); }
      });
      if (base === null) base = qall('*');
      if (parts.length) base = base.filter(function (n) { return parts.every(function (p) { return partHit(n, p); }); });
      if (names.length) {
        if (names.some(function (t) { return !t.norm; })) return [];
        base = innermostBy(base, function (n) { return names.every(function (t) { return nameHit(n, t); }); });
      }
      return texts.length ? innermost(base, texts) : base;
    }
    return [];
  }
  // The first visible match, else the first: several nodes often match
  // while one is on screen (hidden SPA screens, closed menus).
  function pick(l) {
    // On screen first (D-1), so an assertion reads the match a user sees.
    for (var i = 0; i < l.length; i++) if (shown(l[i])) return l[i];
    for (i = 0; i < l.length; i++) if (isVisible(l[i]) && hasBox(l[i])) return l[i];
    for (i = 0; i < l.length; i++) if (isVisible(l[i])) return l[i];
    return l[0] || null;
  }
  // Matches on screen. A box outside its window's viewport is skipped
  // before `shown` (a style read per ancestor) is asked.
  function countShown(l) {
    var n = 0;
    for (var i = 0; i < l.length; i++) {
      var e = l[i], w = e.ownerDocument.defaultView || window, r = e.getBoundingClientRect();
      if (r.bottom <= 0 || r.right <= 0 || r.top >= w.innerHeight || r.left >= w.innerWidth) continue;
      if (shown(e)) n++;
    }
    return n;
  }
  function guarded(f) {
    try { return f(); } catch (e) { if (e && e.velInvalid) return { velInvalid: e.velInvalid }; throw e; }
  }

  // ── The DOM tree ──────────────────────────────────────────────────────
  // Every element under <body>, open shadow roots and same-origin frames
  // included, as a flat list in pre-order with each node's parent index.
  // Iterative and flat, so neither a deep page nor the JSON nesting limit
  // cuts it short: the recursive walk stopped at depth 40 and tree-path
  // selectors missed deeper elements or matched an ancestor (VEL-126).
  function walk(lim) {
    var out = [], stack = [];
    if (document.body) stack.push([document.body, -1, 0, 0]);
    while (stack.length) {
      var it = stack.pop(), n = it[0], ox = it[2], oy = it[3];
      var c = cap(n, lim, ox, oy);
      c.p = it[1];
      var me = out.length;
      out.push(c);
      var kids = [];
      for (var i = 0; i < n.children.length; i++) kids.push([n.children[i], me, ox, oy]);
      if (n.shadowRoot) for (i = 0; i < n.shadowRoot.children.length; i++) kids.push([n.shadowRoot.children[i], me, ox, oy]);
      if (n.tagName === 'IFRAME' || n.tagName === 'FRAME') {
        try {
          var d = n.contentDocument;
          if (d && d.body) {
            var r = n.getBoundingClientRect();
            kids.push([d.body, me, ox + r.left + n.clientLeft, oy + r.top + n.clientTop]);
          }
        } catch (x) { /* cross-origin: not reachable from here */ }
      }
      for (i = kids.length - 1; i >= 0; i--) stack.push(kids[i]);
      if (c.text && n.children.length && wrapper(n)) c.bare = true;
    }
    return quietWrappers(out);
  }
  // A wrapper: an element whose text is all its children's (no text node
  // of its own), and that nothing marks as a control or a named item.
  function wrapper(n) {
    if (CONTROL[n.tagName] || n.isContentEditable || NAMED_BY_CONTENT[roleOf(n)]) return false;
    if (n.hasAttribute('onclick') || n.hasAttribute('tabindex')) return false;
    for (var c = n.firstChild; c; c = c.nextSibling) {
      if (c.nodeType === 3 && /\S/.test(c.data)) return false;
    }
    return styleOf(n).cursor !== 'pointer';
  }
  // On a device a container has no text; here every <body>, <main> and
  // wrapper <div> carried its whole subtree's, so the agent's element
  // list repeated the page once per level and a wrapper whose 200
  // characters held the goal's words outranked the button inside it
  // (review 10 #4). A wrapper's text goes when it only repeats its one
  // child's, or joins several children's into more than a short phrase;
  // a price split in spans (`$` `42`) or a heading in a <div> keeps its
  // own. The page's `text:` matching is unchanged (the innermost match
  // wins there anyway).
  var WRAPPER_TEXT_MAX = 40;
  function quietWrappers(out) {
    var texts = out.map(function (c) { return c.text; }), kids = out.map(function () { return []; });
    for (var i = 1; i < out.length; i++) if (out[i].p >= 0) kids[out[i].p].push(i);
    for (i = 0; i < out.length; i++) {
      var c = out[i];
      if (!c.bare) continue;
      delete c.bare;
      var withText = kids[i].filter(function (k) { return texts[k]; });
      if ((withText.length === 1 && texts[withText[0]] === c.text) ||
        (withText.length > 1 && c.text.length > WRAPPER_TEXT_MAX)) c.text = null;
    }
    return out;
  }

  // ── Actionability ─────────────────────────────────────────────────────
  function nextFrame() {
    return new Promise(function (res) {
      var done = false, fire = function () { if (!done) { done = true; res(); } };
      requestAnimationFrame(fire);
      setTimeout(fire, 100); // rAF stalls in a hidden page
    });
  }
  var CONTROL = { INPUT: 1, SELECT: 1, TEXTAREA: 1 };
  // A form control a user can't see (an opacity-0 custom checkbox, a styled
  // file input) is operated through its visible label (VEL-275).
  function clickTarget(n) {
    if (!CONTROL[n.tagName] || !n.labels) return n;
    var s = styleOf(n), r = n.getBoundingClientRect();
    if (parseFloat(s.opacity) > 0 && r.width > 1 && r.height > 1) return n;
    for (var i = 0; i < n.labels.length; i++) {
      var l = n.labels[i];
      if (isVisible(l) && hasBox(l)) return l;
    }
    return n;
  }
  // aria-disabled set on a container (Playwright inherits it up the chain,
  // across shadow hosts); `false` on the way up stops it.
  function inheritedAriaDisabled(n) {
    for (var p = parentOf(n); p; p = parentOf(p)) {
      var a = (p.getAttribute('aria-disabled') || '').toLowerCase();
      if (a === 'true') return p;
      if (a === 'false') return null;
    }
    return null;
  }
  function tagOf(n) { return '<' + n.tagName.toLowerCase() + '>'; }
  // The control a matched node stands for, for the enabled check
  // (Playwright's `retarget`): a <span> inside a <button> is the button, and
  // text inside a <label> is the label's control. Checking the matched span
  // let a tap on `<button disabled><span>Save</span></button>` pass without
  // the button ever activating (NB-1).
  var RETARGET = 'button,[role=button],[role=checkbox],[role=radio],a,[role=link]';
  function retarget(n) {
    if (CONTROL[n.tagName] || n.isContentEditable) return n;
    var e = n.closest ? n.closest(RETARGET) : null;
    if (e) return e;
    var l = n.closest ? n.closest('label') : null;
    return l && l.control ? l.control : n;
  }
  var ALIGN = [null, 'end', 'center', 'start'];
  // One actionability attempt (`driver::actionable_point` retries it):
  // scrolled into view (`align` cycles end/center/start after the first,
  // if-needed scroll, to get out from under a sticky header), visible,
  // enabled, the same box on two animation frames, and a hit test at its
  // centre that lands on the element or inside it. Returns the fresh,
  // post-scroll click point in page coordinates.
  //
  // `mode` is the step's actionability rule (D-3). 'strict' (the default
  // for suites): still moving, an ancestor taking the click and an
  // inherited aria-disabled fail too. 'lenient': those only warn, as
  // before D-3 (`soft` marks a still-moving element, tapped where it is at
  // the deadline). 'force': no checks; the target's centre after the
  // scroll.
  function act(id, attempt, mode) {
    var n = byVid(id);
    if (!n) return { ok: false, reason: 'element detached from DOM' };
    var t = clickTarget(n), warn = [];
    var strict = mode === 'strict';
    var align = ALIGN[attempt % ALIGN.length];
    if (align === null) {
      if (t.scrollIntoViewIfNeeded) t.scrollIntoViewIfNeeded(true);
      else t.scrollIntoView({ block: 'center', inline: 'center' });
    } else {
      t.scrollIntoView({ block: align, inline: align === 'center' ? 'center' : 'nearest' });
    }
    if (mode === 'force') {
      var rf = t.getBoundingClientRect(), of = frameOffset(t);
      return { ok: true, x: Math.round(rf.left + rf.width / 2 + of[0]), y: Math.round(rf.top + rf.height / 2 + of[1]), warn: warn };
    }
    var s = styleOf(t);
    if (s.display === 'none' || s.visibility === 'hidden' || parseFloat(s.opacity) === 0) return { ok: false, reason: 'not visible' };
    var r = t.getBoundingClientRect();
    if (r.width === 0 || r.height === 0) return { ok: false, reason: 'zero size' };
    var ctl = retarget(n);
    if (ownDisabled(n) || ownDisabled(ctl)) {
      return { ok: false, reason: ctl === n ? 'disabled' : 'disabled (' + tagOf(ctl) + ')' };
    }
    var ad = inheritedAriaDisabled(n);
    if (ad && strict) return { ok: false, reason: 'disabled (inside ' + tagOf(ad) + ' aria-disabled="true")' };
    if (ad) warn.push('it is inside ' + tagOf(ad) + ' aria-disabled="true"');
    return nextFrame().then(nextFrame).then(function () {
      if (!t.isConnected) return { ok: false, reason: 'element detached from DOM' };
      var r2 = t.getBoundingClientRect();
      var moving = Math.abs(r2.left - r.left) > 0.5 || Math.abs(r2.top - r.top) > 0.5 ||
        Math.abs(r2.width - r.width) > 0.5 || Math.abs(r2.height - r.height) > 0.5;
      var cx = r2.left + r2.width / 2, cy = r2.top + r2.height / 2;
      var o = frameOffset(t), x = Math.round(cx + o[0]), y = Math.round(cy + o[1]);
      if (moving) return { ok: false, soft: !strict, reason: 'still moving', x: x, y: y, warn: warn };
      var top = t.getRootNode().elementFromPoint(cx, cy);
      if (!top) return { ok: false, reason: 'outside viewport' };
      var mine = function (h) { return h === t || t.contains(h) || h === n || n.contains(h); };
      // The control holding the target (Playwright retargets its hit check
      // the same way): a <span> with `pointer-events: none` in a <button>
      // sends the click to the button, which is the user's click, not an
      // overlay's (review M3).
      var holder = n.closest ? n.closest(RETARGET + ',label') : null;
      if (!mine(top) && !(holder && holder.contains(top))) {
        if (!top.contains(t)) return { ok: false, reason: 'obscured by ' + tagOf(top) };
        if (strict) return { ok: false, reason: 'obscured by its ancestor ' + tagOf(top) + ' (an overlay takes the click)' };
        warn.push('the click lands on its ancestor ' + tagOf(top));
      }
      // In a same-origin frame the hit test above is the frame's own: a
      // cookie banner on the page over the <iframe> took the click while
      // the step passed (review 10 #2). Each document up the chain must
      // put the point on the <iframe> that holds the target.
      var over = coverOverFrame(t, x, y);
      if (over) return { ok: false, reason: over === 'outside' ? 'outside viewport' : 'obscured by ' + tagOf(over) + ' (on the page, over its frame)' };
      return { ok: true, x: x, y: y, warn: warn };
    });
  }

  // What takes a click at (`x`, `y`, top-page px) instead of the frame
  // that holds `t`, in the documents above it: the element on top, or
  // 'outside' when the point is outside one of them; null when every
  // <iframe> up the chain is hit (or `t` is in the top document).
  function coverOverFrame(t, x, y) {
    var w = t.ownerDocument.defaultView;
    while (w && w.frameElement) {
      var fe = w.frameElement, o = frameOffset(fe);
      var h = fe.getRootNode().elementFromPoint(x - o[0], y - o[1]);
      if (!h) return 'outside';
      if (h !== fe) return h;
      w = fe.ownerDocument.defaultView;
    }
    return null;
  }

  // ── Hit target at dispatch time ──────────────────────────────────────
  // `act` checks the hit target, then the driver clicks: two round trips,
  // and a banner or toast that lands in between took the click. Armed for
  // one click, a capture-phase listener checks the first trusted pointer
  // event against the target and, when something else is under the
  // pointer, blocks it and every later event of the click (Playwright's
  // hit-target interceptor). An ancestor hit was already accepted by `act`.
  var armed = null;
  // Touch events too: on a touch profile the tap is a touch, and an
  // intruder's `touchstart`/`touchend` handlers (FastClick, Swiper) ran
  // while its pointer and mouse events were blocked (review 10 #10).
  // Cancelling `touchstart` also stops the mouse events derived from it.
  var POINTER_EVENTS = ['pointerdown', 'mousedown', 'pointerup', 'mouseup', 'click', 'dblclick', 'auxclick', 'contextmenu',
    'touchstart', 'touchmove', 'touchend'];
  // Not passive: Chrome makes window touch listeners passive by default,
  // and a passive listener can't cancel the touch.
  var LISTEN = { capture: true, passive: false };
  function arm(id) {
    disarm();
    var n = byVid(id);
    if (!n) return false;
    var t = clickTarget(n), win = t.ownerDocument.defaultView;
    // A click on an <iframe> is dispatched in that frame's document, never
    // to this window: whether it arrived can't be seen from here
    // (review-fi2 F3).
    var into = /^i?frame$/i.test(t.tagName || '') || /^i?frame$/i.test(n.tagName || '');
    // `nested`: the target's window is a frame's. A click that something
    // on a page above took never arrives in it, so nothing is blocked
    // here: not seeing it is the miss (review 10 #2, as review-i2 M1 for
    // cross-origin frames).
    var nested = false;
    try { nested = win !== win.top; } catch (x) { nested = true; }
    var st = { win: win, verdict: null, intruder: null, seen: false, into: into, nested: nested };
    st.listener = function (e) {
      if (!e.isTrusted) return;
      st.seen = true;
      if (st.verdict === null) {
        var path = e.composedPath ? e.composedPath() : [e.target], h = path[0];
        var ok = path.indexOf(t) !== -1 || path.indexOf(n) !== -1 ||
          (h && h.nodeType === 1 && h.contains(t));
        st.verdict = ok ? 'ok' : 'blocked';
        if (!ok) st.intruder = h && h.nodeType === 1 ? tagOf(h) + (h.id ? '#' + h.id : '') : 'something else';
      }
      if (st.verdict === 'blocked') { e.preventDefault(); e.stopImmediatePropagation(); }
    };
    POINTER_EVENTS.forEach(function (k) { win.addEventListener(k, st.listener, LISTEN); });
    armed = st;
    return true;
  }
  // `seen`: whether any trusted pointer event of the click reached this
  // window. In a cross-origin frame (W4) a click that something on the
  // top page took never arrives here, so nothing is blocked either; the
  // driver treats `seen: false` as a click that missed (review-i2 M1).
  // With `waitMs`, a click not seen yet is waited for that long first (the
  // frame's renderer gets it a moment after the page's).
  function disarm(waitMs) {
    var st = armed;
    if (!st) return { blocked: null };
    function finish() {
      if (armed === st) armed = null;
      POINTER_EVENTS.forEach(function (k) { try { st.win.removeEventListener(k, st.listener, true); } catch (x) { /* gone */ } });
      return { blocked: st.verdict === 'blocked' ? st.intruder : null, seen: st.into && !st.seen ? null : st.seen, nested: st.nested };
    }
    if (st.seen || st.into || !st.nested || !(waitMs > 0)) return finish();
    var end = Date.now() + waitMs;
    return new Promise(function (resolve) {
      (function poll() {
        if (st.seen || Date.now() >= end) resolve(finish());
        else setTimeout(poll, 10);
      })();
    });
  }

  // `scrollUntilVisible within:` a container below the fold: scroll it into
  // view first and report its fresh box, so the strokes land inside it
  // (VEL-069).
  function reveal(id) {
    var n = byVid(id);
    if (!n) return null;
    if (n.scrollIntoViewIfNeeded) n.scrollIntoViewIfNeeded(true);
    else n.scrollIntoView({ block: 'center', inline: 'center' });
    return cap(n);
  }

  // ── Fields ────────────────────────────────────────────────────────────
  // Shadow roots and frames are descended until neither applies (a shadow
  // root inside a frame included). A frame it can't enter (cross-origin:
  // a card field from Stripe, Braintree or Adyen) is returned as is, and
  // callers treat its focus as unknown (review MAJOR 3).
  function focused() {
    var e = document.activeElement;
    for (;;) {
      if (e && e.shadowRoot && e.shadowRoot.activeElement) { e = e.shadowRoot.activeElement; continue; }
      if (e && isFrame(e)) {
        var d = null;
        try { d = e.contentDocument; } catch (x) { d = null; }
        if (d && d.activeElement) { e = d.activeElement; continue; }
      }
      return e;
    }
  }
  function isFrame(e) { return e.tagName === 'IFRAME' || e.tagName === 'FRAME'; }
  var TEXT_INPUT = /^(text|search|email|url|tel|password|number|date|datetime-local|month|week|time|color|range)$/;
  function editable(e) {
    if (!e) return false;
    if (e.isContentEditable) return true;
    if (e.tagName === 'TEXTAREA') return !e.readOnly && !e.disabled;
    if (e.tagName === 'INPUT') return TEXT_INPUT.test((e.type || 'text').toLowerCase()) && !e.readOnly && !e.disabled;
    return false;
  }
  // Inputs Chrome renders as pickers or segmented editors: typed text
  // doesn't reach `.value`, so they are set by value, as Playwright's
  // `fill` does (NB-2). The date family must take the value as given.
  var SET_BY_VALUE = /^(date|time|datetime-local|month|week|color|range)$/;
  var EXACT_VALUE = /^(date|time|datetime-local|month|week)$/;
  var VALUE_FORMAT = {
    date: 'yyyy-mm-dd', time: 'hh:mm', 'datetime-local': 'yyyy-mm-ddThh:mm', month: 'yyyy-mm', week: 'yyyy-Www'
  };
  // Chrome normalises what it accepts (`2024-01-15T10:30:00` and
  // `2024-01-15 10:30` read back as `2024-01-15T10:30`), so only a value it
  // refused, which reads back empty, is invalid; the old value is restored
  // then, and the events fire whenever the value was taken (review M-3). A
  // colour that isn't #rrggbb and a range value that isn't a number were
  // silently set to black and the midpoint (N-6).
  function setByValue(e, text) {
    var type = (e.type || '').toLowerCase(), v = String(text).trim();
    var setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set;
    var old = e.value, expected = VALUE_FORMAT[type];
    var bad = (type === 'color' && !/^#[0-9a-fA-F]{6}$/.test(v)) ||
      (type === 'range' && (v === '' || isNaN(Number(v))));
    if (!bad) {
      setter.call(e, v);
      bad = EXACT_VALUE.test(type) && v !== '' && e.value === '';
      if (bad) setter.call(e, old);
    }
    if (bad) {
      if (type === 'color') expected = '#rrggbb';
      if (type === 'range') expected = 'a number';
      return { mode: 'set', editable: true, tag: 'input', error: 'not a valid value for <input type=' + type +
        '>: ' + JSON.stringify(v) + ' (expected ' + expected + ')' };
    }
    e.dispatchEvent(new Event('input', { bubbles: true, composed: true }));
    e.dispatchEvent(new Event('change', { bubbles: true }));
    return { mode: 'set', editable: true, tag: 'input' };
  }
  // Put the caret at the end of the focused field, so typed text is
  // appended: the click left it mid-value (VEL-131). Says what has focus
  // and whether it takes text (VEL-133). With `text`, a picker-style input
  // is set by value instead (`mode: 'set'`), and a field that has no
  // selection API (email, number) asks the driver to move the caret with a
  // key command (`moveEnd`, NB-7).
  function caretToEnd(text) {
    // In a frame whose document doesn't have the focus, `activeElement`
    // is only the element focused there last: the keys would go to
    // another frame (review-i2 m1).
    if (window.top !== window && !document.hasFocus()) {
      return { editable: false, tag: null, mode: 'type', unfocused: true };
    }
    var e = focused();
    var opaque = !!e && isFrame(e);
    // `editable: null`: focus inside a frame it can't read, so unknown.
    var info = { editable: opaque ? null : editable(e), tag: e ? e.tagName.toLowerCase() : null, mode: 'type' };
    if (!e || opaque) return info;
    if (text != null && e.tagName === 'INPUT' && SET_BY_VALUE.test((e.type || '').toLowerCase()) &&
      !e.readOnly && !e.disabled) {
      return setByValue(e, text);
    }
    if (typeof e.setSelectionRange === 'function' && typeof e.value === 'string') {
      try { var k = e.value.length; e.setSelectionRange(k, k); } catch (x) { info.moveEnd = true; }
    } else if (e.isContentEditable) {
      var d = e.ownerDocument, sel = d.getSelection(), rg = d.createRange();
      rg.selectNodeContents(e); rg.collapse(false); sel.removeAllRanges(); sel.addRange(rg);
    }
    return info;
  }

  // Whether the cross-origin frame that holds the focus is the target's
  // own: the click at (`x`, `y`, top-page px) landed on its <iframe> (a
  // card widget's container is clicked on the frame it holds). Focus that
  // stays in such a frame is where the user asked to type (review-i1-i3
  // M2).
  // Only where the click landed counts: a container whose mousedown is
  // cancelled, clicked beside its <iframe>, leaves the focus where an
  // earlier step put it, and typing would go there (review-fi2 F6). A
  // container clicked on its frame passes by the point.
  function focusWithin(id, x, y) {
    var n = byVid(id), f = focused();
    if (!n || !f || !isFrame(f)) return false;
    var r = f.getBoundingClientRect(), o = frameOffset(f);
    return x >= r.left + o[0] && x <= r.right + o[0] && y >= r.top + o[1] && y <= r.bottom + o[1];
  }

  // Whether the point (`x`, `y`, top-page px) is over an <iframe> of this
  // document: a click there went into the frame, and the page's own
  // `activeElement` follows that focus change a moment later.
  function frameAt(x, y) {
    var e = document.elementFromPoint(x, y);
    return !!e && isFrame(e);
  }

  // ── <select> (D-14) ───────────────────────────────────────────────────
  // `inputText` on a <select>, or on the <label> of one, chooses an option
  // instead of typing: typed text never reached a select and the step
  // passed (VEL-133). Playwright's `selectOption` rules: the option whose
  // value is the text, else whose label is, both sides normalised alike;
  // a disabled option (or one in a disabled <optgroup>) is refused; then
  // `input` and `change` fire, as a user's choice fires them.
  function selectOf(n) {
    if (!n) return null;
    if (n.tagName === 'SELECT') return n;
    var l = n.tagName === 'LABEL' ? n : (n.closest ? n.closest('label') : null);
    return l && l.control && l.control.tagName === 'SELECT' ? l.control : null;
  }
  function optionList(opts) {
    return opts.map(function (o) {
      var label = norm(o.label);
      return JSON.stringify(label) + (o.value !== label ? ' (value ' + JSON.stringify(o.value) + ')' : '') +
        (o.disabled ? ' (disabled)' : '');
    }).join(', ');
  }
  function selectOption(id, text) {
    var s = selectOf(byVid(id));
    if (!s) return null;
    var opts = Array.prototype.slice.call(s.options), want = norm(text);
    var hit = opts.filter(function (o) { return o.value === text; })[0] ||
      opts.filter(function (o) { return norm(o.label) === want; })[0];
    if (!hit) {
      return { error: 'the <select> has no option ' + JSON.stringify(text) + '; its options: ' + optionList(opts) };
    }
    var group = hit.parentNode && hit.parentNode.tagName === 'OPTGROUP' ? hit.parentNode : null;
    if (s.disabled) return { disabled: 'the <select> is disabled' };
    if (hit.disabled || (group && group.disabled)) {
      return { disabled: 'option ' + JSON.stringify(norm(hit.label)) + ' is disabled' };
    }
    if (s.multiple) opts.forEach(function (o) { o.selected = o === hit; });
    else hit.selected = true;
    try { s.focus({ preventScroll: true }); } catch (x) { /* not focusable */ }
    s.dispatchEvent(new Event('input', { bubbles: true, composed: true }));
    s.dispatchEvent(new Event('change', { bubbles: true }));
    return { ok: true, value: s.value };
  }

  // A tap on an <option> of a closed drop-down <select> (it has no box
  // until the native popup opens, which a page can't click into): choose
  // it, as `selectOption` does. null when `id` is not such an option
  // (review 10 #11).
  function tapOption(id) {
    var o = byVid(id);
    if (!o || o.tagName !== 'OPTION') return null;
    var s = o.closest ? o.closest('select') : null;
    if (!s) return null;
    var r = o.getBoundingClientRect();
    if (s.multiple || (s.size > 1 && r.width > 0 && r.height > 0)) return null;
    var group = o.parentNode && o.parentNode.tagName === 'OPTGROUP' ? o.parentNode : null;
    if (s.disabled) return { disabled: 'the <select> is disabled' };
    if (o.disabled || (group && group.disabled)) return { disabled: 'option ' + JSON.stringify(norm(o.label)) + ' is disabled' };
    o.selected = true;
    try { s.focus({ preventScroll: true }); } catch (x) { /* not focusable */ }
    s.dispatchEvent(new Event('input', { bubbles: true, composed: true }));
    s.dispatchEvent(new Event('change', { bubbles: true }));
    return { ok: true, value: s.value };
  }

  // `inputText` on an <input type=file> (or its <label>): the input, so
  // the driver can set its files (DOM.setFileInputFiles); typing never
  // reached it (review 10 #11).
  function fileInputOf(n) {
    if (!n) return null;
    var f = n;
    if (n.tagName === 'LABEL') f = n.control;
    else if (n.tagName !== 'INPUT' && n.closest) { var l = n.closest('label'); f = l ? l.control : null; }
    return f && f.tagName === 'INPUT' && (f.type || '').toLowerCase() === 'file' ? f : null;
  }
  function fileInput(id) {
    var f = fileInputOf(byVid(id));
    if (!f) return null;
    return { multiple: !!f.multiple, disabled: !!f.disabled, accept: f.getAttribute('accept') || null };
  }

  // ── Settle ────────────────────────────────────────────────────────────
  // Whether the page is still reacting to the last action: DOM mutations
  // (a MutationObserver) or a finite animation or transition running.
  // Endless ones (spinners) are ignored, or nothing would ever settle.
  function animating() {
    if (!document.getAnimations) return false;
    try {
      return document.getAnimations().some(function (a) {
        if (a.playState !== 'running' || !a.effect) return false;
        var end = a.effect.getComputedTiming().endTime;
        return isFinite(end);
      });
    } catch (x) { return false; }
  }
  // Attributes whose change can mean the screen changed. Every attribute
  // made any write (a framer-motion `transform`, a Lottie path's `d`, a
  // progress `value`) restart the quiet timer, and such pages ran every
  // action to the cap (M-3).
  var SETTLE_ATTRS = ['class', 'style', 'hidden', 'open', 'disabled', 'checked', 'selected', 'src',
    'href', 'aria-hidden', 'aria-expanded', 'aria-selected', 'aria-checked', 'aria-busy',
    'aria-disabled', 'aria-current', 'aria-pressed'];
  // A node that changed in this many consecutive ticks is animated by
  // script (a ticking counter, a JS-driven spinner or carousel): its
  // changes are not a reaction to the action. Remembered for one settle
  // only: kept for the life of the document, a container that churned at
  // load (`#root` while hydrating) had the first change of every later
  // settle ignored, so a route swap was never waited for (NB-9).
  var RESTLESS_TICKS = 4;
  // Open shadow roots observed besides the document, at most.
  var MAX_OBSERVED_ROOTS = 2000;
  // Resolves once the page has reacted and then been quiet for `quiet` ms,
  // or nothing happened within `reaction` ms, or `cap` ms passed (VEL-067).
  function settle(reaction, quiet, capMs) {
    return new Promise(function (resolve) {
      var start = performance.now(), last = start, reacted = false;
      // Targets changed since the last tick, and each target's run of
      // consecutive ticks with a change (a missed tick doesn't end a run:
      // ticks and frames don't line up exactly).
      var changed = new Set(), streak = new Map(), tickNo = 0, restless = new WeakSet();
      var opts = {
        subtree: true, childList: true, characterData: true,
        attributes: true, attributeFilter: SETTLE_ATTRS
      };
      // The document and every open shadow root, including ones added
      // while settling: a web component's render inside its shadow root
      // never reached an observer on the document (WA-7).
      var observed = new Set(), mo;
      function observeShadows(n) {
        if ((n.nodeType !== 1 && n.nodeType !== 11) || observed.size >= MAX_OBSERVED_ROOTS) return;
        var list = (n.nodeType === 1 ? [n] : []).concat(Array.prototype.slice.call(n.querySelectorAll('*')));
        for (var i = 0; i < list.length && observed.size < MAX_OBSERVED_ROOTS; i++) {
          var sr = list[i].shadowRoot;
          if (sr && !observed.has(sr)) { observed.add(sr); mo.observe(sr, opts); observeShadows(sr); }
        }
      }
      mo = new MutationObserver(function (records) {
        for (var i = 0; i < records.length; i++) {
          var t = records[i].target;
          if (t.nodeType !== 1) t = t.parentNode;
          if (t && t.nodeType === 11) t = t.host || t;
          if (t) changed.add(t);
          var added = records[i].addedNodes;
          for (var j = 0; added && j < added.length; j++) observeShadows(added[j]);
        }
      });
      mo.observe(document, opts);
      var rs = roots();
      for (var r = 0; r < rs.length && observed.size < MAX_OBSERVED_ROOTS; r++) {
        if (rs[r].nodeType === 11 && !observed.has(rs[r])) { observed.add(rs[r]); mo.observe(rs[r], opts); }
      }
      function finish(settled) {
        mo.disconnect();
        resolve({ settled: settled, reacted: reacted, elapsed: Math.round(performance.now() - start) });
      }
      function absorb(now) {
        tickNo++;
        changed.forEach(function (t) {
          var s = streak.get(t), going = s && tickNo - s.tick <= 2;
          var run = going ? s.run + 1 : 1;
          streak.set(t, { tick: tickNo, run: run });
          // Known to animate: ignored while it keeps changing; a pause
          // then a change is a real one again.
          if (restless.has(t)) {
            if (!s || going) return;
            restless.delete(t);
          }
          if (run >= RESTLESS_TICKS) restless.add(t);
          else { last = now; reacted = true; }
        });
        changed.clear();
      }
      (function tick() {
        var now = performance.now();
        absorb(now);
        if (animating()) { last = now; reacted = true; }
        if (now - start >= capMs) return finish(false);
        if (reacted ? now - last >= quiet : now - start >= reaction) {
          // Something changed: one more frame, so it is laid out and
          // painted. Nothing did: done now, a static page costs nothing more.
          return reacted ? nextFrame().then(function () { finish(true); }) : finish(true);
        }
        // Before the first change, wake for the end of the reaction window
        // itself rather than up to a frame after it.
        setTimeout(tick, reacted ? 16 : Math.max(1, Math.min(16, reaction - (now - start))));
      })();
    });
  }

  // ── Swipes ────────────────────────────────────────────────────────────
  // Whether the element under (x, y) sits in something a stroke along that
  // axis scrolls (an overflow ancestor, or the page), before reaching a
  // widget that takes the pointer itself: a range input, or a touch-action
  // that doesn't pan that way (Swiper, sliders, canvas drawing).
  function scrollsAlong(x, y, vertical) {
    var n = document.elementFromPoint(x, y);
    var root = document.scrollingElement || document.documentElement;
    for (; n && n.nodeType === 1; n = n.parentElement) {
      var cs = getComputedStyle(n);
      if (n.tagName === 'INPUT' && n.type === 'range') return false;
      var ta = cs.touchAction;
      if (ta === 'none' || ta === 'pinch-zoom' ||
        (vertical && /^pan-x|^pan-left|^pan-right/.test(ta) && !/pan-y|pan-up|pan-down/.test(ta)) ||
        (!vertical && /^pan-y|^pan-up|^pan-down/.test(ta) && !/pan-x|pan-left|pan-right/.test(ta))) return false;
      var overflow = vertical ? cs.overflowY : cs.overflowX;
      var room = vertical ? n.scrollHeight > n.clientHeight : n.scrollWidth > n.clientWidth;
      if (n === root || n === document.body) {
        if (room && overflow !== 'hidden' && overflow !== 'clip') return true;
      } else if (room && /(auto|scroll|overlay)/.test(overflow)) {
        return true;
      }
    }
    return false;
  }

  // ── Screenshots for comparison ──────────────────────────────────────
  // A capture compared against a baseline must not depend on when it was
  // taken: running animations and transitions are paused at a fixed time
  // (a finite one just before its end, an endless one at its start), new
  // ones caught as they start, and the text caret is hidden (Playwright's
  // screenshotter, VT-8). Unlike `finish()`, a pause fires no
  // `animationend`/`transitionend`, so a toast dismissed by one survives
  // the capture, and each animation goes back to its own time and state
  // afterwards (review M-12). A thaw the driver never sends happens by
  // itself after 10 s, so the page isn't left frozen (N-3).
  var frozen = null;
  function freeze() {
    if (frozen) { frozen.stop(); return true; }
    var st = { held: [], style: document.createElement('style') };
    st.style.textContent = '*,*::before,*::after{caret-color:transparent !important}';
    (document.head || document.documentElement).appendChild(st.style);
    st.stop = function () {
      if (!document.getAnimations) return;
      document.getAnimations().forEach(function (a) {
        if (!a.effect || a.playbackRate === 0 || a.playState === 'finished') return;
        for (var i = 0; i < st.held.length; i++) if (st.held[i].a === a) return;
        try {
          var end = a.effect.getComputedTiming().endTime, was = a.currentTime;
          if (was === null) return;
          st.held.push({ a: a, time: was, running: a.playState === 'running' });
          a.pause();
          a.currentTime = isFinite(end) ? Math.max(0, end - 1) : 0;
        } catch (x) { /* already gone */ }
      });
    };
    st.stop();
    document.addEventListener('transitionrun', st.stop, true);
    document.addEventListener('animationstart', st.stop, true);
    st.timer = setTimeout(unfreeze, 10000);
    frozen = st;
    return true;
  }
  function unfreeze() {
    var st = frozen;
    frozen = null;
    if (!st) return false;
    clearTimeout(st.timer);
    document.removeEventListener('transitionrun', st.stop, true);
    document.removeEventListener('animationstart', st.stop, true);
    if (st.style.parentNode) st.style.parentNode.removeChild(st.style);
    st.held.forEach(function (h) {
      try {
        h.a.currentTime = h.time;
        if (h.running) h.a.play();
      } catch (x) { /* gone */ }
    });
    return true;
  }

  // ── Flutter web ───────────────────────────────────────────────────────
  // A Flutter web app builds its semantics tree (and so every
  // `flt-semantics-identifier` and label) only once accessibility is on;
  // its own "Enable accessibility" placeholder turns it on when clicked,
  // which is what Maestro's `ensureSemantics` asks for. Clicked once per
  // document; true when it was there. Flutter adds the placeholder once its
  // engine has started, usually after the load event, so when it isn't
  // there yet a watch looks again every 250 ms for up to 30 s, scanning
  // only once the page shows a Flutter element (review M-8).
  var flutterDone = false, flutterWatch = null;
  var FLUTTER_MARK = 'flutter-view, flt-glass-pane, flt-semantics-placeholder, flt-scene-host';
  function clickPlaceholder() {
    var p = null, rs = roots();
    for (var i = 0; i < rs.length && !p; i++) p = rs[i].querySelector('flt-semantics-placeholder');
    if (!p) return false;
    flutterDone = true;
    try { p.click(); } catch (x) { /* nothing to enable */ }
    return true;
  }
  function flutterSemantics() {
    if (flutterDone) return false;
    if (clickPlaceholder()) return true;
    if (!flutterWatch) {
      var until = Date.now() + 30000;
      flutterWatch = setInterval(function () {
        var flutter = window._flutter || document.querySelector(FLUTTER_MARK);
        if (flutterDone || Date.now() > until || (flutter && clickPlaceholder())) clearInterval(flutterWatch);
      }, 250);
    }
    return false;
  }

  var api = {
    // The pick, with `n`: how many matches are on screen (D-1's `shown`),
    // counted in the same call for the executor's ambiguity check (R5),
    // so it needs no second lookup (review MAJOR 8).
    find: function (sel) {
      return guarded(function () {
        var l = matchTiered(sel), c = cap(pick(l));
        if (c) c.n = countShown(l);
        return c;
      });
    },
    findAll: function (sel, lim) {
      return guarded(function () { return matchTiered(sel).map(function (n) { return cap(n, lim); }); });
    },
    walk: walk,
    // `idMatches` (D-11): every element with an id, a data-testid or a
    // Flutter identifier, and those values; the regex runs in Rust.
    idsOf: function () {
      return qall('[id], [data-testid], [flt-semantics-identifier]').map(function (n) {
        return {
          v: vid(n),
          ids: [n.id, n.getAttribute('data-testid'), n.getAttribute('flt-semantics-identifier')]
            .filter(function (x) { return x; })
        };
      });
    },
    // The live state of a resolved element, null once it left the page.
    // `opts`: {page, minArea}, see `shown`.
    state: function (id, opts) {
      var n = byVid(id);
      if (!n) return null;
      return { visible: shown(n, opts), text: textOf(n) };
    },
    // assertText: whether the element's text is `want`, both normalised
    // alike (whitespace incl. NBSP / U+202F, zero-width characters, NFC),
    // as rendered or as written (text-transform); null once it left the
    // page. `norm` is `want` normalised, for a detached element's capture.
    textIs: function (id, want) {
      var w = norm(want), n = byVid(id);
      if (!n) return { gone: true, norm: w };
      return { text: textOf(n), match: textIs(n, w, false), norm: w };
    },
    act: act,
    arm: arm,
    disarm: disarm,
    reveal: reveal,
    freeze: freeze,
    unfreeze: unfreeze,
    flutterSemantics: flutterSemantics,
    caretToEnd: caretToEnd,
    focusWithin: focusWithin,
    frameAt: frameAt,
    isSelect: function (id) { return !!selectOf(byVid(id)); },
    tapOption: tapOption,
    fileInput: fileInput,
    fileInputElement: function (id) { return fileInputOf(byVid(id)); },
    selectOption: selectOption,
    settle: settle,
    scrollsAlong: scrollsAlong,
    // `url` resolved against the page, as the browser will store it.
    resolveUrl: function (url) { try { return new URL(url, location.href).href; } catch (x) { return null; } },
    // How many elements the registry still holds (tests, MINOR-8). Not
    // `deref`: that keeps its target alive until the job ends.
    liveIds: function () { return nodes.size; }
  };
  Object.defineProperty(globalThis, '__velocity', { value: api });
})();
