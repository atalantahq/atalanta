import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// https://vite.dev/config/
export default defineConfig({
  plugins: [react()],
  base: './',
  // `npm run dev`: forward the API to `atalanta inspect` (no CORS). Open the
  // inspector's printed URL once so the session cookie is set for localhost.
  server: {
    proxy: {
      '/api': { target: 'http://127.0.0.1:9876', ws: true },
    },
  },
})
