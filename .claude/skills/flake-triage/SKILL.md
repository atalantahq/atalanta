---
name: flake-triage
description: Analyze atalanta test results and history to find flaky tests and classify root causes (sync, selector, dialog, resource). Use when a suite fails intermittently or after a CI run with retries.
---

# Triage flaky Atalanta tests

1. Gather evidence: read the JSON reports and artifacts in `atalanta-results/` (and `../atalanta-results/` at the repo parent, which holds older runs), plus `atalanta-history.json` duration history if present.
2. For each failed/retried test, classify the failure using the error taxonomy in `crates/atalanta-common/src/error.rs`:
   - **Sync flake**: timeout waiting for idle, or action fired mid-animation → check whether the screen has continuous animations (carousels, banners); the polling stability detector is fooled by these. Fix direction: native sync probe coverage or per-step sync config, not longer timeouts.
   - **Selector flake**: `ElementNotFound` on text selectors → check for formatted/dynamic text ("Member since …"). Fix direction: semantic IDs/testTags, or verify `healing.rs` fingerprinting engaged (look for healing log lines).
   - **Dialog blockage**: a step times out and the screenshot shows a system permission/location dialog → verify `dialog_handler.rs` covers that dialog's button labels for the device locale.
   - **Resource/race**: dashboard/data-load failures under parallel runs → compare durations in history; if the same test is slow only when sharded, suspect device contention, not app bugs.
3. Rank by repeat-offender count across runs, not by most recent failure.
4. Report a table: test name, failure class, evidence (artifact path), recommended fix. Do not "fix" a flake by raising a timeout unless the evidence shows the operation legitimately takes longer.
