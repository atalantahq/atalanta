---
name: release
description: Cut a Velocity release — version bump, changelog, tag, Homebrew formula and installer update. Use when the user asks to release, ship, or publish a new version.
---

# Release Velocity

The release itself is `.github/workflows/release.yml`, triggered by pushing a `vX.Y.Z` tag. Its preflight refuses a tag when the tag, `Cargo.toml` and `CHANGELOG.md` disagree, when the `## [X.Y.Z]` heading still says `- unreleased`, or when `## [Unreleased]` still has entries. The release notes are the `## [X.Y.Z]` section alone (`scripts/changelog-section.sh X.Y.Z`).

1. Preflight (all must pass before anything else):
   - `cargo fmt --all -- --check`
   - `cargo clippy --workspace --all-targets -- -D warnings`
   - `cargo test --workspace`
   - `scripts/check-tree-jar.sh` (the committed tree-server jar rebuilds byte for byte)
   - Run the `/verify-e2e` skill on at least one platform.
2. `scripts/bump-version.sh X.Y.Z --date`. It sets the workspace version and the crates' `=X.Y.Z` pins, updates `Cargo.lock`, moves everything under `## [Unreleased]` into `## [X.Y.Z]` and stamps today's date (UTC) on that heading. Do not bump by hand.
3. Read the new `## [X.Y.Z]` section: it is the release notes. Delete any placeholder paragraph left in it (for 0.2.0: the "Everything planned for 0.2.0 is under [Unreleased] above" note), and leave an empty `## [Unreleased]` heading above it.
4. Commit as `release: vX.Y.Z`, tag `vX.Y.Z`. Do NOT push the tag or publish without explicit user confirmation — pushing a tag starts the release workflow.
5. The workflow renders and pushes the Homebrew formula with the released checksums (`scripts/render-formula.sh`); do not edit `Formula/velocity.rb` by hand. `install.sh` has no hard-coded version. After the release, `install-smoke.yml` installs it through install.sh, Homebrew, npm and cargo.

What is pinned, so the release is built the way CI tested it:

- **Actions**: every `uses:` in `.github/workflows/` is pinned to a commit SHA (the tag is in a comment). Update a pin to a new SHA, never to a tag.
- **Rust toolchain**: `RUST_VERSION: "1.98.1"` in every workflow that builds Velocity, release included; ci.yml checks they agree.
- **Tree-server jar**: built with the Android SDK pieces in `probes/android/velocity-tree-server/toolchain.env` (build-tools 36.1.0, android-36, JDK 17). `build.sh` uses them by default and `scripts/check-tree-jar.sh` rebuilds with them. Changing them means rebuilding and committing the jar in the same change.
- **macOS binaries** are ad-hoc signed, not notarised. `install.sh` removes the quarantine flag only if it is there, and says so (`--keep-quarantine` keeps it).
