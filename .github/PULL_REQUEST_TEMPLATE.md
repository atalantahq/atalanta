## What this changes

<!-- What the change does and why. Link the issue if there is one. -->

## How it was checked

<!-- Commands you ran, and the device, simulator or browser if it touches a driver.
     Say plainly what was not run. -->

## Checklist

- [ ] `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings` passes
- [ ] Tests added or updated, and `cargo test` passes for the crates touched
- [ ] Docs updated where behaviour changed (`docs/DSL-REFERENCE.md` regenerated if the schema changed)
- [ ] Device behaviour that was not run on a device is recorded in `docs/STATUS.md`
- [ ] One line under `## [Unreleased]` in `CHANGELOG.md` for a user-visible change
- [ ] No secrets, private app names or personal data in code, tests, fixtures or screenshots
