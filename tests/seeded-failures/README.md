# Seeded failures

30 small suites (15 iOS, 15 Android) that each fail on purpose, with one planted
defect and a known cause. They measure how often Atalanta's failure report
(`atalanta-results/failures.md` and the CLI reason line) is confidently wrong:
it names a specific cause that is not the real one.

Rules:
- Expected causes live in `manifest.yaml` and were written BEFORE any seed was run.
  Do not edit them after seeing output. Scoring rules are at the top of the manifest.
- Every seed copies a short passing path from `tests/smoke/*-settings.yaml` and
  changes exactly one thing. Settings app only, read-only navigation.

Run one seed:

    atalanta run tests/seeded-failures/ios/01-selector-typo-general.yaml --artifacts-dir <dir>

Then read `<dir>/failures.md` (or `atalanta-results/failures.md`) and score it
against the manifest entry. iOS needs the iPhone 17 / iOS 26 simulator, Android a
phone emulator (API 33+).

WARNING: every suite here is EXPECTED TO FAIL. Never add this directory to a normal
test run, a CI glob or `atalanta run tests/**`. Repo tests that only parse and
validate `tests/**/*.yaml` are fine: these files are valid suites.
