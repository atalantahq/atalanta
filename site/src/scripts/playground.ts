// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

import * as monaco from 'monaco-editor/esm/vs/editor/editor.main.js';
import { configureMonacoYaml } from 'monaco-yaml';
import editorWorker from 'monaco-editor/esm/vs/editor/editor.worker.js?worker';
import yamlWorker from 'monaco-yaml/yaml.worker?worker';
import { url } from '../lib/url';

self.MonacoEnvironment = {
  getWorker: (_moduleId, label) => (label === 'yaml' ? new yamlWorker() : new editorWorker()),
};

const MODEL_URI = monaco.Uri.parse('inmemory://playground/atalanta.yaml');
const SCHEMA_URI = new URL(url('/schema/suite.json'), location.href).href;
const FILE = 'atalanta.yaml';
const SITE = 'https://atalanta.sh';
const WAIT_CAP_MS = 10000;
const NETWORK_BLOCKED = 'no response (network, or the API does not allow cross-origin requests); `atalanta run` has no such limit';
const reduced = matchMedia('(prefers-reduced-motion: reduce)').matches;
const PACE = reduced ? 150 : 650;

const $ = (id: string) => document.getElementById(id) as HTMLElement;
const examplesEl = $('playground-examples') as HTMLSelectElement;
const runBtn = $('playground-run') as HTMLButtonElement;
const runBtns = [runBtn, $('playground-run-m') as HTMLButtonElement];
const setRunLabel = (t: string, on: boolean) => runBtns.forEach((b) => { b.textContent = t; b.classList.toggle('is-running', on); });
const statusEl = $('playground-status');
const term = $('playground-panel-terminal');
const stageEl = $('playground-stage');
const problemsList = $('playground-problems-list');
const problemsEmpty = $('playground-problems-empty');
const tabBadge = $('playground-tab-badge');
const tabTerm = $('playground-tab-terminal');
const tabProblems = $('playground-tab-problems');
const examples = JSON.parse($('playground-examples-json').textContent || '[]');

let editor: any;
let model: any;
let wasm: any;
let plan: any = null;
let running = false;
let stopRequested = false;
let decorations: string[] = [];
let stage: Stage | null = null;
let stageKey = '';

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const h = (tag: string, cls?: string, text?: string) => {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
};

function interp(s: string, vars: Record<string, string>) {
  return String(s ?? '').replace(/\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-([^}]*))?\}/g, (m, n, d) => vars[n] ?? d ?? m);
}

function actionOf(step: any): [string, any] {
  const a = step.action;
  if (typeof a === 'string') return [a, {}];
  if (!a || typeof a !== 'object') return ['', {}];
  const k = Object.keys(a)[0];
  return [k, a[k] ?? {}];
}

function selLabel(sel: any, vars: Record<string, string>): string {
  if (!sel) return '';
  if (sel.index) return selLabel(sel.index.selector, vars);
  if (sel.relative) return selLabel(sel.relative.target, vars);
  if (Array.isArray(sel.compound)) return selLabel(sel.compound.find((c: any) => typeof Object.values(c)[0] !== 'boolean'), vars);
  const v = sel.text ?? sel.textContains ?? sel.textMatches ?? sel.accessibilityId ?? sel.name ?? sel.id ?? sel.idMatches ?? sel.role ?? sel.css ?? sel.xPath ?? sel.className;
  return interp(v != null ? String(v) : JSON.stringify(sel), vars);
}

const clip = (s: string, n = 72) => (s.length > n ? s.slice(0, n - 1) + '…' : s);

const selKey = (sel: any, vars: Record<string, string>) => selLabel(sel, vars).toLowerCase();

// --- terminal --------------------------------------------------------------

type Part = string | [string, string];

function line(...parts: Part[]) {
  const row = h('div', 't-line');
  for (const p of parts) {
    if (typeof p === 'string') row.append(p);
    else row.append(h('span', p[1], p[0]));
  }
  term.append(row);
  term.scrollTop = term.scrollHeight;
  return row;
}

const arrow = (...parts: Part[]) => line(['=>', 't-cyan t-b'], ' ', ...parts);

function termIdle() {
  term.textContent = '';
  line(['$ ', 't-dim'], `atalanta run ${FILE}`, ['█', 't-cursor']);
  line(['# Run, or Ctrl/⌘+Enter in the editor', 't-dim']);
}

function fmtSecs(ms: number) { return (ms / 1000).toFixed(1) + 's'; }
function fmtMs(ms: number) { return ms < 1000 ? Math.round(ms) + 'ms' : fmtSecs(ms); }

// --- validation and problems ----------------------------------------------

async function loadValidator() {
  try {
    const m = await import(/* @vite-ignore */ url('/playground/wasm/atalanta_wasm.js'));
    await m.default();
    wasm = m;
    statusEl.textContent = `Validator ${m.version()}. API steps send real requests from your browser; device and web steps play on a mock screen. Nothing else leaves the page.`;
    statusEl.className = 'playground-status';
  } catch {
    statusEl.textContent = 'Schema checks only: the validator isn’t in this build, so Run is off.';
  }
}

function validate() {
  if (!wasm || !model) return;
  let diags: any[] = [];
  try { diags = JSON.parse(wasm.validate(model.getValue())); } catch { return; }
  const last = model.getLineCount();
  monaco.editor.setModelMarkers(model, 'atalanta', diags.map((d) => {
    const ln = Math.min(Math.max(d.line || 1, 1), last);
    return {
      severity: d.severity === 'warning' ? monaco.MarkerSeverity.Warning : monaco.MarkerSeverity.Error,
      startLineNumber: ln, startColumn: 1, endLineNumber: ln, endColumn: model.getLineMaxColumn(ln),
      message: d.message || '', source: 'atalanta',
    };
  }));
}

function refreshProblems() {
  const all = monaco.editor.getModelMarkers({ resource: model.uri })
    .sort((a, b) => a.startLineNumber - b.startLineNumber || a.startColumn - b.startColumn);
  problemsList.textContent = '';
  problemsEmpty.hidden = all.length > 0;
  tabBadge.textContent = String(all.length);
  tabBadge.classList.toggle('has', all.length > 0);
  for (const m of all) {
    const b = h('button', 'playground-problem');
    (b as HTMLButtonElement).type = 'button';
    b.append(h('span', 'line', `Line ${m.startLineNumber}:`), ' ', m.message || '(no message)');
    b.addEventListener('click', () => {
      editor.focus();
      editor.setPosition({ lineNumber: m.startLineNumber, column: Math.max(1, m.startColumn) });
      editor.revealLineInCenter(m.startLineNumber);
    });
    const li = h('li');
    li.append(b);
    problemsList.append(li);
  }
}

function makePlan(text: string) {
  if (!wasm) return null;
  try { return JSON.parse(wasm.plan(text)); } catch { return null; }
}

function kindOf(p: any): 'api' | 'ios' | 'android' | 'web' {
  return p?.target?.kind || 'ios';
}

function refreshStage() {
  if (running) return;
  plan = makePlan(model.getValue());
  runBtns.forEach((b) => { b.disabled = !wasm; });
  if (!plan || !plan.ok) return;
  const kind = kindOf(plan);
  const key = kind + '|' + (plan.appId || '') + '|' + (plan.target.baseUrl || '');
  if (key !== stageKey || !stage) {
    stageKey = key;
    stage = kind === 'api' ? new ApiStage(plan) : kind === 'web' ? new BrowserStage(plan) : new PhoneStage(kind, plan);
    stageEl.textContent = '';
    stageEl.append(stage.el);
  }
  stage.idle(plan);
}

// --- editor line highlight -------------------------------------------------

