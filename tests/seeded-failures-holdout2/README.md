# Held-out seeded failures, set 2

Twenty short Atalanta tests (10 Android, 10 iOS), each with exactly one planted
defect whose real cause is written down in `manifest.yaml`. They measure
Atalanta's failure reports on cases nobody tuned against. Every seed is expected
to fail, so use them to score failure reporting, not as regression tests.

- Five categories, two per platform each: selector-typo, wrong-screen,
  wrong-expected-value, wrong-app, stale-state.
- Designed from the Settings apps and the DSL only, blind to the earlier seeded
  sets and their tuning, and written before any seed was run.
- Every seed is a passing base from `base/` plus one change.
- Scoring rules (correct, insufficient, wrong, healed, no-failure) are in the
  manifest header and were fixed before any run.
- Every seed is expected to fail (or heal, which the manifest scoring rules
  cover). A seed that passes is a bad seed, not a pass.
- The set is not part of any test run, CI job or `tests/` glob that expects green.
- The suites use only read-only navigation. wrong-app seeds open Clock and
  Contacts (Android) or Calendar and Reminders (iOS) without changing anything.

Run one with `atalanta run <file> -p <android|ios> -d <device>`.
