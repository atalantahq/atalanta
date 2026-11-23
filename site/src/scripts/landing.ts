// Landing page motion: the 3D hero track, the galaxy background, code
// windows that unfold in 3D as they scroll in, a pointer spotlight on cards,
// and sections that rise into view. Everything here is decoration: the page
// reads the same with scripts off or reduced motion on.
import { startHeroTrack } from './hero-track';

const reduce = matchMedia('(prefers-reduced-motion: reduce)').matches;
const saveData = (navigator as Navigator & { connection?: { saveData?: boolean } }).connection?.saveData;

const stage = document.querySelector<HTMLElement>('[data-hero-stage]');
if (stage) startHeroTrack(stage);

// The galaxy loads three.js on its own, after the page is up.
const galaxy = document.querySelector<HTMLCanvasElement>('canvas.galaxy');
if (galaxy && !saveData) {
  const go = () => import('./galaxy').then((m) => m.startGalaxy(galaxy)).catch(() => galaxy.remove());
  if ('requestIdleCallback' in window) (window as Window & { requestIdleCallback: (cb: () => void) => void }).requestIdleCallback(go);
  else setTimeout(go, 200);
} else galaxy?.remove();

// Spotlight: a soft brand-coloured light follows the pointer across a card.
document.querySelectorAll<HTMLElement>('.card, .stat, .steps li').forEach((el) => {
  el.classList.add('spot');
  el.addEventListener('pointermove', (e) => {
    const r = el.getBoundingClientRect();
    el.style.setProperty('--mx', `${e.clientX - r.left}px`);
    el.style.setProperty('--my', `${e.clientY - r.top}px`);
  });
  el.addEventListener('pointerleave', () => { el.style.removeProperty('--mx'); el.style.removeProperty('--my'); });
});

if (!reduce) {
  // Code windows start tilted back in 3D and flatten as they reach the
  // middle of the screen (wide screens only).
  const tilts = [...document.querySelectorAll<HTMLElement>('.tilt')];
  const wide = matchMedia('(min-width: 960px)');
  let ticking = false;
  const update = () => {
    ticking = false;
    const vh = innerHeight;
    for (const el of tilts) {
      if (!wide.matches) { el.style.transform = ''; continue; }
      const r = el.getBoundingClientRect();
      const p = Math.min(Math.max((vh - r.top) / (vh * 0.75), 0), 1);
      const e = 1 - Math.pow(1 - p, 3);
      el.style.transform = `perspective(1200px) rotateX(${(1 - e) * 16}deg) rotateY(${(1 - e) * -12}deg) scale(${0.93 + 0.07 * e})`;
    }
  };
  const onScroll = () => { if (!ticking) { ticking = true; requestAnimationFrame(update); } };
  addEventListener('scroll', onScroll, { passive: true });
  addEventListener('resize', onScroll);
  update();

  // Sections rise into view once.
  if ('IntersectionObserver' in window) {
    const els = document.querySelectorAll<HTMLElement>('.band:not(.hero) .band-head, .band:not(.hero) .copy-block, .band:not(.hero) h2.sec, .cards, .steps, .tbl, .quote');
    const io = new IntersectionObserver((entries) => {
      for (const e of entries) if (e.isIntersecting) { e.target.classList.add('in'); io.unobserve(e.target); }
    }, { rootMargin: '0px 0px -8% 0px' });
    els.forEach((el) => {
      const r = el.getBoundingClientRect();
      if (r.top < innerHeight) return; // already on screen: leave it be
      el.classList.add('reveal');
      io.observe(el);
    });
  }
}