function highlight(ln?: number) {
  decorations = editor.deltaDecorations(decorations, ln ? [{
    range: new monaco.Range(ln, 1, ln, 1),
    options: { isWholeLine: true, className: 'playground-line-running', glyphMarginClassName: 'playground-line-glyph' },
  }] : []);
  if (ln) editor.revealLineInCenterIfOutsideViewport(ln);
}

// --- http ------------------------------------------------------------------

function fetchUrl(u: string) {
  // Off atalanta.sh (a local build or a preview) the same files come from here.
  if (location.hostname !== 'atalanta.sh' && (u === SITE || u.startsWith(SITE + '/'))) {
    return new URL(url(u.slice(SITE.length) || '/'), location.origin).href;
  }
  return u;
}

type Outcome = { status: 'pass' | 'fail' | 'skip'; failures: string[]; captured?: Record<string, string>; exchange?: Exchange };
type Exchange = { method: string; url: string; status?: number; statusText?: string; ms?: number; headers?: Record<string, string>; body?: string };

async function runWasmStep(text: string, ti: number, step: any, vars: Record<string, string>): Promise<Outcome> {
  const prep = JSON.parse(wasm.prepare_step(text, ti, step.index, JSON.stringify(vars)));
  if (prep.kind === 'error') return { status: 'fail', failures: [prep.message] };
  if (prep.kind === 'skip') return { status: 'skip', failures: [] };
  if (prep.kind === 'wait') { await sleep(Math.min(prep.ms, WAIT_CAP_MS)); return { status: 'pass', failures: [] }; }
  if (prep.kind !== 'http') return { status: 'fail', failures: ['unknown step kind'] };
  const req = prep.request;
  const attempts = (prep.retry ? prep.retry.count : 0) + 1;
  let last: Outcome = { status: 'fail', failures: [] };
  for (let n = 0; n < attempts; n++) {
    if (n > 0) await sleep(prep.retry.intervalMs);
    const ex: Exchange = { method: req.method, url: req.url };
    const ac = new AbortController();
    const timer = setTimeout(() => ac.abort(), req.timeoutMs);
    let res: Response;
    const t0 = performance.now();
    try {
      res = await fetch(fetchUrl(req.url), { method: req.method, headers: req.headers, body: req.body, signal: ac.signal });
      ex.body = await res.text();
    } catch (e: any) {
      last = { status: 'fail', failures: [e?.name === 'AbortError' ? `no response within ${req.timeoutMs}ms` : NETWORK_BLOCKED], exchange: ex };
      continue;
    } finally { clearTimeout(timer); }
    ex.ms = Math.round(performance.now() - t0);
    ex.status = res.status;
    ex.statusText = res.statusText;
    ex.headers = Object.fromEntries(res.headers.entries());
    const response = { status: ex.status, headers: ex.headers, body: ex.body, elapsedMs: ex.ms };
    const check = JSON.parse(wasm.check_step(text, ti, step.index, JSON.stringify(vars), JSON.stringify(response)));
    if (check.passed) return { status: 'pass', failures: [], captured: check.captured || {}, exchange: ex };
    last = { status: 'fail', failures: check.failures || [], exchange: ex };
  }
  return last;
}

// --- stages ----------------------------------------------------------------

type Ctx = { text: string; ti: number; steps: any[]; i: number; vars: Record<string, string> };

interface Stage {
  el: HTMLElement;
  mock: boolean;
  idle(plan: any): void;
  beginTest(ti: number): Promise<void>;
  step(step: any, ctx: Ctx): Promise<Outcome>;
  exchange?(ti: number, step: any, ex: Exchange, ok: boolean): void;
  live?(text: string, state: 'idle' | 'run' | 'pass' | 'fail'): void;
}

const NAV_TAPS = new Set(['tap', 'doubleTap', 'longPress']);
const NAV_STOPS = new Set(['launchApp', 'stopApp', 'killApp', 'goto', 'pressKey', 'back', 'openLink', 'clearState']);

type Target = { key: string; label: string; field?: boolean; far?: boolean };

function lookahead(steps: any[], from: number, vars: Record<string, string>): Target[] {
  const out: Target[] = [];
  const add = (t: Target) => { if (t.key && !out.some((o) => o.key === t.key)) out.push(t); };
  for (let j = from; j < steps.length; j++) {
    const [k, v] = actionOf(steps[j]);
    if (NAV_STOPS.has(k)) break;
    if (k === 'assertNotVisible' || k === 'http') continue;
    const sel = v.selector;
    if (sel) {
      const label = k === 'assertText' && v.expected != null ? interp(v.expected, vars) : selLabel(sel, vars);
      add({ key: selKey(sel, vars), label, field: k === 'inputText' || k === 'eraseText', far: k === 'scrollUntilVisible' });
    }
    if (NAV_TAPS.has(k)) break;
  }
  return out;
}

const svg = (body: string, vb = '0 0 24 24') =>
  `<svg viewBox="${vb}" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${body}</svg>`;

const GLYPHS: Record<string, string> = {
  Clock: '<circle cx="12" cy="12" r="8.5"/><path d="M12 7.5V12l3 2"/>',
  Notes: '<rect x="5" y="4" width="14" height="16" rx="3"/><path d="M8.5 9h7M8.5 12.5h7M8.5 16h4"/>',
  Camera: '<path d="M4 8.5A2.5 2.5 0 0 1 6.5 6h1.8l1.4-2h4.6l1.4 2h1.8A2.5 2.5 0 0 1 20 8.5v8a2.5 2.5 0 0 1-2.5 2.5h-11A2.5 2.5 0 0 1 4 16.5Z"/><circle cx="12" cy="12.5" r="3.3"/>',
  Mail: '<rect x="3.5" y="5.5" width="17" height="13" rx="2.5"/><path d="m4.5 7 7.5 6 7.5-6"/>',
  Maps: '<path d="M9 4 3.5 6v14L9 18l6 2 5.5-2V4L15 6Z"/><path d="M9 4v14M15 6v14"/>',
  Music: '<path d="M9 17.5V6l10-2v11.5"/><circle cx="6.5" cy="17.5" r="2.5"/><circle cx="16.5" cy="15.5" r="2.5"/>',
  Photos: '<rect x="4" y="4" width="16" height="16" rx="3.5"/><circle cx="9" cy="9.5" r="1.6"/><path d="m4.5 17 5-5 4 4 2.5-2.5 3.5 3.5"/>',
  Weather: '<path d="M7 18a4 4 0 0 1-.6-7.96A5.5 5.5 0 0 1 17 9.5a3.5 3.5 0 0 1 .5 6.97V18Z"/>',
  Calendar: '<rect x="4" y="5" width="16" height="15" rx="3"/><path d="M4 10h16M9 3v4M15 3v4"/>',
  Phone: '<path d="M6.5 4h3l1.5 4-2 1.5a10 10 0 0 0 5.5 5.5l1.5-2 4 1.5v3a2 2 0 0 1-2 2A15 15 0 0 1 4.5 6a2 2 0 0 1 2-2Z"/>',
  Messages: '<path d="M4 11.5C4 7.4 7.6 4.5 12 4.5s8 2.9 8 7-3.6 7-8 7c-1 0-2-.15-2.9-.43L5 19.5l1.2-3.3A6.3 6.3 0 0 1 4 11.5Z"/>',
  Browser: '<circle cx="12" cy="12" r="8.5"/><path d="M3.5 12h17M12 3.5c2.5 2.6 2.5 14.4 0 17M12 3.5c-2.5 2.6-2.5 14.4 0 17"/>',
  Gear: '<circle cx="12" cy="12" r="3"/><path d="M12 2.8v2.4M12 18.8v2.4M2.8 12h2.4M18.8 12h2.4M5.5 5.5l1.7 1.7M16.8 16.8l1.7 1.7M5.5 18.5l1.7-1.7M16.8 7.2l1.7-1.7"/><circle cx="12" cy="12" r="6.3"/>',
  App: '<rect x="5" y="5" width="14" height="14" rx="4"/><path d="M9 12h6M12 9v6"/>',
};
const glyph = (name: string) => svg(GLYPHS[name] || GLYPHS.App);
const ICON_CHEV = svg('<path d="m9.5 6 6 6-6 6"/>');
const ICON_BACK = svg('<path d="m14.5 6-6 6 6 6"/>');
const ICON_CHECK = svg('<path d="m5.5 12.5 4 4 9-9"/>');
const ICON_LOCK = svg('<rect x="5.5" y="10.5" width="13" height="9.5" rx="2.5"/><path d="M8.5 10.5V8a3.5 3.5 0 0 1 7 0v2.5"/>');
const ICON_RELOAD = svg('<path d="M19 12a7 7 0 1 1-2.05-4.95M19 4.5V8h-3.5"/>');
const CURSOR = '<svg viewBox="0 0 20 24" aria-hidden="true"><path d="M2 1.5v18.2l5-4.6 3.2 7.2 3.1-1.4-3.2-7H17Z" fill="#111" stroke="#fff" stroke-width="1.4" stroke-linejoin="round"/></svg>';

