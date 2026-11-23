# Atalanta vs Maestro, head to head

A script that times one user-visible flow in Atalanta and in Maestro, each run
from its CLI on the same Android emulator and the same app. Use it to check or
repeat the speed comparison. It turns an earlier manual Android comparison
(Maestro 26–27 s, Atalanta 3.7 s) into a script anyone can rerun.

## What is compared

The flow from [tests/smoke/android-settings.yaml](../../tests/smoke/android-settings.yaml),
8 steps on the built-in Settings app, written once for each tool:

| # | Atalanta ([flows/settings.atalanta.yaml](flows/settings.atalanta.yaml)) | Maestro ([flows/settings.maestro.yaml](flows/settings.maestro.yaml)) |
| --- | --- | --- |
| 1 | `stopApp` | `stopApp` |
| 2 | `launchApp` (`clearState: false`) | `launchApp` (`clearState: false`, `stopApp: false`) |
| 3 | `assertVisible: { textContains: "Network" }` | `assertVisible: "Network.*"` |
| 4 | `tap: { textContains: "Network" }` | `tapOn: "Network.*"` |
| 5 | `assertVisible: { text: "Airplane mode" }` | `assertVisible: "Airplane mode"` |
| 6 | `pressKey: { key: back }` | `back` |
| 7 | `assertNotVisible: { text: "Airplane mode" }` | `assertNotVisible: "Airplane mode"` |
| 8 | `assertVisible: { textContains: "Network" }` | `assertVisible: "Network.*"` |

The number is the wall clock of the whole CLI invocation, from process start
to exit: startup, device connection, driver setup, the steps and the report.
That is what a developer or a CI job waits for.

## Requirements

- **Atalanta release binary**: `cargo build --release -p atalanta-cli`
  (`target/release/atalanta`; override with `ATALANTA_BIN`). Debug builds are
  not comparable.
- **Maestro 2.3.0**, pinned: `curl -Ls "https://get.maestro.mobile.dev" | MAESTRO_VERSION=2.3.0 bash`.
  `run.sh` refuses another version unless `MAESTRO_VERSION` is set to it, and
  disables Maestro's analytics.
- **One API 34 Android emulator**, booted and online in `adb devices`
  (Google APIs image, so the Settings home lists "Network & internet").
  The earlier measurement used an arm64 image on Apple Silicon, headless.
  Pass `-d SERIAL` when more than one device is attached.
- **The Settings app** (`com.android.settings`), which the image ships.
- `adb`, `python3` and `bash`. Bash 3.2 is fine; `perl` is used for timing there.

## How to run

```bash
cargo build --release -p atalanta-cli
bench/head-to-head/run.sh                 # N=5, the only online device
bench/head-to-head/run.sh -n 9 -d emulator-5554 -o /tmp/h2h
python3 bench/head-to-head/summarize.py bench/head-to-head/results/<stamp>/results.csv
```

Each run directory (`results/<stamp>/` by default) holds:

- `results.csv`
- `summary.md`
- `env.txt` (tool versions, device, host)
- `logs/`, one log per run
- `artifacts/`, each tool's JUnit report

`results.csv` columns: `phase` (`warmup` or `measured`), `iteration`, `order`
(1 or 2 within the iteration), `tool`, `wall_ms`, `exit_code`, `device`,
`api`, `started_at`.

## Rules

1. **Same device, same app, same flow.** One emulator for the whole run; the
   script checks it is booted, on API 34 and has Settings installed.
2. **Warm-up run discarded.** One run of each tool first (Maestro installs its
   driver APKs, Atalanta pushes its tree server). Recorded as `warmup` and
   ignored by `summarize.py`.
3. **N=5 alternating runs.** Iteration 1 runs Atalanta then Maestro, iteration
   2 Maestro then Atalanta, and so on, so neither tool always runs on the
   device the other just warmed.
4. **Same starting state before every run**: animation scales reset to 1.0,
   Settings force-stopped, home screen, 2 s idle.
5. **Animations as each tool sets them.** Atalanta turns system animations off
   for its run and restores them afterwards. That is its default, and the
   script makes sure `ATALANTA_KEEP_ANIMATIONS` is unset. Maestro leaves them on.
   Each tool is measured as a user gets it out of the box.
6. **No retries.** Atalanta runs with `--retries 0`; Maestro does not retry.
7. **Medians and p90** over the passing measured runs, per tool. A failed run
   is counted (`Passed` column) but not timed. With N=5, p90 sits between the
   two slowest runs; raise `-n` for a tighter tail.

## What is not compared

- **Per-step latency.** Only the end-to-end wall clock is recorded.
- **iOS.** Atalanta's WDA path could not finish the iOS flow in the earlier
  measurement; this harness is Android only.
- **Reliability or flakiness** beyond the pass count of these few runs.
- **Atalanta's daemon** (`run --daemon`, warm drivers between runs) and
  **Maestro's** continuous or Studio modes. Both tools run cold from the CLI.
- **Parallel runs, sharding, cloud services**, real devices, other apps, other
  API levels.
- **Features**: reporting, selectors, healing, AI steps, ecosystem. This is a
  speed number for one flow, nothing more.
- **Animations off for both.** Not the default for Maestro, so not measured
  here; set the scales to 0 in `reset_device` for that variant and say so.
