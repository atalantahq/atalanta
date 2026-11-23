/** The one base-aware helper: every internal link, asset and bundle path goes through it. */
const base = (import.meta.env.BASE_URL || '/').replace(/\/+$/, '');

export function url(p: string = '/'): string {
  if (/^([a-z][a-z0-9+.-]*:|\/\/)/i.test(p)) return p;
  return base + (p.startsWith('/') ? p : '/' + p);
}

export const BASE = base + '/';
