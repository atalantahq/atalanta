---
name: pre-pr
description: Run locally the CI checks that apply to what a branch changed, before pushing or opening a pull request — fmt, clippy, rustdoc, nextest, doc tests, MSRV, the docs guards, the licence inventory, cargo-deny, shellcheck, actionlint, the unsafe baseline, the tree-server jar and the inspector frontend. Use before a push or PR, when asked "will CI pass", or to reproduce a red `CI OK`, `Docs` or `Security` check.
---

# Pre-PR checks

`CI OK` (`.github/workflows/ci.yml`) is the one required check. It skips jobs by changed path, so do the same: list what the branch changed, then run the rows that match. Run the cheap ones first and stop at the first failure.

```bash
git diff --name-only origin/main...
```

CI builds with `RUSTFLAGS=-Dwarnings` and `--locked`: any compiler warning fails every Rust job, and a `Cargo.lock` that is out of step with a `Cargo.toml` fails before anything compiles.

## What to run

| Changed | Run | CI job |
|---|---|---|
| anything | `cargo fmt --all -- --check` | `fmt` |
| `*.rs`, `Cargo.toml`, `Cargo.lock`, `tests/`, `examples/` | `cargo clippy --workspace --all-targets --locked -- -D warnings` | `clippy` |
| doc comments, public items | `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked` (a broken intra-doc link fails) | `rustdoc` |
| Rust code | tests, see below | `test` |
| a newer std API or syntax | `cargo +1.89 test --workspace --locked --no-run` (needs `rustup toolchain install 1.89`) | `msrv` |
| `Cargo.lock` | `python3 -I scripts/third-party-rust.py --check` and `cargo deny check licenses bans sources`: see the `deps` skill | `release-files`, `deny` |
| `docs/`, `README.md`, `CHANGELOG.md`, `site/`, `schema.rs`, an MCP tool | docs guards, see below | `Docs` |
| `crates/atalanta-inspector/frontend/` | `npm ci && npm test && npm run build` there | `frontend` |
| `*.sh`, `scripts/hooks/*` | `shellcheck -x -P SCRIPTDIR <files>` | `release-files` |
| `.github/`, `action.yml` | `actionlint`; every `uses:` pinned to a 40-character commit SHA; `RUST_VERSION` the same in every workflow that sets it | `actionlint`, `release-files` |
| `unsafe` added | `python3 -I scripts/unsafe-inventory.py --check` | `Security` (`unsafe`) |
| `probes/android/atalanta-tree-server/`, the jar | `scripts/check-tree-jar.sh`: see the `tree-server` skill | `tree-jar` |
| the workspace version, `Formula/` | `scripts/render-formula.sh <version> \| diff -u Formula/atalanta.rb -` | `release-files` |

## Tests

While working, test the crate you touched:

```bash
cargo test -p atalanta-core            # or: cargo nextest run -p atalanta-core
```

Before the PR, run what CI runs. Nextest runs one process per test and applies `.config/nextest.toml` (the Chrome test group, the 60 s slow timeout); it does not run doc tests, so those are a second command.

```bash
cargo nextest run --workspace --locked --profile ci    # cargo install --locked cargo-nextest
cargo test --workspace --locked --doc
```

Without nextest, `cargo test --workspace --locked` runs the same tests in shared processes.

Three things pass locally and fail in CI:

- **No Chrome.** About 150 `atalanta-web` tests skip themselves and report a pass. CI greps `target/nextest/ci/junit.xml` for `skipped: no Chrome` and fails on any. Run them with Chrome installed, or say they were not run.
- **A pass on retry.** The `ci` profile retries once and records the test as flaky. A flaky test only reports on a PR and fails the job on `main`. Do not push one: find why it failed the first time (`flake-triage` is for device suites; for a Rust test, rerun it alone in a loop: `for i in $(seq 20); do cargo test -p <crate> <test> || break; done`).
- **The MCP schema budget.** `cargo test -p atalanta-mcp-server --lib core_profile_schema_budget -- --nocapture` fails when the core tools' schemas grow past their size budget.

## Docs guards

`.github/workflows/docs.yml` runs these, then builds the site:

```bash
cargo test -p atalanta-core --locked --test doc_examples --test dsl_reference --test llms_txt
cargo test -p atalanta-mcp-server --locked --lib the_mcp_doc_lists_every_tool
cd site && npm ci && npm run build
```

`docs/DSL-REFERENCE.md` is generated: after a `schema.rs` change, `ATALANTA_BLESS=1 cargo test -p atalanta-core --test dsl_reference` rewrites it. The site build fails on a relative link to a file that does not exist.

## Git hooks

`scripts/install-hooks.sh` points git at `scripts/hooks/`. The pre-commit hook checks staged files (gitleaks, `cargo fmt --check`, the unsafe baseline, shellcheck, actionlint and zizmor); it runs no clippy and no tests. The pre-push hook scans the pushed commits for secrets and, when a lockfile changed, runs `scripts/security-check.sh deny osv npm-audit`.

## Report

List each check run with its result, then the ones that applied and were not run, with the reason (no Chrome, no 1.89 toolchain, no device). A check that was skipped is not a pass.
