---
name: debug-run
description: Find why one Atalanta test step fails or misbehaves on a device, simulator or browser — read failures.md and the per-test trace (steps.json, the UI tree at each step, video, app log), reproduce the single test with traces on, test the selector live in the REPL, raise RUST_LOG for the layer involved, and decide whether the fault is in the suite, the app or Atalanta. Use when a step times out, an element is "not found" that is visibly there, a tap lands on nothing, a run exits 3 or 5, or the behaviour differs between Android, iOS and web. For a test that fails only sometimes, use flake-triage.
---

# Debug a failing run

Work from the evidence the run already wrote before running anything again. Change nothing in Atalanta until the fault is located; raising a timeout is not a diagnosis.

## 1. Read what the run left

Everything is under `--artifacts-dir` (default `./atalanta-results`):

| File | What it tells you |
|---|---|
| `failures.md` | Written on every run: each failed test, its step and `file:line`, the error, the tail of the app log, and a one-line why. Start here |
| `failure_<test>__<context>_step<i>.png` | The screen when the step failed |
| `traces/<test>/steps.json`, `trees/<hash>.json` | Every step with its timings, and the UI tree Atalanta saw at each one |
| `traces/<test>/trace.html` | The same as a viewer: `atalanta trace view atalanta-results` |
| `traces/<test>/video.mp4`, `app.log`, `device.log` (Android), `console.log` (web) | What the app did around the failure |
| `crashlogs/<test>/` | The app crashed |
| `results.json`, `junit.xml`, `report.html` | The run as a whole (`--report json,junit,html`) |

A retry's files end in `-retry<N>`. Compare a failing attempt with a passing one before anything else.

The exit code names the layer: 1 a test failed, 2 the suite or config is wrong (also "unsupported on this platform"), 3 the device or its connection, 4 the suite timeout, 5 Atalanta itself (a panic also exits 5). The mapping is `AtalantaError::exit_code` in `crates/atalanta-common/src/error.rs`.

## 2. Reproduce one test with everything on

```bash
cargo build -p atalanta-cli
target/debug/atalanta run path/to/suite.yaml --filter "<test name or glob>" \
  --trace always --video always --logs always --report json,html \
  --artifacts-dir /tmp/atalanta-debug
```

`--filter` takes a glob over test names (`--tags` selects by tag); a selection that matches nothing exits 2. Useful additions:

- `--pause-on-failure` stops at the failing step with the device in that state (Enter continues, `r` retries the step, `q` aborts). Interactive terminals only.
- `--profile-steps` prints where the time went per action; `--profile FILE` writes a Chrome/Perfetto trace of Atalanta's own spans.
- `atalanta validate <suite>` and `atalanta doctor` first, when the failure is at the start of the run.

## 3. Look at the screen the way Atalanta does

A selector that "should match" usually fails because the tree differs from what a person sees: the text is in `accessibilityId` not `text`, the element is off-screen or covered, or it sits in another window or frame.

```bash
target/debug/atalanta repl -d <device>     # then, at the prompt:
:tree                       # the element tree, as the selector engine sees it
:find { text: "Pay" }       # what a selector matches right now
:shot before.png
```

`atalanta snapshot [--find TEXT] [--bounds]` prints one line per element with a ref, and `atalanta inspect --open` is the same in the browser. Run a step in the REPL to see it act; `:save` writes what you typed as a suite.

## 4. Turn up the logs for one layer

Logs go to stderr, filtered by `RUST_LOG` (default `warn`). There is no `--verbose` flag. Scope the filter to the crate involved or the output is unreadable:

```bash
RUST_LOG=atalanta_core=debug  …            # executor, selectors, sync/settle, healing
RUST_LOG=atalanta_android=debug …          # adb, the tree server
RUST_LOG=atalanta_ios=debug …              # WDA / AtalantaRunner, simctl, devicectl
RUST_LOG=atalanta_web=debug,chromiumoxide=debug …
```

`ATALANTA_LOG_FORMAT=json` gives one JSON object per line with the `test`, `device` and `attempt` of the current span. `ATALANTA_EVENTS_FILE=<path>` writes step start and end events as JSONL. Outside the run: WDA and xcodebuild output is in `~/.atalanta/wda/logs/<device>.log`, the web daemon's in `~/.atalanta/daemon.log`. `atalanta settings [filter]` lists every named setting with its current value.

Switches that isolate a subsystem by turning it off, when you suspect it:

| Suspect | Try |
|---|---|
| the Android tree server | `ATALANTA_ANDROID_TREE_SERVER=0` falls back to uiautomator dumps |
| the iOS runner | `ATALANTA_IOS_RUNNER=atalanta` uses AtalantaRunner in place of WDA; `ATALANTA_IOS_FULL_SOURCE=1` reads the full page source |
| the Android video server | `ATALANTA_ANDROID_VIDEO=screenrecord` |
| actionability waits | `ATALANTA_ACTIONABLE_WAIT_MS` |
| strict selectors | `ATALANTA_SELECTORS_STRICT=1` |

If the step passes with a subsystem off, the bug is in that subsystem: that is a finding, not a fix.

## 5. Decide where the fault is

- **The suite.** The tree at the failing step (`trees/`) does not contain what the selector asks for, and the app is right. Fix the selector, or add the assertion the step was implicitly waiting for.
- **The app.** The screenshot and `app.log` show the app in the wrong state. Report it with the evidence; do not make the test tolerate it.
- **Atalanta.** The tree contains the element and the step still fails; or the action reports success and nothing happened; or the same suite behaves differently across platforms without a row in `docs/STATUS.md` saying so. Now read the code: the step's arm in `execute_action` (`crates/atalanta-core/src/executor/actions.rs`), then the driver method it calls.

For an Atalanta bug, write the failing test first. Driver logic is tested against fakes: scripted `adb`/`xcrun` on a `PATH` of shell scripts for Android and iOS, `FakeWda` in `crates/atalanta-ios/tests/common`, headless Chrome for web, `ScriptDriver` in `crates/atalanta-core/src/executor/tests.rs` for the executor. Reproduce the device's reply in the fake, then fix.

## Report

The failing step and `file:line`, the error, the evidence that locates it (the tree excerpt, the log line, the frame of video), which of the three it is, and what you ran to confirm. If you could not reproduce it, say so and give what you tried.