const ROW_GLYPHS = ['Gear', 'Browser', 'Photos', 'Clock', 'Notes', 'Calendar', 'Mail', 'Maps', 'Music', 'Messages'];
const hash = (s: string) => [...s].reduce((a, c) => (a * 31 + c.charCodeAt(0)) >>> 0, 7);
const TINTS = ['#4f7cff', '#2fb2c6', '#33b66b', '#f29a2e', '#ef5a4f', '#7a6cf2', '#b061e8', '#7d8794', '#e8507a', '#3aa3e8'];

class Keyboard {
  el = h('div', 'pg-kbd');
  keys = new Map<string, HTMLElement>();
  constructor() {
    for (const row of ['qwertyuiop', 'asdfghjkl', 'zxcvbnm']) {
      const r = h('div', 'pg-kbd-row');
      if (row[0] === 'z') r.append(h('span', 'pg-key pg-key-mod', '⇧'));
      for (const c of row) {
        const k = h('span', 'pg-key', c);
        this.keys.set(c, k);
        r.append(k);
      }
      if (row[0] === 'z') r.append(h('span', 'pg-key pg-key-mod', '⌫'));
      this.el.append(r);
    }
    const last = h('div', 'pg-kbd-row');
    const space = h('span', 'pg-key pg-key-space', 'space');
    this.keys.set(' ', space);
    last.append(h('span', 'pg-key pg-key-mod pg-key-wide', '123'), space, h('span', 'pg-key pg-key-mod pg-key-wide', 'return'));
    this.el.append(last);
  }
  show(on: boolean) { this.el.classList.toggle('pg-kbd-on', on); }
  press(ch: string) {
    const k = this.keys.get(ch.toLowerCase()) || this.keys.get(' ');
    if (!k || (ch !== ' ' && !this.keys.has(ch.toLowerCase()))) return;
    k.classList.remove('pg-key-down');
    k.getBoundingClientRect();
    k.classList.add('pg-key-down');
  }
}

/** A stack of mock screens drawn from the suite's own selectors. */
class MockScreens {
  views: { el: HTMLElement; scroll: HTMLElement; title: string }[] = [];
  keyboard: Keyboard | null = null;
  pointer: HTMLElement | null = null;
  constructor(public host: HTMLElement, public skin: 'ios' | 'android' | 'web', public touch: HTMLElement) {}

  clear() { this.views = []; this.host.textContent = ''; }
  get top() { return this.views[this.views.length - 1]; }

  row(t: Target) {
    const r = h('div', 'pg-row' + (t.field ? ' pg-field' : ''));
    r.dataset.key = t.key;
    if (t.field) {
      r.append(h('span', 'pg-field-label', t.label), h('span', 'pg-field-value'));
      return r;
    }
    if (this.skin !== 'web') {
      const ico = h('span', 'pg-ico');
      const n = hash(t.label);
      ico.style.setProperty('--tint', TINTS[n % TINTS.length]);
      ico.innerHTML = glyph(ROW_GLYPHS[n % ROW_GLYPHS.length]);
      r.append(ico);
    }
    r.append(h('span', 'pg-row-label', t.label));
    if (this.skin !== 'web') {
      const c = h('span', 'pg-chev');
      c.innerHTML = ICON_CHEV;
      r.append(c);
    }
    return r;
  }

  filler(n: number) {
    const r = h('div', 'pg-row pg-filler');
    if (this.skin !== 'web') r.append(h('span', 'pg-ico'));
    const bar = h('span', 'pg-bar');
    bar.style.width = 30 + ((n * 37) % 42) + '%';
    r.append(bar);
    return r;
  }

  async push(title: string, targets: Target[], root: boolean) {
    const el = h('div', 'pg-view');
    const nav = h('div', 'pg-nav' + (root ? ' pg-nav-root' : ''));
    if (this.skin === 'web') {
      const brand = h('span', 'pg-site');
      brand.append(h('span', 'pg-site-logo'), h('span', 'pg-site-name', title));
      const links = h('span', 'pg-site-links');
      links.append(h('i'), h('i'), h('i'));
      nav.append(brand, links);
    } else {
      if (!root) {
        const b = h('span', 'pg-back');
        b.innerHTML = ICON_BACK;
        if (this.skin === 'ios') b.append(h('span', '', this.top?.title || 'Back'));
        nav.append(b);
      }
      nav.append(h('span', 'pg-title', title));
    }
    const scroll = h('div', 'pg-scroll');
    const group = h('div', 'pg-group');
    let n = this.views.length * 3 + title.length;
    const lead = root ? 2 : 1;
    for (let i = 0; i < lead; i++) group.append(this.filler(n++));
    for (const t of targets) {
      if (t.far) for (let i = 0; i < 9; i++) group.append(this.filler(n++));
      group.append(this.row(t));
      group.append(this.filler(n++));
    }
    group.append(this.filler(n++));
    scroll.append(group);
    el.append(nav, scroll);
    const prev = this.top;
    this.views.push({ el, scroll, title });
    this.host.append(el);
    if (prev && !reduced) {
      el.classList.add('pg-in');
      el.getBoundingClientRect();
      el.classList.remove('pg-in');
      prev.el.classList.add('pg-under');
      await sleep(380);
    } else if (prev) prev.el.classList.add('pg-under');
  }

  async pop() {
    const v = this.views.pop();
    if (!v) return false;
    const prev = this.top;
    if (prev) prev.el.classList.remove('pg-under');
    if (!reduced) { v.el.classList.add('pg-in'); await sleep(380); }
    v.el.remove();
    return true;
  }

  find(key: string) {
    return (this.top?.el.querySelector(`.pg-row[data-key="${CSS.escape(key)}"]`) as HTMLElement) || null;
  }

  ensure(t: Target) {
    let r = this.find(t.key);
    if (!r && this.top) {
      r = this.row(t);
      const group = this.top.el.querySelector('.pg-group')!;
      group.insertBefore(r, group.children[1] || null);
    }
    return r;
  }

  async reveal(r: HTMLElement, smooth = false) {
    const sc = this.top.scroll;
    const top = r.offsetTop - sc.clientHeight / 2 + r.offsetHeight / 2;
    if (Math.abs(sc.scrollTop - top) < 4) return;
    sc.scrollTo({ top: Math.max(0, top), behavior: smooth && !reduced ? 'smooth' : 'auto' });
    if (smooth) await sleep(reduced ? 50 : 600);
  }

