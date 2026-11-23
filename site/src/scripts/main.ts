import './search';

const root = document.documentElement;
const isMac = /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);
document.querySelectorAll('[data-mod]').forEach((el) => (el.textContent = isMac ? '⌘K' : 'Ctrl K'));

// theme
document.querySelector('[data-theme-toggle]')?.addEventListener('click', () => {
  const next = root.dataset.theme === 'light' ? 'dark' : 'light';
  root.dataset.theme = next;
  try { localStorage.setItem('theme', next); } catch {}
});

// mobile menu
const menuBtn = document.querySelector<HTMLButtonElement>('[data-menu-toggle]');
function setMenu(open: boolean) {
  if (open) root.dataset.menu = 'open'; else delete root.dataset.menu;
  menuBtn?.setAttribute('aria-expanded', String(open));
  menuBtn?.setAttribute('aria-label', open ? 'Close menu' : 'Open menu');
}
menuBtn?.addEventListener('click', () => setMenu(root.dataset.menu !== 'open'));
document.addEventListener('keydown', (e) => { if (e.key === 'Escape' && root.dataset.menu === 'open') setMenu(false); });
document.querySelectorAll('.sidebar a, .menu-panel a').forEach((a) => a.addEventListener('click', () => setMenu(false)));
document.querySelector('[data-scrim]')?.addEventListener('click', () => setMenu(false));

// copy buttons: wrap every highlighted block that does not have one
function addCopy(host: HTMLElement, getText: () => string) {
  const b = document.createElement('button');
  b.type = 'button'; b.className = 'copy'; b.textContent = 'Copy'; b.setAttribute('aria-label', 'Copy code');
  b.addEventListener('click', async () => {
    const text = getText();
    try { await navigator.clipboard.writeText(text); }
    catch {
      const ta = document.createElement('textarea'); ta.value = text; document.body.appendChild(ta); ta.select();
      try { document.execCommand('copy'); } catch {} ta.remove();
    }
    b.textContent = 'Copied'; b.setAttribute('data-done', '');
    setTimeout(() => { b.textContent = 'Copy'; b.removeAttribute('data-done'); }, 1600);
  });
  host.appendChild(b);
}
document.querySelectorAll<HTMLElement>('.prose pre').forEach((pre) => {
  const wrap = document.createElement('div');
  wrap.className = 'codeblock';
  pre.replaceWith(wrap); wrap.appendChild(pre);
  addCopy(wrap, () => pre.textContent || '');
});
document.querySelectorAll<HTMLElement>('[data-copy-target]').forEach((host) => {
  const target = host.querySelector<HTMLElement>(host.dataset.copyTarget!) || host;
  addCopy(host, () => (host.dataset.copyText ?? target.textContent ?? '').trim());
});

// docs: highlight the section in view
const toc = document.querySelectorAll<HTMLAnchorElement>('.toc a');
if (toc.length && 'IntersectionObserver' in window) {
  const map = new Map<string, HTMLAnchorElement>();
  toc.forEach((a) => map.set(decodeURIComponent(a.hash.slice(1)), a));
  const heads = [...map.keys()].map((id) => document.getElementById(id)).filter(Boolean) as HTMLElement[];
  const set = (id: string) => toc.forEach((a) => (decodeURIComponent(a.hash.slice(1)) === id ? a.setAttribute('aria-current', 'location') : a.removeAttribute('aria-current')));
  const visible = new Set<string>();
  const io = new IntersectionObserver((entries) => {
    for (const e of entries) e.isIntersecting ? visible.add(e.target.id) : visible.delete(e.target.id);
    const first = heads.find((h) => visible.has(h.id));
    if (first) set(first.id);
  }, { rootMargin: '-72px 0px -70% 0px' });
  heads.forEach((h) => io.observe(h));
}
