# Atalanta site

The public website: a landing page plus the docs, as one static Astro site. The docs are not copied here; they are rendered at build time from the repo's Markdown (`../README.md`, `../docs/*.md`, `../CHANGELOG.md`, `../CONTRIBUTING.md`, `../SECURITY.md`).

## Run and build

```bash
npm ci
npm run dev        # dev server (search needs a built index, see below)
npm run build      # astro build, then Pagefind over dist/, then prints anchor warnings
npm run preview    # serve dist/ (needed to try search)
```

Environment variables (both optional):

- `SITE_URL` — default `https://atalanta.sh` (canonical URLs, sitemap, Open Graph)
- `BASE_PATH` — default `/`; use `/atalanta/` for a GitHub Pages project site

Every internal URL goes through `src/lib/url.ts`. The version comes from `[workspace.package]` in `../Cargo.toml`; other constants are in `src/lib/site.ts`.

## Registering a docs page

All pages live in `src/lib/docs-config.ts`: add `{ label, slug, source }` to a group in `GROUPS`, or add the file to `IGNORED`. The build fails if a `docs/*.md` file is in neither.

Markdown handling is in `src/lib/docs.ts`: relative links between files become site routes (or GitHub links for other repo files; a missing target fails the build, or set `LINK_CHECK=warn`), heading ids follow GitHub's anchors, and links to a `#anchor` that does not exist are printed as warnings at the end of the build.

`/llms.txt` is `../docs/llms.txt`, served at build time.
