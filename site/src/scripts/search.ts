const dialog = document.getElementById('search') as HTMLDialogElement | null;
const input = document.getElementById('search-q') as HTMLInputElement | null;
const list = document.getElementById('search-results') as HTMLUListElement | null;
const base = document.documentElement.dataset.base || '/';

let pf: any = null;
let pfFailed = false;
let active = -1;
let seq = 0;

async function load() {
  if (pf || pfFailed) return pf;
  try {
    pf = await import(/* @vite-ignore */ `${base}pagefind/pagefind.js`);
    await pf.options({ baseUrl: base });
  } catch {
    pfFailed = true;
  }
  return pf;
}

function open() {
  if (!dialog || dialog.open) return;
  dialog.showModal();
  input!.value = '';
  render(null);
  input!.focus();
  load();
}
function close() { dialog?.close(); }

function esc(s: string) { return s.replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]!)); }

function render(items: any[] | null, msg?: string) {
  if (!list) return;
  active = -1;
  if (!items) { list.innerHTML = `<li class="search-empty" role="presentation">${msg ?? 'Type to search the docs.'}</li>`; return; }
  if (!items.length) { list.innerHTML = `<li class="search-empty" role="presentation">No results.</li>`; return; }
  list.innerHTML = items.map((d, i) => {
    const sub = d.sub_results?.[0];
    const href = (sub?.url || d.url);
    const sec = sub && sub.title && sub.title !== d.meta?.title ? `<div class="r-sec">${esc(sub.title)}</div>` : '';
    return `<li role="presentation"><a role="option" id="r${i}" href="${esc(href)}" aria-selected="false"><div class="r-title">${esc(d.meta?.title || d.url)}</div>${sec}<div class="r-ex">${sub?.excerpt || d.excerpt || ''}</div></a></li>`;
  }).join('');
  select(0);
}

function select(i: number) {
  const links = list ? [...list.querySelectorAll<HTMLAnchorElement>('a')] : [];
  if (!links.length) return;
  active = (i + links.length) % links.length;
  links.forEach((a, j) => a.setAttribute('aria-selected', String(j === active)));
  links[active].scrollIntoView({ block: 'nearest' });
  input?.setAttribute('aria-activedescendant', links[active].id);
}

let timer: number | undefined;
input?.addEventListener('input', () => {
  clearTimeout(timer);
  timer = window.setTimeout(async () => {
    const q = input.value.trim();
    if (!q) return render(null);
    const lib = await load();
    if (!lib) return render(null, 'Search is built with the site. Run <code>npm run build</code> and <code>npm run preview</code> to use it.');
    const my = ++seq;
    const res = await lib.search(q);
    const data = await Promise.all(res.results.slice(0, 8).map((r: any) => r.data()));
    if (my === seq) render(data);
  }, 90);
});

input?.addEventListener('keydown', (e) => {
  if (e.key === 'ArrowDown') { e.preventDefault(); select(active + 1); }
  else if (e.key === 'ArrowUp') { e.preventDefault(); select(active - 1); }
  else if (e.key === 'Escape') { e.preventDefault(); close(); }
  else if (e.key === 'Enter') {
    const a = list?.querySelectorAll<HTMLAnchorElement>('a')[active];
    if (a) { e.preventDefault(); window.location.href = a.href; }
  }
});
list?.addEventListener('mousemove', (e) => {
  const a = (e.target as HTMLElement).closest('a');
  if (a) select([...list.querySelectorAll('a')].indexOf(a));
});
dialog?.addEventListener('click', (e) => { if (e.target === dialog) close(); });
document.querySelectorAll('[data-search-open]').forEach((b) => b.addEventListener('click', open));
document.addEventListener('keydown', (e) => {
  const t = e.target as HTMLElement;
  const typing = t && (t.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName));
  if ((e.key === 'k' || e.key === 'K') && (e.metaKey || e.ctrlKey)) { e.preventDefault(); dialog?.open ? close() : open(); }
  else if (e.key === '/' && !typing && !e.metaKey && !e.ctrlKey && !e.altKey) { e.preventDefault(); open(); }
});
