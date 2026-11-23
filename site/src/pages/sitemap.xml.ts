import type { APIRoute } from 'astro';
import { loadDocs } from '../lib/docs';
import { url } from '../lib/url';

export const GET: APIRoute = async ({ site }) => {
  const pages = await loadDocs();
  const paths = ['/', ...pages.map((p) => p.route)];
  const body = `<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n` +
    paths.map((p) => `  <url><loc>${new URL(url(p), site).href}</loc></url>`).join('\n') + `\n</urlset>\n`;
  return new Response(body, { headers: { 'Content-Type': 'application/xml' } });
};
