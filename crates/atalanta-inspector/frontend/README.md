# Inspector frontend

The React + TypeScript + Vite source of the web UI served by `atalanta inspect`
(the console). You need this directory only to build the UI from a source
checkout or to change it. Release binaries already embed the built UI.

Cargo does not build it: `build.rs` never runs npm. A build without `dist/`
serves the API and a page saying the UI is not built.

## Build

```bash
npm ci && npm run build
```

This writes `dist/` (git-ignored), which the `atalanta-inspector` crate embeds
on its next build.

## Develop

1. Run `atalanta inspect` and open the URL it prints once, so the session
   cookie is set for localhost.
2. Run `npm run dev`. Vite forwards `/api` (including WebSockets) to
   `http://127.0.0.1:9876`, set in `vite.config.ts`, so the inspector must be
   listening on that port.

## Scripts

| Command | Does |
|---|---|
| `npm run dev` | Vite dev server with hot reload |
| `npm run build` | `tsc -b && vite build` |
| `npm run lint` | ESLint over the project |
| `npm run preview` | Serve the built `dist/` |
| `npm test` | `node --test tests/*.test.ts` |
