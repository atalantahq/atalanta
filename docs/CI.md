# Atalanta in CI

Contents:

- [Exit codes in a pipeline](#exit-codes-in-a-pipeline)
- [The Atalanta Action](#the-atalanta-action)
- [Sharding](#sharding)
- [No silent passes](#no-silent-passes)
- [Sharing history between jobs](#sharing-history-between-jobs-no-server)
- [Cancelled and timed-out jobs](#cancelled-and-timed-out-jobs)
- [Secrets](#secrets)

## Exit codes in a pipeline

The full table is in [CLI.md](CLI.md#exit-codes). A pipeline needs only these:

| Code | Meaning | Pipeline action |
|---|---|---|
| 0 | Everything passed | Continue |
| 1 | A test failed | Fail the job |
| 2 | Usage or configuration error | Fail the job |
| 3 | Device or infrastructure problem | Retry the job |
| 4 | Suite timeout (`--timeout`) | Fail the job |
| 5 | Internal error | Fail the job |
| 129, 130, 143 | SIGHUP, SIGINT, SIGTERM | See [cancelled jobs](#cancelled-and-timed-out-jobs) |

A pipeline can retry on 3 and fail on everything else.

## The Atalanta Action

The Atalanta Action installs a pinned release of Atalanta on a GitHub runner and runs a suite. `atalanta init --ci github` writes `.github/workflows/atalanta.yml`, which runs the suite in two shards through the Action and then a job that checks every shard reported. Without `--ci`, `init` writes the same workflow as an example under the suite's `ci/` folder.

```yaml
      - uses: atalantahq/atalanta@v0.2.0
        with:
          version: 0.2.0               # the release to install; pin it
          platform: android            # android, ios or web
          suite: tests/
          shard: ${{ matrix.shard }}/4 # optional
          # sha256: <archive sha256>   # optional: pin the exact build
          # args: --app app.apk --retries 1
```

- **Install.** The Action runs its own `install.sh`, the one from the same tag, with the version given. The installer checks the archive against the release's `SHA256SUMS`, and against `sha256` when given. Nothing is fetched from `main`.
- **Android.** The Action turns on KVM and caches the AVD and its snapshot. The cache key is made of the runner OS, API level, image target and architecture. The suite runs inside `reactivecircus/android-emulator-runner`, and its first step there records that the emulator booted. When the emulator never booted, the Action tries once more with a fresh AVD (`boot-attempts`, at most 2). If that fails too, the job exits **3**. That is an infrastructure failure, never a test failure, so a pipeline can retry 3 and treat every other code as final.
- **iOS.** `atalanta devices up --ios 1` boots a simulator and is retried `boot-attempts` times. A simulator that never boots is exit 3 too.
- **Web.** Headless Chrome. There is nothing to boot.
- **Results.** The results folder is uploaded as `atalanta-results-<platform>-shard-M-of-N` (or `-all`), and the job's exit code is Atalanta's.

Inputs:

| Input | Required | Default | Meaning |
|---|---|---|---|
| `version` | yes | | Release to install (X.Y.Z) |
| `platform` | yes | | `android`, `ios` or `web` |
| `sha256` | no | | Checksum of the archive, to pin the exact build |
| `suite` | no | | Suite file or folder (not used in check mode) |
| `shard` | no | | `M/N` |
| `args` | no | | More `atalanta run` flags, split on spaces |
| `results-dir` | no | `atalanta-results` | Where the reports go |
| `upload` | no | `true` | Upload the results folder as an artifact |
| `api-level` | no | `34` | Android emulator API level |
| `target` | no | `google_apis` | Android system image target |
| `arch` | no | `x86_64` | Android emulator architecture |
| `boot-attempts` | no | `2` | Tries to bring the device up before exit 3 |
| `check-shards` | no | | Check mode: the number of shards a matrix ran. The Action installs Atalanta only, runs no tests, and fails unless every shard reported |

## Sharding

Sharding splits one suite across several CI jobs, each running a part of it.

- `--shard M/N` (1-based) runs the Mth of N shards. `--shard-index` and `--shard-total` are the 0-based form.
- Without timing data, tests are balanced by count, with a stable hash for tie-breaks.
- `--shard-timings FILE` balances by duration, from a history JSON or JUnit XML. Give every shard the same file, or the shards will disagree on the partition.
- `atalanta shard-plan --total N` shows each shard's tests and the plan fingerprint without a device.
- After the matrix, a [check job](#no-silent-passes) confirms that every shard reported.


## No silent passes

A run that tests nothing must not go green. Atalanta refuses these cases:

- **A selection that matches nothing** (`--tags`, `--filter` typos): exit 2.
- **A shard out of range** (`--shard 3/2`): exit 2.
- **A shard that owns no test** (more shards than tests, or dependency groups that cannot be split): exit 2, naming the shard. Selection after the partition (`--last-failed`, `--changed-since`) can still leave a shard empty on purpose. `--fail-if-none` covers that.
- **A planned test with no result** (a worker that stopped, a bug): it is reported failed as an internal error (exit 5), never dropped.

A single job cannot see a job that died before uploading, or one that uploaded an empty or damaged folder. With the Atalanta Action, use check mode instead of writing that job by hand. After the sharded matrix, add a `results` job that sets `check-shards` to the matrix size:

```yaml
  results:                       # after the matrix: every shard reported?
    needs: tests                 # the sharded matrix
    if: always()                 # also when a shard failed or was cancelled
    runs-on: ubuntu-latest
    steps:
      - uses: atalantahq/atalanta@v0.2.0
        with: { version: 0.2.0, platform: android, check-shards: 4 }
```

In check mode the Action installs Atalanta only, runs no tests, downloads every shard's `atalanta-results-<platform>-shard-M-of-N` artifact and runs `atalanta results check --expect-shards N`. The check exits 1 when any of these hold:

- Fewer or more results files than `--expect-shards` arrived.
- A shard number is missing or appears twice.
- A shard ran no test.
- A `results.json` does not parse, or has no `junit.xml` with a test suite next to it.
- The shards disagree on the partition. Each shard records its plan's fingerprint in `results.json` (`shard_plan`). Two different fingerprints (different checkouts, filters or `--shard-timings` files) mean a test may have run twice or not at all.
- With one plan, a test is planned in two shards, or the plans do not add up to the whole.

On other CI systems, or without the Action, download every shard's artifacts and check them the same way:

```yaml
  results:
    needs: test            # the sharded matrix
    if: always()           # also when a shard failed or was cancelled
    runs-on: ubuntu-latest
    steps:
      - uses: actions/download-artifact@v4
        with: { path: shards }
      - run: atalanta results check --expect-shards 4 shards
```

## Sharing history between jobs (no server)

Atalanta keeps `atalanta-history.json` in the artifacts folder. It holds:

- per-test durations, for `--shard-timings` and longest-first ordering;
- recent outcomes, for flaky flags, `--last-failed` and `--changed-since`;
- outcomes per device profile.

On CI each job starts with an empty workspace, so the history has to travel as an artifact:

1. Every job uploads its `atalanta-results/atalanta-history.json`.
2. The next run downloads them all, and either
   - reads them directly with `atalanta run --history-from 'history/*'`
     (repeatable; a file, a folder holding `atalanta-history.json`, or a
     glob where `*` matches within one path component and `**` matches any
     number of folders), or
   - merges them once with `atalanta history merge 'history/*' -o
     atalanta-results` and uploads the merged file for later runs.

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
profile times for a test at all (written by an Atalanta without
`byDeviceLastRun`) uses its test-wide `last_run` for each of that test's
profiles; a file with times for some profiles but not this one ranks
below any file that has one, then by its `last_run`. The merged file
carries a time for every profile, and a newer Atalanta that updates an
older file writes down a time (the test's previous run) for every
profile it already holds. On a tie, the later file given wins.

**Mixed versions in a matrix.** An older Atalanta rewrites its history
file without `byDeviceLastRun`, so a merge can't tell the device that job
ran from its base copies of the others: they all rank by that job's
test-wide run time. A device whose newer job ran *earlier* than the old
job can then lose its new outcomes to the old job's base copy. Run one
Atalanta version across all jobs of a matrix (pin it in the install step)
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
`atalanta history merge` exits 2 when nothing matches at all.

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
          name: atalanta-history
          path: history/main
          branch: main
          if_no_artifact_found: ignore
      - run: >
          atalanta run tests/app.yaml --shard ${{ matrix.shard }}/3
          --shard-timings history/main --history-from 'history/*'
      - uses: actions/upload-artifact@v4
        if: always()
        with:
          name: history-shard-${{ matrix.shard }}
          path: atalanta-results/atalanta-history.json

  merge-history:
    needs: test
    if: always()
    runs-on: ubuntu-latest
    steps:
      - uses: actions/download-artifact@v4
        with:
          pattern: history-shard-*
          path: history          # history/history-shard-1/atalanta-history.json, …
      - run: atalanta history merge 'history/*' -o merged
      - uses: actions/upload-artifact@v4
        with:
          name: atalanta-history
          path: merged/atalanta-history.json
```

### Rolling flake rate

After the merge, gate on each test's flake rate so that a creeping flake fails the pipeline:

```yaml
      - run: atalanta history flaky --file merged/atalanta-history.json --max-rate 0.05
```

`atalanta history flaky` needs no device, so it works as a CI gate. It does the following:

- Prints each test's flakiness over its last 20 runs (`--window`), worst first. Flakiness counts runs that passed only on retry plus pass/fail flips between consecutive runs, the same measure `atalanta run` reports.
- Marks tests above `--max-rate` with `*`.
- Exits 1 when any test with at least 5 runs exceeds `--max-rate`. Tests with fewer than 5 runs get a rate but never trip the threshold.
- Exits 2 when `--window` is under 5 or `--max-rate` is outside 0..1.

Use `--json` to feed the result to another step. The JSON output carries a `flips` field too.

## Cancelled and timed-out jobs

- A cancelled job sends SIGTERM, and Atalanta exits 143. SIGINT exits 130 and SIGHUP 129. The
  reports are still written. JUnit carries the
  `atalanta.termination` property (`interrupted:SIGTERM`), and every test
  that did not run has `<skipped message="not run: interrupted by
  SIGTERM"/>`. Every `<skipped>` element has a message.
- With `--report github` (automatic in GitHub Actions), the step summary of
  a run that did not complete starts with a warning banner: the run was
  terminated, interrupted or stopped by `--timeout`, and so many tests did
  not run or finish.
- A stopped run gets 20 s to return its results. If it doesn't (a driver
  call that can't be cancelled), Atalanta writes the reports from the tests
  that had finished by then. The rest are reported as `not run: … (the
  run did not stop within 20s)`. Then it cleans up and exits.
- Timeouts nest, from the innermost out:
  1. A step's own `timeoutMs`.
  2. `--test-timeout`: one attempt of one test. It fails that test (exit 1) and the run goes on.
  3. `--timeout`: the whole run. It exits 4 and skips the rest.
  4. The CI job's own limit: SIGTERM, exit 143.

  Set `--test-timeout` well under `--timeout` so one stuck test cannot use up the job.
- `atalanta run --daemon` waits its turn when the daemon is busy with
  another run, and says so (`=> queued: …`) after 2 s. A daemon that dies
  mid-run makes the client exit 3, so retry-on-3 pipelines retry it.

## Screenshot baselines

`assertScreenshot` never passes on a baseline nobody committed. A missing baseline is written and the step fails, on CI as on a laptop. A mismatch leaves `diffs/<name>-expected|actual|diff.png` and `<name>-review.json` in the artifacts folder, so upload `atalanta-results/` with `if: always()`. To review it:

1. Download the artifact into `atalanta-results/` and run `atalanta baselines review`, or `atalanta baselines review --serve --baselines-dir path/to/suite/atalanta-baselines` for the page with Accept and Reject buttons (without `--baselines-dir` the page's Accept writes to the runner's recorded path, which doesn't exist here).
2. Run `atalanta baselines accept <name|all> --baselines-dir path/to/suite/atalanta-baselines`. The record holds the runner's absolute path, so `--baselines-dir` writes into your checkout at the same `<platform>/<W>x<H>/<name>.png`. Without the flag, it writes to the recorded path. Either way the target must resolve inside that `atalanta-baselines` folder with no symlinked folder on the way, and a baseline changed after the comparison was recorded (a later run passed or re-recorded it, or it changed upstream) is skipped with a warning rather than overwritten with the older frame. "Changed" is judged by content: the review record holds the SHA-256 of the baseline the run compared against (or `absent`), and the target (with `--baselines-dir`, the file in your checkout) must still have it. Checkouts, clones and `unzip` change modification times, not content, so CI artifacts accept cleanly. Records written by an older Atalanta have no hash and fall back to comparing modification times, with a warning. A run's pass clears only the record of the baseline it passed on (same size folder and platform), not another device's record of the same file name.
3. Commit the baseline files.

## Secrets

Secret values are masked in everything Atalanta prints and writes: logs, reports, traces and events. Secrets reach a suite as variables (`${NAME}`), so a pipeline passes them as environment variables from its own secret store. Never write a secret into a suite file. See the README and [DSL.md](DSL.md#logging-in) for how a suite names a secret.

- Every line of a multi-line secret (a PEM key, a JSON credential) is also masked on its own, because output is printed a line at a time.
- In GitHub Actions, `::add-mask::` is sent once per line of 8 or more characters, because GitHub masks single lines only.
- `--env-file PATH` loads `KEY=VALUE` lines and is never loaded unless named ([CLI.md](CLI.md#execution)).
- The Atalanta Action passes its inputs to the installer as variables, never spliced into a script.

## Colour

One switch decides colour for everything Atalanta prints: the CLI, the
runner's result lines and the log lines on stderr. The first rule below that applies wins.

1. `--color always|never` (any command) wins.
2. `NO_COLOR` set to a non-empty value turns colour off.
3. `CLICOLOR_FORCE` set (not `0`) turns it on, even into a pipe.
4. `CLICOLOR=0` or `TERM=dumb` turns it off.
5. Otherwise colour is on when the output is a terminal.

An `atalanta run --daemon` client's decision is used for its run.

## Atalanta's own workflows

The workflows in Atalanta's own repository (`.github/workflows/`). You don't need this to use the tool.

- Every job starts with `step-security/harden-runner` (egress audit).
- Every job checks out without persisting credentials, except two that need them: ci.yml `Changes` for paths-filter's `git fetch`, and release.yml's tap push.
- Every action is pinned to a commit SHA.

| Workflow | When | Jobs |
|---|---|---|
| `ci.yml` | pull requests, pushes to `main`, merge queue | `Changes`, `Inspector frontend`, `Format`, `Clippy`, `Rustdoc`, `Test (ubuntu, macos)`, `MSRV (1.89)`, `Release build` (push only), `Release files` (shellcheck, changelog and formula, protocol numbers, one toolchain, SHA pins, licence inventory, packaging), `Tree-server jar reproduces`, `Bench scorer tests`, `cargo-deny (licenses bans sources)`, `Workflow lint` (actionlint), and `CI OK`, the one required check |
| `security.yml` | pull requests touching code, dependencies or workflows; pushes to `main`; Mondays; manual | `RustSec advisories (cargo-deny)`, `OSV-Scanner`, `npm audit`, `Secrets (gitleaks)`, `Workflow audit (zizmor)`, `SAST (Semgrep)`, `Unsafe code inventory`, `SBOM (CycloneDX)`; scheduled and manual runs add `Fuzz (<target>)`, a minute per cargo-fuzz target. Not part of `CI OK`. See [SECURITY.md](../SECURITY.md#automated-security-checks) |
| `scorecard.yml` | pushes to `main`, Mondays, manual | `Scorecard analysis` (OpenSSF Scorecard; SARIF artifact and job summary, not published while the repository is private) |
| `canary.yml` | Mondays, manual | `Clippy and tests (stable, beta)`: the toolchains after the pinned one |
| `release.yml` | `v*` tags | `Preflight`, `Inspector frontend`, `Build (4 targets)` (with `cargo auditable`), `SBOM`, `GitHub release` (SHA256SUMS over archives and SBOMs, provenance attestations, `SHA256SUMS.sigstore.json`), then the opt-in publish jobs |
| `install-smoke.yml` | after a release | installs each published route and runs an API-only suite |
| `docs.yml` | docs pull requests, releases | builds and deploys the docs site |
| `device-smoke.yml`, `flutter-bridge.yml`, `probes.yml` | nightly / labels / manual | see below |

## Opt-in workflows

These workflows in Atalanta's repository need devices or other toolchains. They never run on an
ordinary push or pull request. Run them from the Actions tab
(`workflow_dispatch`), or add a label to a pull request:

| Workflow | Label | What it does |
|---|---|---|
| `device-smoke.yml` | `device-ci` | iOS: boots an iPhone simulator on `macos-latest` and runs `tests/smoke/ios-settings.yaml` against the Settings app. With the `app_url` input (a zipped simulator `.app`) and `suite`, it runs that suite against your build instead. Android: an API 34 emulator through `reactivecircus/android-emulator-runner`, running `tests/smoke/android-settings.yaml`. Web: headless Chrome, running `tests/smoke/web-smoke.yaml`. Nightly, Android and web run on a schedule (iOS on Mondays) and a final job checks that both reported. |
| `flutter-bridge.yml` | `flutter-ci` | The Dart bridge package (`analyze` + `test`) on Flutter 3.19.6, the SDK floor, and on current stable. Also `cargo test -p atalanta-flutter-bridge`. |
| `probes.yml` | `probes-ci` | Android: the sync probe's host checks, the tree server's `build.sh` (host checks and the d8 jar), and a Gradle build of the sync probe (AGP 8.3 and Kotlin 1.9, through a throwaway settings file). iOS: the sync probe's swiftc host checks, an `xcodebuild` of the Swift package, and a `build-for-testing` of AtalantaRunner. |

Unverified: these workflows have been checked for YAML syntax only. They have not
been run on GitHub's runners.