  async touchAt(x: number, y: number) {
    if (this.pointer) {
      this.pointer.style.transform = `translate(${x}px, ${y}px)`;
      this.pointer.classList.add('pg-pointer-on');
      await sleep(reduced ? 0 : 480);
    }
    const d = h('span', this.pointer ? 'pg-dot pg-click' : 'pg-dot');
    d.style.left = x + 'px';
    d.style.top = y + 'px';
    this.touch.append(d);
    setTimeout(() => d.remove(), 800);
  }

  centre(r: HTMLElement, fx = 0.42) {
    const s = this.touch.getBoundingClientRect();
    const b = r.getBoundingClientRect();
    return [b.left - s.left + b.width * fx, b.top - s.top + b.height / 2];
  }

  async tapRow(r: HTMLElement) {
    const [x, y] = this.centre(r, this.pointer ? 0.25 : 0.42);
    await this.touchAt(x, y);
    r.classList.add('pg-pressed');
    setTimeout(() => r.classList.remove('pg-pressed'), 450);
  }

  mark(r: HTMLElement) {
    r.classList.remove('pg-ok');
    r.getBoundingClientRect();
    r.classList.add('pg-ok');
    if (!r.querySelector('.pg-okmark')) {
      const m = h('span', 'pg-okmark');
      m.innerHTML = ICON_CHECK;
      r.append(m);
    }
  }

  /** Steps every skin plays the same way. */
  async play(k: string, v: any, ctx: Ctx, toast: (s: string) => void): Promise<Outcome> {
    const sel = v.selector;
    const pass: Outcome = { status: 'pass', failures: [] };
    if (!this.top) return { status: 'fail', failures: ['nothing is open: start the test with launchApp'] };
    if (k === 'assertNotVisible') {
      if (this.find(selKey(sel, ctx.vars))) return { status: 'fail', failures: [`"${selLabel(sel, ctx.vars)}" is still visible`] };
      toast(`✓ "${selLabel(sel, ctx.vars)}" not on screen`);
      await sleep(PACE);
      return pass;
    }
    if (sel) {
      const label = k === 'assertText' && v.expected != null ? interp(v.expected, ctx.vars) : selLabel(sel, ctx.vars);
      const t: Target = { key: selKey(sel, ctx.vars), label, field: k === 'inputText' || k === 'eraseText' };
      const r = this.ensure(t);
      await this.reveal(r, k === 'scrollUntilVisible');
      if (NAV_TAPS.has(k)) {
        await this.tapRow(r);
        await sleep(PACE * 0.5);
        const next = lookahead(ctx.steps, ctx.i + 1, ctx.vars);
        if (next.length) await this.push(label, next, false);
        else await sleep(PACE * 0.4);
        return pass;
      }
      if (k === 'inputText') return this.type(r, interp(v.text ?? '', ctx.vars));
      if (k === 'eraseText') { r.querySelector('.pg-field-value')!.textContent = ''; await sleep(PACE); return pass; }
      this.mark(r);
      await sleep(PACE);
      return pass;
    }
    if (k === 'inputText') {
      const r = (this.top.el.querySelector('.pg-field') as HTMLElement) || this.ensure({ key: '__field', label: 'Text', field: true });
      return this.type(r, interp(v.text ?? '', ctx.vars));
    }
    if (k === 'hideKeyboard') { this.keyboard?.show(false); await sleep(PACE * 0.5); return pass; }
    if (k === 'swipe' || k === 'scroll') {
      this.top.scroll.scrollBy({ top: 160, behavior: reduced ? 'auto' : 'smooth' });
      await sleep(PACE);
      return pass;
    }
    if (k === 'screenshot' || k === 'takeScreenshot' || k === 'assertScreenshot') {
      this.host.classList.add('pg-flash');
      setTimeout(() => this.host.classList.remove('pg-flash'), 350);
      toast(v.filename ? `screenshot · ${v.filename}` : 'screenshot');
      await sleep(PACE);
      return pass;
    }
    toast(k || 'step');
    await sleep(PACE);
    return pass;
  }

  async type(r: HTMLElement, text: string): Promise<Outcome> {
    await this.tapRow(r);
    const out = r.querySelector('.pg-field-value')!;
    r.classList.add('pg-focus');
    this.keyboard?.show(true);
    if (this.keyboard && !reduced) await sleep(320);
    for (const ch of text) {
      this.keyboard?.press(ch);
      out.textContent += ch;
      await sleep(reduced ? 0 : Math.max(45, Math.min(110, 1400 / text.length)));
    }
    await sleep(PACE * 0.5);
    r.classList.remove('pg-focus');
    this.keyboard?.show(false);
    return { status: 'pass', failures: [] };
  }
}

function toaster(host: HTMLElement) {
  return (text: string) => {
    host.querySelector('.pg-toast')?.remove();
    const t = h('div', 'pg-toast', text);
    host.append(t);
    setTimeout(() => t.remove(), 1500);
  };
}

/** The pill under a device that names the step being played. */
function liveChip() {
  const el = h('div', 'pg-live');
  const dot = h('span', 'pg-live-dot');
  const text = h('span', 'pg-live-text', 'Ready · press Run');
  el.append(dot, text);
  return {
    el,
    set(t: string, state: 'idle' | 'run' | 'pass' | 'fail') {
      text.textContent = t;
      el.dataset.state = state;
    },
  };
}

function scene(device: HTMLElement) {
  const s = h('div', 'pg-scene');
  s.append(h('span', 'pg-scene-light'), h('span', 'pg-scene-floor'), device);
  return s;
}

const KNOWN_APPS: Record<string, string> = { 'com.apple.preferences': 'Settings', 'com.android.settings': 'Settings' };
function appName(appId: string) {
  const id = (appId || '').toLowerCase();
  if (KNOWN_APPS[id]) return KNOWN_APPS[id];
  const last = (appId || 'App').split(/[./]/).filter(Boolean).pop() || 'App';
  return last.charAt(0).toUpperCase() + last.slice(1);
}

const HOME_APPS = ['Calendar', 'Photos', 'Camera', 'Mail', 'Notes', 'Clock', 'Weather', ''];
const DOCK_APPS = ['Phone', 'Browser', 'Messages', 'Music'];
const DAYS = ['Sunday', 'Monday', 'Tuesday', 'Wednesday', 'Thursday', 'Friday', 'Saturday'];
const MONTHS = ['January', 'February', 'March', 'April', 'May', 'June', 'July', 'August', 'September', 'October', 'November', 'December'];

class PhoneStage implements Stage {
  el = h('div', 'pg-device');
  mock = true;
  phone: HTMLElement;
  screen: HTMLElement;
  home: HTMLElement;
  app: HTMLElement;
  icon!: HTMLElement;
  clock: HTMLElement;
  island: HTMLElement | null = null;
  islandText: HTMLElement | null = null;
  screens: MockScreens;
  toast: (s: string) => void;
  chip = liveChip();
  name = '';
  appId = '';
  open = false;

