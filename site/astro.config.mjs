import { defineConfig } from 'astro/config';

const raw = process.env.BASE_PATH || '/';
const base = '/' + raw.replace(/^\/+|\/+$/g, '');

export default defineConfig({
  site: process.env.SITE_URL || 'https://atalanta.sh',
  base: base === '/' ? '/' : base + '/',
  trailingSlash: 'always',
  build: { format: 'directory' },
  devToolbar: { enabled: false },
  // three.js is one lazy chunk (~130 KB gzipped) loaded only by the landing page's
  // background, after the page is idle.
  vite: { build: { chunkSizeWarningLimit: 600 } },
  telemetry: false,
});
