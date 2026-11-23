---
name: verify-e2e
description: Build the atalanta binary and run a real end-to-end suite against a booted simulator/emulator. Use after any change to sync, selectors, drivers, executor, or dialog handling — unit tests alone don't exercise device I/O.
---

# Verify Atalanta end-to-end

1. Build release binary: `cargo build --release -p atalanta-cli`. Binary lands at `target/release/atalanta`.
2. Check for an available device: `target/release/atalanta device list --platform ios` (or `android`). If no simulator is booted, boot one with `target/release/atalanta device boot <id>` — prefer an iPhone simulator on iOS.
3. Validate first (cheap): `target/release/atalanta validate tests/`.
4. Run the smoke fixtures: `target/release/atalanta run tests/fixtures/simple_login.yaml --artifacts-dir atalanta-results/`.
5. If the change touched sync, selectors, healing, or dialog handling, also run the Settings smoke suite for the platform: `target/release/atalanta run tests/smoke/ios-settings.yaml --artifacts-dir atalanta-results/` (or `android-settings.yaml`), and any app suite you keep outside the repository. Say in your report which suites were NOT run.
6. On failure, inspect `atalanta-results/` artifacts (screenshots, logs) before concluding — distinguish a test-app problem from a framework regression.
7. Report: which suites ran, on which device, pass/fail counts, and total wall time (compare against previous runs if `atalanta-history.json` exists).
