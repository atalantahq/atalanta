---
name: release
description: Cut a Velocity release — version bump, changelog, tag, Homebrew formula and installer update. Use when the user asks to release, ship, or publish a new version.
---

# Release Velocity

1. Preflight (all must pass before anything else):
   - `cargo fmt --all -- --check`
   - `cargo clippy --workspace --all-targets -- -D warnings`
   - `cargo test --workspace`
   - Run the `/verify-e2e` skill on at least one platform.
2. Bump the version in the workspace `Cargo.toml` (and any crate that pins its own version); run `cargo build` once so `Cargo.lock` updates.
3. Update docs that state the version: `README.md`, and check `Formula/velocity.rb` + `install.sh` for hardcoded versions/URLs/checksums.
4. Write a changelog entry from `git log <last-tag>..HEAD --oneline`, grouped feat/fix/perf. If no CHANGELOG.md exists, create one.
5. Commit as `release: vX.Y.Z`, tag `vX.Y.Z`. Do NOT push the tag or publish without explicit user confirmation — pushing a tag triggers the release GitHub Actions workflow (see `.github/workflows/`).
6. After the release workflow builds artifacts, update the Homebrew formula sha256 to match the released tarball.
