# Measurement scripts (P5)

Scripts the owner runs against a real device, emulator or simulator to get
numbers Velocity's design relies on but nobody has measured on hardware.
None of them was run when they were written (no device on that machine):
the suites pass `velocity validate` and the scripts `bash -n`, nothing more.
Each prints numbers; none passes or fails on a threshold.

| Script | Measures | Suite |
|---|---|---|
| `compose-testtag.sh` | Cost per full tree dump of reading Compose testTags (`android.composeTestTag` 1 vs 0), first dump and cached | `suites/compose-testtag.yaml` |
| `strict-actionability.sh` | Cost per tap of `selectors.strict` and of the actionability wait (`actionableWaitMs`) | `suites/strict-actionability.yaml` |
| `native-settle.sh` | Settle time per action: `sync.mode` native (the in-app probe) vs polling vs none | `suites/native-settle.yaml` |
| `demo-mode.sh` | Verifies `screenshots.freezeStatusBar`: two exact status-bar compares a minute apart, frozen and an unfrozen control, plus the device's demo-mode state during and after | `suites/demo-mode.yaml` |

Common environment (see `lib.sh`): `VELOCITY` (binary), `DEVICE`,
`PLATFORM`, `APP_ID` (replaces the suites' `appId`), `ROUNDS` (default 5),
`OUT` (results folder; default `./velocity-measure/<script>-<time>`).
Suite texts come from `MEASURE_TEXT`, `MEASURE_TAP`, `MEASURE_BACK_TO`;
the defaults fit the Android emulator's Settings app. `native-settle.sh`
takes `FRAMEWORK=react_native` for an RN app (turns on the probe's React
Native trackers) and needs an app built with the sync probe.

Each variant's runs land in `<OUT>/<variant>/<n>/results.json` (plus the
run's `stdout.log`); `summarize.py <OUT> <variant>...` prints them again.
