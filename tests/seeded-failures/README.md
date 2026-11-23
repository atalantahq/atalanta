# Seeded failures

30 small suites (15 iOS, 15 Android) that each fail on purpose, with one planted
defect and a known cause. They measure how often Atalanta's failure report
(`atalanta-results/failures.md` and the CLI reason line) is confidently wrong:
it names a specific cause that is not the real one. Use them when you change
failure reporting or want to check how it was scored.

**Every suite here is expected to fail.** Never add this directory to a normal
test run, a CI glob or `atalanta run tests/**`. Repo tests that only parse and
validate `tests/**/*.yaml` are fine: these files are valid suites.

## Rules

- Expected causes live in `manifest.yaml` and were written before any seed was
  run. Do not edit them after seeing output. Scoring rules are at the top of the
  manifest.
- Every seed copies a short passing path from `tests/smoke/*-settings.yaml` and
  changes exactly one thing. Settings app only, read-only navigation.

## Run one seed

iOS needs the iPhone 17 / iOS 26 simulator. Android needs a phone emulator
(API 33+).

1. Run the seed:

       atalanta run tests/seeded-failures/ios/01-selector-typo-general.yaml --artifacts-dir <dir>

2. Read `<dir>/failures.md` (or `atalanta-results/failures.md`).
3. Score it against the seed's entry in `manifest.yaml`.