  constructor(public kind: 'ios' | 'android', plan: any) {
    this.phone = h('div', 'pg-phone pg-' + kind);
    this.phone.append(h('span', 'pg-btn pg-btn-a'), h('span', 'pg-btn pg-btn-b'), h('span', 'pg-btn pg-btn-c'));
    this.screen = h('div', 'pg-screen');
    const bar = h('div', 'pg-statusbar');
    this.clock = h('span', 'pg-clock');
    const sys = h('span', 'pg-sys');
    sys.innerHTML = '<i class="pg-signal"><b></b><b></b><b></b><b></b></i><i class="pg-battery"></i>';
    bar.append(this.clock, sys);
    this.home = h('div', 'pg-home');
    this.app = h('div', 'pg-app');
    const touch = h('div', 'pg-touch');
    const kbd = new Keyboard();
    this.screen.append(h('div', 'pg-wall'), this.home, this.app, kbd.el, bar, touch);
    if (kind === 'ios') {
      this.island = h('div', 'pg-island');
      this.islandText = h('span', 'pg-island-text');
      this.island.append(h('span', 'pg-island-dot'), this.islandText);
      this.screen.append(this.island, h('div', 'pg-homebar'));
    } else this.screen.append(h('div', 'pg-punch'), h('div', 'pg-navbar'));
    this.phone.append(this.screen);
    this.el.append(scene(this.phone), this.chip.el, h('p', 'pg-caption'));
    this.screens = new MockScreens(this.app, kind, touch);
    this.screens.keyboard = kbd;
    this.toast = toaster(this.screen);
    this.setApp(plan.appId || '');
    this.tick();
    setInterval(() => this.tick(), 30000);
  }

  tick() {
    const d = new Date();
    this.clock.textContent = d.getHours() + ':' + String(d.getMinutes()).padStart(2, '0');
    const w = this.home.querySelector('.pg-widget');
    if (w) {
      w.querySelector('.pg-widget-day')!.textContent = DAYS[d.getDay()];
      w.querySelector('.pg-widget-date')!.textContent = `${d.getDate()} ${MONTHS[d.getMonth()]}`;
    }
  }

  live(text: string, state: 'idle' | 'run' | 'pass' | 'fail') {
    this.chip.set(text, state);
    if (!this.island) return;
    this.island.dataset.state = state;
    this.island.classList.toggle('pg-island-wide', state !== 'idle');
    this.islandText!.textContent = text;
  }

  setApp(appId: string) {
    if (appId === this.appId && this.icon) return;
    this.appId = appId;
    this.name = appName(appId);
    this.home.textContent = '';
    const widget = h('div', 'pg-widget');
    widget.append(h('span', 'pg-widget-day'), h('span', 'pg-widget-date'), h('span', 'pg-widget-note', 'No tests failed today'));
    const grid = h('div', 'pg-grid');
    HOME_APPS.forEach((name) => {
      const cell = h('div', 'pg-app-icon');
      const tile = h('span', 'pg-tile');
      if (name) {
        tile.innerHTML = glyph(name);
        cell.append(tile, h('span', 'pg-app-name', name));
      } else {
        tile.classList.add('pg-tile-target');
        tile.innerHTML = glyph(/settings|preferences/i.test(appId) ? 'Gear' : 'App');
        cell.append(tile, h('span', 'pg-app-name', this.name));
        this.icon = tile;
      }
      grid.append(cell);
    });
    const dock = h('div', 'pg-dock');
    for (const name of DOCK_APPS) {
      const t = h('span', 'pg-tile');
      t.innerHTML = glyph(name);
      dock.append(t);
    }
    this.home.append(widget, grid, dock);
    this.tick();
    (this.el.querySelector('.pg-caption') as HTMLElement).textContent =
      `Mock ${this.kind === 'ios' ? 'iPhone' : 'Android phone'} · ${appId || 'no appId'}`;
  }

  idle(plan: any) {
    if (running) return;
    this.setApp(plan.appId || '');
    this.live('Ready · press Run', 'idle');
  }

  async goHome() {
    if (!this.open) return;
    this.screens.keyboard?.show(false);
    this.screen.classList.remove('pg-in-app');
    this.app.classList.remove('pg-open');
    if (!reduced) await sleep(380);
    this.screens.clear();
    this.open = false;
  }

  async beginTest() {
    await this.goHome();
  }

  async launch(appId: string, ctx: Ctx) {
    if (appId && appId !== this.appId) this.setApp(appId);
    await this.goHome();
    await sleep(PACE * 0.3);
    const s = this.screen.getBoundingClientRect();
    const b = this.icon.getBoundingClientRect();
    const x = b.left - s.left + b.width / 2;
    const y = b.top - s.top + b.height / 2;
    this.icon.classList.add('pg-tile-press');
    await this.screens.touchAt(x, y);
    await sleep(PACE * 0.35);
    this.icon.classList.remove('pg-tile-press');
    this.app.style.transformOrigin = `${x}px ${y}px`;
    await this.screens.push(this.name, lookahead(ctx.steps, ctx.i + 1, ctx.vars), true);
    this.app.getBoundingClientRect();
    this.app.classList.add('pg-open');
    this.screen.classList.add('pg-in-app');
    this.open = true;
    await sleep(reduced ? 100 : 700);
  }

  async step(step: any, ctx: Ctx): Promise<Outcome> {
    const [k, v] = actionOf(step);
    const pass: Outcome = { status: 'pass', failures: [] };
    if (k === 'launchApp') { await this.launch(interp(v.app_id || this.appId, ctx.vars), ctx); return pass; }
    if (k === 'stopApp' || k === 'killApp') { await this.goHome(); return pass; }
    if ((k === 'pressKey' && v.key === 'back') || k === 'back') {
      this.screens.keyboard?.show(false);
      if (this.screens.views.length > 1) await this.screens.pop();
      else await this.goHome();
      await sleep(PACE * 0.5);
      return pass;
    }
    if (k === 'pressKey' && v.key === 'home') { await this.goHome(); return pass; }
    if (k === 'http') return pass;
    return this.screens.play(k, v, ctx, this.toast);
  }
}

const DRIVER = `(() => {
  const vis = (el) => { if (!el) return false; const r = el.getBoundingClientRect(); return r.width > 0 && r.height > 0 && getComputedStyle(el).visibility !== 'hidden'; };
  const deepest = (pred) => { let best = null; for (const el of document.body ? document.body.querySelectorAll('*') : []) if (pred((el.innerText || el.textContent || '').trim())) best = el; return best; };
  const find = (s) => {
    if (!s) return null;
    if (s.index) return find(s.index.selector);
    if (s.relative) return find(s.relative.target);
    if (s.compound) return find(s.compound.find((c) => typeof Object.values(c)[0] !== 'boolean'));
    if (s.css) return document.querySelector(s.css);
    if (s.xPath) return document.evaluate(s.xPath, document, null, 9, null).singleNodeValue;
    if (s.accessibilityId || s.name) { const n = s.accessibilityId || s.name; return document.querySelector('[aria-label="' + CSS.escape(n) + '"]') || deepest((t) => t === n); }
    if (s.textMatches) { const re = new RegExp('^(?:' + s.textMatches + ')$'); return deepest((t) => re.test(t)); }
    if (s.id) return document.getElementById(s.id) || document.querySelector('[data-testid="' + s.id + '"]');
    if (s.text != null) return deepest((t) => t === s.text);
    if (s.textContains != null) return deepest((t) => t.includes(s.textContains));
    return null;
  };
  const until = async (fn, ms) => { const end = Date.now() + ms; for (;;) { const v = fn(); if (v || Date.now() > end) return v; await new Promise((r) => setTimeout(r, 100)); } };
  const rect = (el) => { const r = el.getBoundingClientRect(); return { x: r.left + Math.min(r.width / 2, 40), y: r.top + r.height / 2 }; };
  const ring = (el) => { const o = el.style.outline, f = el.style.outlineOffset; el.style.outline = '2px solid #34c759'; el.style.outlineOffset = '3px'; setTimeout(() => { el.style.outline = o; el.style.outlineOffset = f; }, 900); };
  addEventListener('message', async (ev) => {
    if (ev.source !== parent) return;
    const { id, op, sel, text, expected, act } = ev.data || {};
    if (id == null) return;
    let out;
    try {
      const el = await until(() => { const e = find(sel); return vis(e) ? e : null; }, op === 'assertNotVisible' ? 0 : 3000);
      if (op === 'assertNotVisible') {
        const gone = await until(() => !vis(find(sel)), 3000);
        out = gone ? { ok: true } : { ok: false, message: 'still visible' };
      } else if (!el) out = { ok: false, message: 'no element matches the selector' };
      else if (op === 'locate') { el.scrollIntoView({ block: 'center' }); out = { ok: true, at: rect(el) }; }
      else if (op === 'tap') { el.click(); out = { ok: true }; }
      else if (op === 'inputText') { el.focus(); el.value = (el.value || '') + text; el.dispatchEvent(new Event('input', { bubbles: true })); out = { ok: true }; }
      else if (op === 'assertText') { const got = (el.innerText || el.textContent || '').trim(); if (got === expected) ring(el); out = got === expected ? { ok: true, at: rect(el) } : { ok: false, message: 'expected "' + expected + '", found "' + got + '"' }; }
      else { el.scrollIntoView({ block: 'center' }); ring(el); out = { ok: true, at: rect(el) }; }
    } catch (e) { out = { ok: false, message: String(e && e.message || e) }; }
    parent.postMessage({ __pgDriver: true, id, ...out }, PARENT_ORIGIN);
  });
})();`;

