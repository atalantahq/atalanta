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
  vite: {
    // monaco-editor, loaded only by /playground/, is ~3.2 MB (~800 KB gzipped).
    build: { chunkSizeWarningLimit: 3300 },
    ssr: { external: ['satori', '@resvg/resvg-js'] },
  },
  telemetry: false,
});
