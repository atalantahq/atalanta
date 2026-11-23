---
name: deps
description: Add, update or remove a Cargo dependency in the Atalanta workspace the way CI expects — declared once in [workspace.dependencies], rustls not OpenSSL, the licence inventory regenerated, cargo-deny's licence, duplicate-version and source rules, and RustSec/OSV advisory ignores kept in step. Use when editing a Cargo.toml or Cargo.lock, on a Dependabot PR, or when the `deny`, `release-files` (third-party licence inventory), `advisories` or `osv` check is red.
---

# Dependencies

Atalanta ships as one static binary, so every crate in `Cargo.lock` is shipped code and shipped licence text. Before adding one, check whether a crate already in `[workspace.dependencies]` or the standard library does the job.

## Adding or updating

1. **Declare it once.** Put the version and features in `[workspace.dependencies]` in the root `Cargo.toml`; the crate that uses it says `name = { workspace = true }`. Add a comment there when the feature set is a decision (see `reqwest`).
2. **No OpenSSL.** `deny.toml` bans `openssl-sys` and `native-tls`. For anything that does TLS, turn default features off and pick the rustls feature, as `reqwest` does.
3. **Update the lockfile** with a build (`cargo check -p <crate>`), or `cargo update -p <name>` for an update. CI passes `--locked` everywhere, so commit `Cargo.lock` with the `Cargo.toml` change.
4. **Regenerate the licence inventory**, `docs/licenses/THIRD-PARTY-RUST.md`. It needs only cargo:

   ```bash
   python3 scripts/third-party-rust.py --write
   ```

   CI runs `--check` in the `release-files` job and fails on a stale file, Dependabot PRs included.
5. **Run cargo-deny**, as the `deny` job does:

   ```bash
   cargo deny check licenses bans sources
   ```

   | It fails with | Do |
   |---|---|
   | a licence not on the allow-list | Stop and ask. The list in `deny.toml` is the permissive licences that already occur; a copyleft or unlicensed crate needs a person to decide. A new permissive licence goes in `allow`, and in section 6 of `NOTICE` |
   | a duplicate version (`multiple-versions = "deny"`) | First try to make the versions agree (`cargo update -p <name> --precise <version>`, or a newer release of the crate that pulls the old one). Only if that is impossible, add a `[bans] skip` entry with the reason, like the ones there |
   | a wildcard version or a git/registry source | Use a crates.io release with a version requirement. Only the crates.io index is allowed |

6. **Check advisories** (the `Security` workflow, not `CI OK`):

   ```bash
   scripts/security-check.sh deny osv
   ```

   Fix an advisory by updating. An ignore is for a notice with no fix available: it goes in both `deny.toml` (`[advisories] ignore`, with a reason) and `osv-scanner.toml` (with an `ignoreUntil` date at most six months out), and the two lists stay in step.

## Things that follow from the dependency

- **MSRV.** The workspace's `rust-version` is 1.89 and CI builds with it. A dependency that needs a newer compiler fails the `msrv` job: `cargo +1.89 test --workspace --locked --no-run`.
- **`unsafe`.** A dependency does not count toward `scripts/unsafe-baseline.txt`; a wrapper you write around it does.
- **Workspace crates** depend on each other through `[workspace.dependencies]` with `path` and `version = "=X.Y.Z"`. `scripts/bump-version.sh` maintains those pins; do not edit them by hand.
- **The fuzz crate** (`fuzz/`) has its own `Cargo.lock` and is not in the workspace.
- **Frontend (npm) dependencies** under `crates/atalanta-inspector/frontend`: `npm audit` runs through `scripts/security-check.sh npm-audit`. `docs/licenses/THIRD-PARTY-NPM.md` and section 5 of `NOTICE` list the bundled packages and are edited by hand; no check covers them, so update both in the same change.

## Removing

Remove the entry from the crate and, if nothing else uses it, from `[workspace.dependencies]`; rebuild so `Cargo.lock` drops it; regenerate the inventory (step 4). Then delete any `deny.toml` `skip` or `ignore` and `osv-scanner.toml` entry that existed only for it.

## Report

Name the crate and version, why an existing dependency would not do, its licence, how many crates it added to `Cargo.lock` (`git diff --stat Cargo.lock`), and the result of steps 4 to 6.
