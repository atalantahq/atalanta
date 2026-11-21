# Held-out seeded failures

Twenty short tests (10 iOS, 10 Android), each with exactly one planted defect, used to
measure Velocity's failure reports on cases nobody tuned against.

- Written blind to the first seeded-failure set: neither it nor its docs were read.
- Designed from the Settings apps and the DSL only, on screens the first set does not use.
- The cause, failing step and scoring rules for every seed are fixed in `manifest.yaml`,
  which was written before any seed was run.
- `base/` holds six passing suites (3 per platform). Every seed is a base with one change.
- Categories (2 per platform each): selector-typo, wrong-screen, wrong-expected-value,
  wrong-app, stale-state. File names are neutral and do not name the category.

The seed suites are expected to fail. They are not part of any test run or CI job; run
them by hand, for example:

    velocity run tests/seeded-failures-holdout/ios/03-accessibility.yaml -p ios -d <udid>

Read-only navigation only: no toggles, no typing, no setting changes. Run from a scratch
directory, since velocity writes history and results into the current directory.
