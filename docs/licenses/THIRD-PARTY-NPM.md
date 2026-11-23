# Third-party npm packages (inspector frontend)

Source: `crates/atalanta-inspector/frontend/package-lock.json` (lockfileVersion 3),
licenses read from each package's `package.json` in `node_modules`. Regenerate
after changing the frontend's dependencies.

## Shipped in the Atalanta binary (production dependencies)

`npm run build` bundles these into `frontend/dist/assets/*.js`, which
`rust-embed` compiles into the `atalanta` binary. The minified bundle does not
keep their license comments, so their notices are reproduced in `NOTICE`.

| Package | Version | License | Copyright | Source |
| --- | --- | --- | --- | --- |
| react | 19.2.4 | MIT | Copyright (c) Meta Platforms, Inc. and affiliates. | https://github.com/facebook/react |
| react-dom | 19.2.4 | MIT | Copyright (c) Meta Platforms, Inc. and affiliates. | https://github.com/facebook/react |
| scheduler | 0.27.0 | MIT | Copyright (c) Meta Platforms, Inc. and affiliates. | https://github.com/facebook/react |
| zustand | 5.0.11 | MIT | Copyright (c) 2019 Paul Henschel | https://github.com/pmndrs/zustand |

Summary: 4 packages, all MIT.

## Build and test tooling only (devDependencies, not shipped)

These run on the developer's or CI's machine to type-check, lint and bundle
the frontend. None of their code ends up in `dist/` or in the binary.

| License | Packages |
| --- | ---: |
| MIT | 151 |
| Apache-2.0 | 15 |
| MPL-2.0 | 12 (`lightningcss` and its 11 per-platform optional binaries, used by Vite for CSS) |
| ISC | 11 |
| BSD-2-Clause | 6 |
| BSD-3-Clause | 2 |
| 0BSD | 1 |
| BlueOak-1.0.0 | 1 (`minimatch` 10, nested under `@typescript-eslint/typescript-estree`) |
| Python-2.0 | 1 (`argparse`) |
| CC-BY-4.0 | 1 (`caniuse-lite`, browser-support data) |

Total in the lockfile: 205 packages (4 production, 201 development).

No GPL, LGPL or AGPL package appears anywhere in the tree, and no package is
missing a license field. The MPL-2.0 and CC-BY-4.0 entries are build tools and
data consumed at build time; using them unmodified to build the bundle places
no obligation on Atalanta's own code.
