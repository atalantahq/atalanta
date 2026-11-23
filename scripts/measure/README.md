# Measurement scripts

Scripts that measure, on a real device, emulator or simulator, numbers that
Atalanta's design relies on but that nobody has measured on hardware. Run them
yourself when you want those numbers for your own device or app. Each prints
numbers; none passes or fails on a threshold.

**Status: never run.** None of them was run when they were written, because no
device was available. The suites pass `atalanta validate` and the scripts pass
`bash -n`, nothing more.

| Script | Measures | Suite |
|---|---|---|
| `compose-testtag.sh` | Cost per full tree dump of reading Compose testTags (`android.composeTestTag` 1 vs 0), first dump and cached | `suites/compose-testtag.yaml` |
| `strict-actionability.sh` | Cost per tap of `selectors.strict` and of the actionability wait (`actionableWaitMs`) | `suites/strict-actionability.yaml` |
| `native-settle.sh` | Settle time per action: `sync.mode` native (the in-app probe) vs polling vs none | `suites/native-settle.yaml` |
| `demo-mode.sh` | Verifies `screenshots.freezeStatusBar`: two exact status-bar compares a minute apart, frozen and an unfrozen control, plus the device's demo-mode state during and after | `suites/demo-mode.yaml` |

## Environment

Set these before running a script (see `lib.sh`):

| Variable | Meaning |
|---|---|
| `ATALANTA` | Path to the `atalanta` binary |
| `DEVICE` | Device serial or UDID |
| `PLATFORM` | `android` or `ios` |
| `APP_ID` | Replaces the suites' `appId` |
| `ROUNDS` | Runs per variant (default 5) |
| `OUT` | Results folder (default `./atalanta-measure/<script>-<time>`) |
| `MEASURE_TEXT`, `MEASURE_TAP`, `MEASURE_BACK_TO` | Texts the suites use; the defaults fit the Android emulator's Settings app |
| `FRAMEWORK` | `native-settle.sh` only: `react_native` turns on the probe's React Native trackers |

`native-settle.sh` needs an app built with the sync probe.

## Output

Each variant's runs land in `<OUT>/<variant>/<n>/results.json`, plus the run's
`stdout.log`. `summarize.py <OUT> <variant>...` prints them again.