class BrowserStage implements Stage {
  el = h('div', 'pg-device');
  mock = true;
  addr: HTMLElement;
  host: HTMLElement;
  tab: HTMLElement;
  progress: HTMLElement;
  page: HTMLElement;
  touch: HTMLElement;
  frame: HTMLIFrameElement | null = null;
  screens: MockScreens;
  toast: (s: string) => void;
  chip = liveChip();
  html = '';
  seq = 0;
  waiting = new Map<number, (d: any) => void>();

  constructor(plan: any) {
    const win = h('div', 'pg-browser');
    const tabs = h('div', 'pg-tabs');
    const dots = h('span', 'pg-lights');
    dots.innerHTML = '<i></i><i></i><i></i>';
    this.tab = h('span', 'pg-tab', 'New Tab');
    tabs.append(dots, this.tab);
    const bar = h('div', 'pg-browser-bar');
    const nav = h('span', 'pg-browser-nav');
    nav.innerHTML = ICON_BACK + ICON_CHEV + ICON_RELOAD;
    this.addr = h('span', 'pg-addr');
    const lock = h('span', 'pg-lock');
    lock.innerHTML = ICON_LOCK;
    this.host = h('span', 'pg-addr-text');
    this.addr.append(lock, this.host);
    bar.append(nav, this.addr);
    this.progress = h('span', 'pg-progress');
    const body = h('div', 'pg-browser-body');
    this.page = h('div', 'pg-page');
    this.touch = h('div', 'pg-touch');
    const pointer = h('span', 'pg-pointer');
    pointer.innerHTML = CURSOR;
    this.touch.append(pointer);
    body.append(this.page, this.touch);
    win.append(tabs, bar, this.progress, body);
    this.el.append(scene(win), this.chip.el, h('p', 'pg-caption', 'data: pages run for real in a sandboxed frame; other URLs get a mock page.'));
    this.screens = new MockScreens(this.page, 'web', this.touch);
    this.screens.pointer = pointer;
    this.toast = toaster(body);
    addEventListener('message', (ev) => {
      if (!this.frame || ev.source !== this.frame.contentWindow || !ev.data?.__pgDriver) return;
      this.waiting.get(ev.data.id)?.(ev.data);
      this.waiting.delete(ev.data.id);
    });
    this.idle(plan);
  }

  live(text: string, state: 'idle' | 'run' | 'pass' | 'fail') { this.chip.set(text, state); }

  idle(plan: any) {
    if (running) return;
    this.setAddr(plan.target.url || plan.appId || 'about:blank');
    this.live('Ready · press Run', 'idle');
  }

  setAddr(u: string) {
    const data = u.startsWith('data:');
    this.addr.classList.toggle('pg-addr-secure', u.startsWith('https:'));
    const shown = data ? u.slice(0, u.indexOf(',') + 1) + '…' : u.replace(/^https?:\/\//, '');
    this.host.textContent = shown;
    this.addr.title = u;
    let title = 'New Tab';
    if (data) title = 'Page under test';
    else { try { title = new URL(u).host; } catch {} }
    this.tab.textContent = title;
  }

  async beginTest() {
    this.closeFrame();
    this.screens.clear();
    this.screens.pointer?.classList.remove('pg-pointer-on');
  }

  closeFrame() {
    this.frame?.remove();
    this.frame = null;
  }

  async loading<T>(work: () => Promise<T>) {
    this.progress.classList.remove('pg-progress-done');
    this.progress.classList.add('pg-progress-on');
    const out = await work();
    await sleep(reduced ? 0 : 250);
    this.progress.classList.add('pg-progress-done');
    setTimeout(() => this.progress.classList.remove('pg-progress-on', 'pg-progress-done'), 400);
    return out;
  }

  async load(u: string, ctx: Ctx) {
    this.setAddr(u);
    await this.loading(async () => {
      this.closeFrame();
      this.screens.clear();
      const m = /^data:text\/html(;base64)?,(.*)$/s.exec(u);
      if (m) {
        try { this.html = m[1] ? atob(m[2]) : decodeURIComponent(m[2]); } catch { this.html = m[2]; }
        await this.reload();
      } else {
        let host = u;
        try { host = new URL(u).host; } catch {}
        await sleep(reduced ? 0 : 300);
        await this.screens.push(host, lookahead(ctx.steps, ctx.i + 1, ctx.vars), true);
      }
    });
    await sleep(PACE * 0.4);
  }

  async reload() {
    this.closeFrame();
    const f = document.createElement('iframe');
    f.className = 'pg-frame';
    f.sandbox.add('allow-scripts');
    f.title = 'Page under test';
    f.srcdoc = `<!doctype html><meta charset="utf-8"><style>body{font:15px/1.5 system-ui,sans-serif;margin:24px;color:#14161a;background:#fff}h1{font-size:28px;letter-spacing:-.01em;margin:0 0 10px}a{color:#3b5bdb}</style>${this.html}<script>const PARENT_ORIGIN = ${JSON.stringify(location.origin)};${DRIVER}<\/script>`;
    const loaded = new Promise((r) => f.addEventListener('load', r, { once: true }));
    this.page.append(f);
    this.frame = f;
    await loaded;
  }

  drive(cmd: any): Promise<any> {
    const id = ++this.seq;
    return new Promise((resolve) => {
      this.waiting.set(id, resolve);
      // The sandboxed frame's origin is opaque, so it can't be named.
      // nosemgrep: javascript.browser.security.wildcard-postmessage-configuration.wildcard-postmessage-configuration
      this.frame!.contentWindow!.postMessage({ id, ...cmd }, '*');
      setTimeout(() => { if (this.waiting.delete(id)) resolve({ ok: false, message: 'the page did not answer' }); }, 8000);
    });
  }

  async step(step: any, ctx: Ctx): Promise<Outcome> {
    const [k, v] = actionOf(step);
    const pass: Outcome = { status: 'pass', failures: [] };
    if (k === 'launchApp' || k === 'goto' || k === 'openLink') {
      await this.load(interp(v.url || v.app_id || plan?.target?.url || plan?.appId || '', ctx.vars), ctx);
      return pass;
    }
    if (k === 'http') return pass;
    if (k === 'refresh') {
      await this.loading(async () => { if (this.frame) await this.reload(); else await sleep(reduced ? 0 : 350); });
      return pass;
    }
    if (!this.frame) {
      if (k === 'back' || (k === 'pressKey' && v.key === 'back')) { await this.screens.pop(); return pass; }
      return this.screens.play(k, v, ctx, this.toast);
    }
    if (k === 'screenshot') {
      this.page.classList.add('pg-flash');
      setTimeout(() => this.page.classList.remove('pg-flash'), 350);
      this.toast(v.filename ? `screenshot · ${v.filename}` : 'screenshot');
      await sleep(PACE);
      return pass;
    }
    const ops: Record<string, string> = { tap: 'tap', inputText: 'inputText', assertVisible: 'assertVisible', assertNotVisible: 'assertNotVisible', assertText: 'assertText', scrollUntilVisible: 'assertVisible' };
    if (!ops[k] || !v.selector) { this.toast(k); await sleep(PACE); return pass; }
    const sel = JSON.parse(interp(JSON.stringify(v.selector), ctx.vars));
    const args = { sel, text: interp(v.text ?? '', ctx.vars), expected: interp(v.expected ?? '', ctx.vars) };
    if (k === 'tap' || k === 'inputText') {
      const at = await this.drive({ op: 'locate', ...args });
      if (!at.ok) return { status: 'fail', failures: [at.message || 'failed'] };
      await this.screens.touchAt(at.at.x, at.at.y);
      const res = await this.drive({ op: ops[k], ...args });
      await sleep(PACE);
      return res.ok ? pass : { status: 'fail', failures: [res.message || 'failed'] };
    }
    const res = await this.drive({ op: ops[k], ...args });
    await sleep(PACE);
    return res.ok ? pass : { status: 'fail', failures: [res.message || 'failed'] };
  }
}

class ApiStage implements Stage {
  el = h('div', 'pg-api');
  mock = false;
  list = h('div', 'pg-api-list');
  detail = h('div', 'pg-api-detail');
  rows = new Map<string, HTMLElement>();
  exchanges = new Map<string, Exchange>();

