# Velocity in CI

Exit codes are listed in [CLI.md](CLI.md#exit-codes): 1 means a test failed, 2 a usage or config error, 3 a device or infrastructure problem, 4 the suite timeout, 5 an internal error. A pipeline can retry on 3 and fail on everything else.

## Sharing history between jobs (no server)

Velocity keeps `velocity-history.json` in the artifacts folder. It holds
per-test durations (for `--shard-timings` and longest-first ordering),
recent outcomes (flaky flags, `--last-failed`, `--changed-since`), and
outcomes per device profile. On CI each job starts with an empty
workspace, so the history has to travel as an artifact:

1. Every job uploads its `velocity-results/velocity-history.json`.
2. The next run downloads them all, and either
   - reads them directly with `velocity run --history-from 'history/*'`
     (repeatable; a file, a folder holding `velocity-history.json`, or a
     glob where `*` matches within one path component and `**` matches any
     number of folders), or
   - merges them once with `velocity history merge 'history/*' -o
     velocity-results` and uploads the merged file for later runs.

**Merge rule.** Jobs usually start from the same base, so outcome lists
are not concatenated (that would count the base once per job). Per test,
the file that ran it most recently (its `last_run`) supplies its
duration, outcomes and resource baseline. That is the shard that ran it.
Device-profile outcomes (`byDevice`) are taken profile by profile, each
from the file in which *that profile* ran the test most recently. History
records that time per profile (`byDeviceLastRun`). This matters because
every matrix job starts from the same base, so each job's file also holds
an unchanged base copy of the other devices' profiles. Ranking a profile
by the test-wide `last_run` would let the job that finished last overwrite
the other devices' new outcomes with its stale copies. A file with no
profile times for a test at all (written by a Velocity without
`byDeviceLastRun`) uses its test-wide `last_run` for each of that test's
profiles; a file with times for some profiles but not this one ranks
below any file that has one, then by its `last_run`. The merged file
carries a time for every profile, and a newer Velocity that updates an
older file writes down a time (the test's previous run) for every
profile it already holds. On a tie, the later file given wins.

**Mixed versions in a matrix.** An older Velocity rewrites its history
file without `byDeviceLastRun`, so a merge can't tell the device that job
ran from its base copies of the others: they all rank by that job's
test-wide run time. A device whose newer job ran *earlier* than the old
job can then lose its new outcomes to the old job's base copy. Run one
Velocity version across all jobs of a matrix (pin it in the install step)
while you roll out an upgrade. For a test run by several matrix jobs in the same pipeline,
the test-wide aggregate (duration, `outcomes`, resource baseline) is the
latest job's. Per-device outcomes keep every device's latest run.

Run times are the job machine's wall clock. A time later than the merging
machine's clock (a runner whose clock is fast) is treated as unknown: it
ranks below every real run time, so a fast runner can't win a test's
outcomes in any merge, even against runs of the same batch. Where it is
written out (a merged `last_run`), it is clamped to the merge time.

`--history-from` merges into the run's own history before anything reads
it. A pattern that matches nothing is a warning, because a first run has
no artifacts. A matched file that can't be read or parsed exits 2.
`velocity history merge` exits 2 when nothing matches at all.

```yaml
jobs:
  test:
    strategy:
      matrix:
        shard: [1, 2, 3]
    steps:
      - uses: actions/checkout@v4
      # History from the last run on main (any earlier workflow run).
      - uses: dawidd6/action-download-artifact@v6
        with:
          name: velocity-history
          path: history/main
          branch: main
          if_no_artifact_found: ignore
      - run: >
          velocity run tests/app.yaml --shard ${{ matrix.shard }}/3
          --shard-timings history/main --history-from 'history/*'
      - uses: actions/upload-artifact@v4
        if: always()
        with:
          name: history-shard-${{ matrix.shard }}
          path: velocity-results/velocity-history.json

  merge-history:
    needs: test
    if: always()
    runs-on: ubuntu-latest
    steps:
      - uses: actions/download-artifact@v4
        with:
          pattern: history-shard-*
          path: history          # history/history-shard-1/velocity-history.json, …
      - run: velocity history merge 'history/*' -o merged
      - uses: actions/upload-artifact@v4
        with:
          name: velocity-history
          path: merged/velocity-history.json
```

## Cancelled and timed-out jobs

- SIGTERM (a cancelled job) exits 143, SIGINT 130 and SIGHUP 129. The
  reports are still written. JUnit carries the
  `velocity.termination` property (`interrupted:SIGTERM`), and every test
  that did not run has `<skipped message="not run: interrupted by
  SIGTERM"/>`. Every `<skipped>` element has a message.
- With `--report github` (automatic in GitHub Actions), the step summary of
  a run that did not complete starts with a warning banner: the run was
  terminated, interrupted or stopped by `--timeout`, and so many tests did
  not run or finish.
- A stopped run gets 20 s to return its results. If it doesn't (a driver
  call that can't be cancelled), Velocity writes the reports from the tests
  that had finished by then. The rest are reported as `not run: … (the
  run did not stop within 20s)`. Then it cleans up and exits.

## Screenshot baselines

`assertScreenshot` never passes on a baseline nobody committed. A missing baseline is written and the step fails, on CI as on a laptop. A mismatch leaves `diffs/<name>-expected|actual|diff.png` and `<name>-review.json` in the artifacts folder, so upload `velocity-results/` with `if: always()`. To review it:

1. Download the artifact into `velocity-results/` and run `velocity baselines review`, or `velocity baselines review --serve --baselines-dir path/to/suite/velocity-baselines` for the page with Accept and Reject buttons (without `--baselines-dir` the page's Accept writes to the runner's recorded path, which doesn't exist here).
2. Run `velocity baselines accept <name|all> --baselines-dir path/to/suite/velocity-baselines`. The record holds the runner's absolute path, so `--baselines-dir` writes into your checkout at the same `<platform>/<W>x<H>/<name>.png`. Without the flag, it writes to the recorded path. Either way the target must resolve inside that `velocity-baselines` folder with no symlinked folder on the way, and a baseline changed after the comparison was recorded (a later run passed or re-recorded it, or it changed upstream) is skipped with a warning rather than overwritten with the older frame. "Changed" is judged by content: the review record holds the SHA-256 of the baseline the run compared against (or `absent`), and the target (with `--baselines-dir`, the file in your checkout) must still have it. Checkouts, clones and `unzip` change modification times, not content, so CI artifacts accept cleanly. Records written by an older Velocity have no hash and fall back to comparing modification times, with a warning. A run's pass clears only the record of the baseline it passed on (same size folder and platform), not another device's record of the same file name.
3. Commit the baseline files.

## Secrets

Secret values (see the README) are masked in everything Velocity prints
and writes. Every line of a multi-line secret (a PEM key, a JSON
credential) is also masked on its own, because output is printed a line at
a time. In GitHub Actions, `::add-mask::` is sent once per line of 8 or more
characters, since GitHub masks single lines only.

## Colour

One switch decides colour for everything Velocity prints: the CLI, the
runner's result lines and the log lines on stderr.

1. `--color always|never` (any command) wins.
2. `NO_COLOR` set to a non-empty value turns colour off.
3. `CLICOLOR_FORCE` set (not `0`) turns it on, even into a pipe.
4. `CLICOLOR=0` or `TERM=dumb` turns it off.
5. Otherwise colour is on when the output is a terminal.

A `velocity run --daemon` client's decision is used for its run.

## Opt-in workflows

These workflows need devices or other toolchains. They never run on an
ordinary push or pull request. Run them from the Actions tab
(`workflow_dispatch`), or add a label to a pull request:

| Workflow | Label | What it does |
|---|---|---|
| `device-smoke.yml` | `device-ci` | iOS: boots an iPhone simulator on `macos-latest` and runs `tests/smoke/ios-settings.yaml` against the Settings app. With the `app_url` input (a zipped simulator `.app`) and `suite`, it runs that suite against your build instead. Android: an API 34 emulator through `reactivecircus/android-emulator-runner`, running `tests/smoke/android-settings.yaml`. |
| `flutter-bridge.yml` | `flutter-ci` | The Dart bridge package (`analyze` + `test`) on Flutter 3.19.6, the SDK floor, and on current stable. Also `cargo test -p velocity-flutter-bridge`. |
| `probes.yml` | `probes-ci` | Android: the sync probe's host checks, the tree server's `build.sh` (host checks and the d8 jar), and a Gradle build of the sync probe (AGP 8.3 and Kotlin 1.9, through a throwaway settings file). iOS: the sync probe's swiftc host checks, an `xcodebuild` of the Swift package, and a `build-for-testing` of VelocityRunner. |

These workflows have been checked for YAML syntax only. They have not
been run on GitHub's runners yet.
