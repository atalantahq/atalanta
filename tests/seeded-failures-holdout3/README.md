# Seeded failures, held-out set 3

20 short suites (10 Android, 10 iOS), each with one planted defect whose real cause is in `manifest.yaml`.
They were written from the apps and the DSL only, independent of the earlier seeded and holdout sets, to measure
failure reports on cases nobody tuned against.

- `base/`: 6 passing suites (3 per platform) the seeds were cut from. Each passed twice on a device.
- `android/`, `ios/`: the seeds, named `<nn>-<slug>.yaml`, test name `holdout3_<platform>_<nn>`, tag `seeded-failure`.
- `manifest.yaml`: per seed the category, planted cause, failing step, and what a correct or wrong report says,
  plus the scoring rules (correct, insufficient, wrong, healed, no-failure), fixed before any run.

Categories (2 per platform each): selector-typo, wrong-screen, wrong-expected-value, wrong-app, stale-state.

Devices: Android emulator API 33 (`emulator-5556`), iPhone 17 simulator, iOS 26. Read-only navigation of the Settings apps;
the wrong-app seeds only launch Clock, Contacts or Files and never act in them.

Run one: `velocity run tests/seeded-failures-holdout3/android/01-services-row.yaml -p android -d <device>`.
Validate all: `velocity validate tests/seeded-failures-holdout3/android` (and `ios`).
Seeds were validated but not run when written.