  constructor(plan: any) {
    const head = h('div', 'pg-api-head');
    head.append(h('span', 'pg-api-title', 'Requests'), h('span', 'pg-api-base'));
    this.el.append(head, this.list, this.detail);
    this.idle(plan);
  }

  idle(plan: any) {
    if (running) return;
    (this.el.querySelector('.pg-api-base') as HTMLElement).textContent = plan.target.baseUrl || '';
    this.list.textContent = '';
    this.rows.clear();
    this.exchanges.clear();
    this.detail.textContent = '';
    this.detail.append(h('p', 'pg-api-hint', 'Run the suite to send these requests from your browser. Select one to see its response.'));
    plan.tests.forEach((t: any, ti: number) => {
      this.list.append(h('div', 'pg-api-test', t.name));
      for (const s of t.steps) {
        const [k, v] = actionOf(s);
        if (k !== 'http') continue;
        const b = h('button', 'pg-api-row') as HTMLButtonElement;
        b.type = 'button';
        b.disabled = true;
        b.append(h('span', 'pg-method', (v.method || 'GET').toUpperCase()), h('span', 'pg-path', v.url || ''), h('span', 'pg-code'), h('span', 'pg-ms'));
        const key = ti + ':' + s.index;
        b.addEventListener('click', () => this.show(key));
        this.rows.set(key, b);
        this.list.append(b);
      }
    });
  }

  async beginTest() {}

  async step(): Promise<Outcome> { return { status: 'pass', failures: [] }; }

  exchange(ti: number, step: any, ex: Exchange, ok: boolean) {
    const key = ti + ':' + step.index;
    const b = this.rows.get(key);
    this.exchanges.set(key, ex);
    if (b) {
      b.disabled = false;
      const path = ex.url.replace(/^https?:\/\/[^/]+/, '') || '/';
      (b.querySelector('.pg-path') as HTMLElement).textContent = path;
      const code = b.querySelector('.pg-code') as HTMLElement;
      code.textContent = ex.status ? String(ex.status) : 'ERR';
      code.className = 'pg-code ' + (ok ? 'ok' : 'bad');
      (b.querySelector('.pg-ms') as HTMLElement).textContent = ex.ms != null ? ex.ms + 'ms' : '';
    }
    this.show(key);
  }

  show(key: string) {
    const ex = this.exchanges.get(key);
    if (!ex) return;
    this.rows.forEach((r, k) => r.classList.toggle('sel', k === key));
    this.detail.textContent = '';
    this.detail.append(h('div', 'pg-api-url', `${ex.method} ${ex.url}`));
    if (ex.status == null) { this.detail.append(h('p', 'pg-api-hint', 'No response.')); return; }
    const ct = ex.headers?.['content-type'] || '';
    this.detail.append(h('div', 'pg-api-meta', `${ex.status} ${ex.statusText || ''} · ${ex.ms}ms${ct ? ' · ' + ct : ''}`));
    let body = ex.body || '';
    if (/json/.test(ct)) { try { body = JSON.stringify(JSON.parse(body), null, 2); } catch {} }
    if (body.length > 6000) body = body.slice(0, 6000) + `\n… ${(ex.body!.length - 6000).toLocaleString()} more characters`;
    this.detail.append(h('pre', 'pg-api-body', body));
  }
}

// --- the run ---------------------------------------------------------------

async function run() {
  if (running) { stopRequested = true; setRunLabel('Stopping…', true); return; }
  if (!wasm) return;
  const text = model.getValue();
  showOutput('terminal');
  term.textContent = '';
  line(['$ ', 't-dim'], `atalanta run ${FILE}`);
  arrow('Loading suite from ', [FILE, 't-b']);
  plan = makePlan(text);
  if (!plan || !plan.ok) {
    line(['error', 't-red t-b'], ': ' + (plan?.error || 'the suite did not load'));
    line(['exit 2 (config)', 't-dim']);
    setResult('fail');
    setView('output');
    return;
  }
  refreshStage();
  const kind = kindOf(plan);
  if (kind === 'api') arrow('Platform: ', ['none (http steps only)', 't-green']);
  else {
    arrow('Platform: ', [kind, 't-green']);
    arrow('Preparing ', [kind, 't-green'], kind === 'web' ? ' driver...' : ' driver... ', ...(kind === 'web' ? [] : [['(mock device in this page)', 't-dim'] as Part]));
  }
  line('');
  line(`  Atalanta v${wasm.version()}`);
  line(`  Tests: ${plan.tests.length}`);
  line('');

  running = true;
  stopRequested = false;
  setRunLabel('Stop', true);
  setResult('');
  setView('device');
  examplesEl.disabled = true;
  const st = stage!;
  let passed = 0, failed = 0, total = 0;
  try {
    for (let ti = 0; ti < plan.tests.length && !stopRequested; ti++) {
      const test = plan.tests[ti];
      const head = line(['  ● ', 't-dim'], test.name, ['  running', 't-dim']);
      const vars: Record<string, string> = {};
      let took = 0;
      let failure = '';
      await st.beginTest(ti);
      for (let i = 0; i < test.steps.length; i++) {
        if (stopRequested) { failure = 'stopped'; break; }
        const step = test.steps[i];
        highlight(step.line);
        st.live?.(clip(interp(step.label, vars), 48), 'run');
        const sl = line('    ', ['… ', 't-dim'], [clip(interp(step.label, vars)), 't-dim']);
        const s0 = performance.now();
        let out: Outcome;
        try {
          if (step.kind === 'other') out = await st.step(step, { text, ti, steps: test.steps, i, vars });
          else {
            out = await runWasmStep(text, ti, step, vars);
            if (out.exchange) st.exchange?.(ti, step, out.exchange, out.status === 'pass');
            if (st.mock && out.exchange) (st as any).toast?.(`${out.exchange.method} ${out.exchange.url.replace(/^https?:\/\/[^/]+/, '')} → ${out.exchange.status ?? 'ERR'}`);
          }
        } catch (e: any) {
          out = { status: 'fail', failures: [String(e?.message || e)] };
        }
        Object.assign(vars, out.captured || {});
        const icon: Part = out.status === 'pass' ? ['✓ ', 't-green'] : out.status === 'skip' ? ['- ', 't-yellow'] : ['✗ ', 't-red'];
        sl.textContent = '';
        const ms = performance.now() - s0;
        took += ms;
        sl.append('    ', h('span', icon[1], icon[0]), h('span', 't-dim', clip(interp(step.label, vars)) + '  ' + fmtMs(ms)));
        if (out.status === 'fail') {
          failure = out.failures.join('; ') || 'step failed';
          break;
        }
        if (!st.mock) await sleep(PACE * 0.4);
      }
      const dur = took;
      total += took;
      head.textContent = '';
      if (failure === 'stopped') {
        head.append('  ', h('span', 't-yellow', '●'), ` ${test.name} ... `, h('span', 't-yellow t-b', 'STOPPED'));
        break;
      }
      if (failure) {
        failed++;
        head.append('  ', h('span', 't-red', '●'), ` ${test.name} ... `, h('span', 't-red t-b', 'FAILED'), ` ${fmtSecs(dur)}`);
        line(['    └ ', 't-dim'], [failure, 't-red']);
      } else {
        passed++;
        head.append('  ', h('span', 't-green', '●'), ` ${test.name} ... `, h('span', 't-green t-b', 'PASSED'), ` ${fmtSecs(dur)}`);
      }
    }
    line('');
    line('  ' + '━'.repeat(43));
    line(`  ${passed} passed, ${failed} failed — ${fmtSecs(total)}`);
    st.live?.(stopRequested ? 'Stopped' : `${passed} passed, ${failed} failed · ${fmtSecs(total)}`, stopRequested ? 'idle' : failed ? 'fail' : 'pass');
    setResult(stopRequested ? '' : failed ? 'fail' : 'pass');
    if (st.mock) arrow(['Played on a mock screen. On a real ' + (kindOf(plan) === 'web' ? 'Chrome' : 'device') + ': ', 't-dim'], [`atalanta run ${FILE}`, 't-b']);
    else arrow(['Same file from a terminal: ', 't-dim'], [`atalanta run ${FILE}`, 't-b']);
  } finally {
    running = false;
    highlight();
    setRunLabel('Run', false);
    examplesEl.disabled = false;
  }
}

// --- output tabs -------------------------------------------------------------

function showOutput(which: 'terminal' | 'problems') {
  const t = which === 'terminal';
  tabTerm.setAttribute('aria-selected', String(t));
  tabProblems.setAttribute('aria-selected', String(!t));
  tabTerm.tabIndex = t ? 0 : -1;
  tabProblems.tabIndex = t ? -1 : 0;
  term.hidden = !t;
  $('playground-panel-problems').hidden = t;
}

function bindTabs() {
  tabTerm.addEventListener('click', () => showOutput('terminal'));
  tabProblems.addEventListener('click', () => showOutput('problems'));
  for (const t of [tabTerm, tabProblems]) {
    t.addEventListener('keydown', (e) => {
      if (e.key !== 'ArrowLeft' && e.key !== 'ArrowRight') return;
      e.preventDefault();
      const next = t === tabTerm ? tabProblems : tabTerm;
      next.focus();
      showOutput(next === tabTerm ? 'terminal' : 'problems');
    });
  }
}

// --- narrow screens: one panel at a time ------------------------------------

const root = document.querySelector('.playground') as HTMLElement;
const viewBtns = [...document.querySelectorAll<HTMLButtonElement>('[data-view-btn]')];

function setView(v: 'code' | 'device' | 'output') {
  root.dataset.view = v;
  for (const b of viewBtns) b.setAttribute('aria-pressed', String(b.dataset.viewBtn === v));
  if (v === 'output') showOutput('terminal');
}

function setResult(r: '' | 'pass' | 'fail') {
  const b = viewBtns.find((x) => x.dataset.viewBtn === 'output');
  if (b) b.dataset.result = r;
}

function bindViews() {
  for (const b of viewBtns) b.addEventListener('click', () => setView(b.dataset.viewBtn as any));
  setView('code');
}

// --- init ------------------------------------------------------------------

const themeFor = () => (document.documentElement.dataset.theme === 'light' ? 'vs' : 'vs-dark');

async function init() {
  new MutationObserver(() => monaco.editor.setTheme(themeFor()))
    .observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] });
  termIdle();
  bindTabs();
  await loadValidator();

  editor = monaco.editor.create($('playground-editor'), {
    language: 'yaml', theme: themeFor(), automaticLayout: true, minimap: { enabled: false },
    wordWrap: 'on', scrollBeyondLastLine: false, fontFamily: 'var(--mono)', fontSize: 13.5,
    lineNumbers: 'on', renderLineHighlight: 'none', glyphMargin: true, ariaLabel: 'Atalanta test suite editor',
    padding: { top: 10 },
    stickyScroll: { enabled: false },
  });
  configureMonacoYaml(monaco, {
    schemas: [{ uri: SCHEMA_URI, fileMatch: ['*'] }],
    enableSchemaRequest: true, completion: true, hover: true, validate: true,
  });
  model = monaco.editor.createModel('', 'yaml', MODEL_URI);
  editor.setModel(model);
  editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.Enter, () => { run(); });

  let vt: any, pt: any;
  model.onDidChangeContent(() => {
    clearTimeout(vt); vt = setTimeout(validate, 300);
    clearTimeout(pt); pt = setTimeout(refreshStage, 400);
    try { localStorage.setItem('atalanta-playground', model.getValue()); } catch {}
  });
  monaco.editor.onDidChangeMarkers((uris) => {
    if (uris.some((u) => u.toString() === model.uri.toString())) refreshProblems();
  });

  const byName = (n: string) => examples.find((e: any) => e.name === n);
  let current = examples[0];
  for (const e of examples) {
    const o = h('option', '', e.label) as HTMLOptionElement;
    o.value = e.name;
    examplesEl.append(o);
  }
  const setText = (v: string) => {
    if (running) stopRequested = true;
    model.setValue(v);
    editor.setScrollPosition({ scrollTop: 0 });
    termIdle();
  };

  let saved: string | null = null;
  try { saved = localStorage.getItem('atalanta-playground-example') && localStorage.getItem('atalanta-playground'); } catch {}
  try { const n = localStorage.getItem('atalanta-playground-example'); if (n && byName(n)) current = byName(n); } catch {}
  examplesEl.value = current.name;
  setText(saved || current.yaml);
  refreshStage();

  examplesEl.addEventListener('change', () => {
    current = byName(examplesEl.value) || examples[0];
    try { localStorage.setItem('atalanta-playground-example', current.name); } catch {}
    setText(current.yaml);
  });
  $('playground-reset').addEventListener('click', () => setText(current.yaml));
  $('playground-copy').addEventListener('click', async (e) => {
    const b = e.currentTarget as HTMLButtonElement;
    try { await navigator.clipboard.writeText(model.getValue()); } catch { return; }
    b.textContent = 'Copied';
    setTimeout(() => { b.textContent = 'Copy'; }, 1600);
  });
  $('playground-download').addEventListener('click', () => {
    const a = document.createElement('a');
    a.href = URL.createObjectURL(new Blob([model.getValue()], { type: 'text/yaml' }));
    a.download = FILE;
    a.click();
    setTimeout(() => URL.revokeObjectURL(a.href), 1000);
  });
  runBtns.forEach((b) => b.addEventListener('click', () => { run(); }));
  bindViews();
}

init();
