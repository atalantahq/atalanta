# Architecture

Atalanta is a Rust workspace that builds one binary, `atalanta`. Tests are YAML suites. The engine runs them through a platform driver (Android, iOS, the web, or a headless renderer) and the runner orchestrates the devices and writes the reports.

Related docs:

- [DSL.md](DSL.md) and [DSL-REFERENCE.md](DSL-REFERENCE.md): the suite format.
- [DRIVERS.md](DRIVERS.md): platforms.
- [CLI.md](CLI.md): commands and exit codes.
- [CI.md](CI.md): CI usage.
- [MCP.md](MCP.md): the MCP server.
- [STATUS.md](STATUS.md): what has been verified where.

## Crates

```text
atalanta-cli               the `atalanta` binary: argument parsing, commands, exit codes, signals
├── atalanta-runner        suite orchestration, scheduling, sharding, parallel devices, retries,
│                          quarantine, result cache, history, leases, reporters
├── atalanta-core          parser, validator, schema, executor, selector engine, sync, dialogs,
│                          healing, visual comparison, baselines, traces, impact selection
├── atalanta-nl            natural-language steps and goal turns: 19 provider presets over three
│                          wire formats, grounding, cache, `verify:` checker, nl-eval
├── atalanta-explore       `atalanta explore`: the agent loop, the YAML writer, replay and repair
├── atalanta-doctor        environment checks behind `atalanta doctor` and the MCP `diagnose`
├── atalanta-android       Android driver (adb + embedded on-device tree server and video server)
├── atalanta-ios           iOS driver (simctl/devicectl + WebDriverAgent or AtalantaRunner)
├── atalanta-web           Chrome driver over the DevTools protocol
├── atalanta-headless      CPU layout renderer for Android XML / iOS xib (no emulator)
├── atalanta-flutter-bridge  headless Flutter via a `flutter test` process
├── atalanta-inspector     axum web UI and API: the console (live mirror, input, run and explore
│                          panels, tasks, diagnostics), selectors, recording, screenshot review
├── atalanta-mcp           MCP server (JSON-RPC over stdio), background tasks, `mcp install`
├── atalanta-migrate       Maestro → Atalanta converter
├── atalanta-api           `http` steps: requests, JSONPath checks, captures, the no-device driver
└── atalanta-common        shared types (Selector, Action, Element, Step), PlatformDriver trait,
                           errors and exit codes, settings table, resilience, redaction,
                           live events (`live_events`)
```

There are also native components that cargo does not build (see [DRIVERS.md](DRIVERS.md)):

- `probes/android/atalanta-tree-server`: Java, embedded as a jar.
- `probes/ios/AtalantaRunner`: Swift XCUITest.
- `probes/*/atalanta-sync-probe`: in-app idle probes.

## A run, end to end

1. **Parse.** `atalanta-core::parser` reads the YAML into a `TestSuite`.
   - Unknown step keys are errors, with a "did you mean".
   - Unknown `config:` keys are warnings (`config_keys.rs`, derived from the serde structs).
   - `source_map.rs` remembers `file:line` for each step.
   - `${VAR}`, `${VAR:-default}` and `${VAR:?message}` are expanded from `--env`, then from the environment (`env.rs`). `$${` is a literal `${`. Values of variables with secret-looking names are masked in all output.
2. **Validate.** `validator.rs` checks:
   - selectors;
   - platform capabilities (for example `mockRequest` on the web only, keys a platform can't press);
   - framework and driver combinations.

   Errors exit 2.
3. **Resolve.** `resolver.rs` inlines `runFlow`. It rejects cycles and limits nesting to 32 levels.
4. **Select and schedule.** `atalanta-runner::scheduler` does the following:
   - filters by tags, name and `--changed-since`;
   - adds `dependsOn` prerequisites and orders them;
   - shards: by count with stable tie-breaks, or by duration with `--shard-timings`;
   - orders the tests longest-first or failed-first.
5. **Run.**
   - **Sequential:** `SuiteRunner` takes one device.
   - **Parallel:** `ParallelRunner` has a supervisor that owns the queue, retries, quarantine and results, plus one actor per leased device with a warm executor.
     - A retry goes to another device when it can.
     - An infrastructure failure gets one extra attempt elsewhere.
     - A device with two infrastructure failures in a row is quarantined and re-probed.
   - **Leases:** `lease.rs` takes a `flock` per device under `~/.atalanta/leases`, so two runs never share a device.
6. **Execute a test.** `atalanta-core::executor` runs each step: see below.
7. **Report.**
   - JUnit, JSON, HTML, GitHub and Allure reporters (`atalanta-runner/src/reporter`).
   - `atalanta-history.json` (durations and outcomes) and traces for failed tests.
   - The exit code comes from the results: see [CLI.md](CLI.md#exit-codes).

**Live events.** With `ATALANTA_EVENTS_FILE` set, the executor appends one JSON line per test and step start and end to that file (`atalanta_common::live_events`: `testStarted` with the step count, `stepStarted` with index and total, `stepFinished` with status, duration and error, `testFinished`), and `atalanta explore` adds its own turn events. A supervisor tails it: the MCP server's `run_task` (task progress) and the console's Run and Explore panels. It is the only progress channel; the executor's hook is four one-line calls.

## Executing a step

For each step, `TestExecutor` does the following.

1. **Before the action.**
   - Dismisses system dialogs, unless the step targets something inside one.
   - Waits for the app to settle. It skips this for `wait`, `screenshot`, `launchApp`/`stopApp`, navigation, `setConfig`, mocks and `settle: none`.
2. **Find the element** (`find_element_smart`).
   - Polls the tree with back-off (0, 20, 50, 100, 100, 250, then 500 ms, each at most a fifth of the timeout) until the step timeout (`timeoutMs`, else `sync.timeoutMs`, default 10 s).
   - At the deadline it tries one loose match: one part of a merged label, with a warning.
   - Then it tries self-healing once, if healing is allowed.
3. **Actionability** (`actionability: strict`, the default).
   - The target must be enabled, and in the same place in two reads.
   - It waits up to 2 s for this (`ATALANTA_ACTIONABLE_WAIT_MS`).
   - `force: true` skips the check. `actionability: lenient` acts anyway, with a warning.
4. **Act**, through the driver.
5. **After the action.**
   - Invalidates the tree cache.
   - Settles the screen.
   - Checks for an app crash, and dialogs again.
   - A settle that reaches its cap warns. It never fails the step.
6. **Sticky crash.** Once the app is known to have crashed (a crash report, or the iOS device crash watcher that polls the app's state every second on a real iPhone), every later step that needs the app fails at once with that crash instead of waiting out its timeout. `launchApp` (of the suite's app) clears it; `launchApp`, `stopApp`, `restoreState`, `setConfig`, `mockRequest` and `clearMocks` still run. *The device crash watcher is unverified on hardware.*

Other per-step rules:

- **Watchdog:** a step fails with `StepTimeout` after 3× its timeout, and never sooner than 30 s.
- **Ambiguous selectors:** when a selector matches several elements, the first match in tree order is used, with a warning that suggests `childOf:`/`index:`. With `selectors.strict` it fails instead.
- **`optional: true`:** the step's failure becomes a warning. Infrastructure failures and crashes are still failures.
- **On failure:** a screenshot is saved, and a trace (`traces/<test>/`) holding each step's tree, settle reads, start time and the elements closest to the failed selector, with a self-contained viewer `trace.html` next to it (`trace_html.rs`).
- **Failure evidence:** at each test's start the executor asks the driver for an `EvidenceSession` (`atalanta-common/src/evidence.rs`): video and device logs (`artifacts.video`/`logs`: `off`, `on-failure`, `always`). At the end it is finished with what to keep; the files go next to the trace, crashes the logs saw are added to the failure message, and the files are linked from every report (`TestResult.attachments`). `--pause-on-failure` stops at the first failing step on an interactive terminal, writes what there is so far and waits (Enter, `r` retry, `q` abort).
- **Step screenshots:** with `stepScreenshots: true`, the screen after every step is kept, but written only when the test fails.

Retries live in the runner (`retry.rs`, at most 10). A test that passes on a retry is reported as flaky. `--fail-on-flaky` or `retry.flakyResult: fail` makes that exit 1.

## Sync

The goal is to act only when the app is ready, without fixed sleeps. The code is in `sync.rs` (`AdaptiveSyncEngine`) and `native_sync.rs` (`SyncManager`).

**Polling** compares UI-tree hashes:

1. The action has *reacted* once the tree changes, or once `reactionMs` (300) passes with no change.
2. The screen is *settled* once it is quiet for `quietMs` (150).
3. `settleCapMs` (3000) is the cap. Reaching it warns and moves on.

A loading spinner that appeared after the action holds the settle.

`sync.mode`:

- `auto` (default): uses the in-app probe when one answers, otherwise polling. It stops trying the probe after 3 failed connects, until the next launch.
- `native`: the probe only, falling back to polling if the probe belongs to another app.
- `polling`: tree diffs only.
- `none`: no settle.

**Native probes** are optional libraries compiled into a debug build of the app. They answer `wait_for_idle` over loopback TCP:

- iOS: port 19400. It tracks the run loop, animations, URL requests, timers (`sync.maxTimerMs`) and `performSelector:afterDelay:` work (swizzled after checking the argument encodings; cancels pass through).
- Android: port 19401. It tracks the main looper, animations and delayed main-looper messages due within the horizon (1.5 s, or `sync.maxTimerMs`). Network calls are counted by `AtalantaOkHttpInterceptor` when the app adds it to its OkHttp client; otherwise the app reports its own.
- React Native (both probes): with `framework: react_native` the probe also waits for JS timers within the horizon, Fabric mounts, native animations, RN network calls and RN loading.

Both probes speak protocol 4: `config` takes `"framework"`, and the reply carries `framework` (null when the probe has no tracking for it) and `unknown` (trackers that couldn't read their state). The `hello` must name the app under test. `web.networkIgnore` patterns are sent to the probe. *All of this is host-checked with fakes only: unverified on devices and in a real React Native app* (see [STATUS.md](STATUS.md)).

**Per step:** `settle: none | auto | { reactionMs, quietMs, capMs }`.

**On the web**, the page itself reports DOM mutations and animations, and then Atalanta waits for network idle. The defaults are a 30 ms reaction and a 1 s cap, and `web.settle` overrides them.

## Selectors

Matching is in `atalanta-common/src/selector_match.rs`. The cache and healing wrapper is `atalanta-core/src/selector.rs`. Every key is listed in [DSL-REFERENCE.md](DSL-REFERENCE.md#selectors).

- **Text matching.** Text is normalised: zero-width characters are dropped and whitespace is collapsed.
  - `text:` is tried exactly, then case-folded.
  - `textContains` ignores case.
  - `textMatches`/`idMatches` are whole-value regexes, case-insensitive, linear-time.
- **Candidates.** Only visible elements that intersect the screen are considered. The innermost match wins. `index:` counts in reading order.
- **Relations.** `below`, `above`, `leftOf` and `rightOf` pick the nearest match. `childOf` and `containsChild` use containment.
- **Lint.** `atalanta lint` flags fragile selectors: volatile ids, positional XPath, long texts, indexes, fixed waits and foreign syntax.

## Dialogs

`dialog_handler.rs` dismisses **system** dialogs only:

- iOS: SpringBoard permission alerts.
- Android: the permission controller and system-package alert dialogs.

It presses the most permissive button (for example "Allow While Using App" or "While using the app"). The app's own dialogs are left alone.

Config: `dialog: { enabled, maxDismissals, customDismissLabels }`.

## Self-healing

`healing.rs`. When a selector finds nothing at its deadline, the healer scores the on-screen elements. The score is a weighted similarity, Similo-style, against snapshots of the element from earlier passing runs (`.atalanta/healer-snapshots.json`). A candidate is used only if all of these hold:

- it reaches `confidenceThreshold` (0.8);
- it beats the runner-up by 0.05;
- the rewritten selector picks exactly that element.

Assertions and scrolls never heal.

`healing.mode`:

- `auto` (the default locally): use the heal, pass, and warn.
- `report` (the default in CI): fail the step and show the proposed patch.
- `off`.

Heals are written to `.atalanta/healed-selectors.json`. `atalanta heal --accept` rewrites the suite and keeps its comments. `--reject` stores the pair in `.atalanta/heal-rejected.json`, and that pair is never proposed again.

## Natural-language steps

`do:`/`act:` and `assert:`/`expect:` steps, and bare string steps, go through `atalanta-nl`. The executor sees it only through the `NaturalStepResolver` trait in `atalanta-common/src/natural.rs`, so `atalanta-core` does not depend on `atalanta-nl` or its HTTP clients; the CLI and the runner attach a `NaturalResolver`.

**The model picks and code decides.**

- The screen is sent as a numbered element list.
- The planner returns only `{op, element}`.
- `grounding.rs` builds the selector (stable id, then label, then exact text, then index if needed) and checks that it picks that element.
- Values come from the step's `data:` or are quoted from the step text. Model output never becomes a selector or a value directly.

**Model.** Any of the provider presets in `provider.rs` ([AI-PROVIDERS.md](AI-PROVIDERS.md)), on three wire formats:

- Anthropic Messages (`anthropic.rs`, also Claude on Vertex);
- OpenAI Chat Completions (`openai.rs`: OpenAI, Azure, Gemini, OpenRouter, Groq, Ollama, …);
- Bedrock Converse (`bedrock.rs`, SigV4 in `sigv4.rs`).

`AiSettings` resolves the provider, the step model and the goal model from `ATALANTA_NL_*` and `config.ai`. The default step model is a small one (`claude-haiku-5-5`), and it is provisional. `ATALANTA_NL_EFFORT` sets Anthropic's effort.

**Cache.** Plans are cached in `.atalanta/nl-cache.json` (format v3), per test and per step occurrence.

- **Written only once verified.** The executor holds a plan whose action and `waitUntil` passed, and commits it when the next step passes or the test passes (`PendingPlan`). With `verify:` on, it commits only when the whole test, checks included, passes. A test that fails drops its unverified plans.
- **No test data in the key.** The key is the step as written (`${VAR}`s unresolved, quoted values that were typed replaced by slots), whether it has `data:`, the platform and the occurrence. Values are placeholders in the stored action and are filled in on replay. A selector that holds only part of a value is bound to a fingerprint of it, so another value re-plans. Secrets never reach the file.
- **Old cache files.** A v2 file is ignored with a warning and rewritten.
- **Self-healing.** A cached action that stops working is invalidated and re-planned once, and the new plan is cached only after it verifies.
- **Concurrency.** Saves merge with the file on disk under a lock (`fsutil::with_lock`).
- **CI modes.** `ATALANTA_NL_CACHE=read-only|frozen|prune`. With no API key, an existing cache is replayed read-only.

**Verification.** `verify: off|final|checkpoints|strict` (`checker.rs`) asks the model whether a claim holds on the screen: the test's `description` at the end, and with `checkpoints` also `assert:` steps and `checkpoint:` steps. The model returns `{holds, evidence_element_ids, reason}`. Code keeps only the cited elements that are on the screen list, and a "holds" that cites none counts as unknown, not as a pass. Results are in `TestResult.verification` and in the JSON, JUnit and HTML reports.

**Data.** `nl.dataPolicy: describe` shows the model only a description of each value. `nl.allowCrossOrigin` lets an NL `goto` leave the page's origin.

**Consistency.** `atalanta nl-eval` measures pass^k on saved traces (`.atalanta/nl-traces`). With `--model provider:model` (repeatable), it runs the built-in task set (`crates/atalanta-nl/fixtures/eval/tasks.json`) across models and prints pass^k, latency, tokens and cost. `--recommend` names a default.

## Exploration and diagnostics

- **`atalanta explore`** (`crates/atalanta-explore`) runs an agent loop on the same executor: each turn the goal model returns an operation and an element number (`atalanta-nl/src/goal.rs`), code grounds and runs it, and the session is written as deterministic YAML with assertions, replayed with no planner attached, and repaired once if a replay fails. See [EXPLORE.md](EXPLORE.md).
- **`atalanta doctor`** (`crates/atalanta-doctor`): `run_checks(&Options) -> Report` (host, Android, iOS, web, AI, project; every external command with a deadline) and `apply_safe_fixes`. The MCP `diagnose` tool and the console's diagnostics call it in process.

## Visual assertions

`assertScreenshot` (`executor/screenshot.rs`, `visual.rs`, `baselines.rs`):

- **Baselines** are kept at `<suite dir>/atalanta-baselines/<platform>[-headless]/<W>x<H>/<name>.png`, where W×H is the device's full capture.
- **Capturing.** A first frame that matches passes at once. Otherwise Atalanta waits for two identical frames in a row. A screen that never holds still fails and is never written as a baseline.
- **Comparing.**
  - `threshold` is the share of pixels that must match (default 0.98).
  - Any changed cluster of `clusterPx` (20) pixels fails the step.
  - `comparator: strict` uses a per-channel `tolerance` (16). `perceptual` uses pixelmatch's YIQ distance.
  - `mask:` and `crop:` take selectors.
  - On devices the status bar is masked, or frozen with `screenshots.freezeStatusBar`.
- **Missing baselines.** A missing baseline is written and the step fails, on every machine. `--update-baselines=missing` writes it and passes. `--update-baselines` also rewrites changed ones.
- **Failures** leave `diffs/<name>-expected|actual|diff.png` and a `-review.json` in the artifacts folder. Review them with `atalanta baselines review` or the inspector's `/review` page; accepting writes the actual frame over the baseline atomically.

## Inspector

`atalanta inspect` (`crates/atalanta-inspector`) serves:

- **A React UI**, built by CI and the release job into `frontend/dist` and embedded with rust-embed. `build.rs` never runs npm, so a source build without `dist/` serves the API and a "UI not built" page (D-42).
- **A JSON API** under `/api`: devices, screenshots, hierarchy, actions, selector generation, flow saving, and a WebSocket live view.
- **`/review`**: a server-rendered screenshot review page that needs no frontend build. `atalanta baselines review --serve` serves it without a device.
- **The console** (`src/console/`): a live mirror over `/api/mirror/<device>` (H.264 from the Android video server for WebCodecs; WDA MJPEG on iOS simulators; the CDP screencast on the web; changed screenshots otherwise), input on the mirror under the device's lease (`control.rs`), and a task queue (`tasks.rs`) running `atalanta run` and `atalanta explore --json` children with live events (`ATALANTA_EVENTS_FILE`), the page's instructions appended to explore's `--inject-file`, a finished exploration's YAML saved into the project on request, and artifacts served from the task's folder. Diagnostics are `atalanta doctor`'s checks in process. `routing.rs` puts the Android, iOS and web drivers behind one `PlatformDriver`, by device id. The CLI plugs in the real hooks (`atalanta-cli/src/commands/inspect.rs`: live sources, leases, `ExploreChild`, the doctor); the defaults need no device and answer 501 for explore and diagnostics.

**Security:**

- It binds to loopback by default.
- Every `/api` request and `/review` needs the per-session token from the printed URL. The token can come as a header, a query parameter, or a Strict cookie set when the URL is opened.
- `Host` and `Origin` are checked, which defeats DNS rebinding and cross-site requests. The guard is one middleware over every route, so it covers each WebSocket upgrade (live view, mirror, task events) too.
- Served run artifacts are confined to the run's folder and carry a CSP with `connect-src 'none'`.
- WDA's MJPEG stream (the iOS mirror) has no authentication; Atalanta's WDA build is patched so it listens where `USE_IP` says, loopback on simulators ([CLI.md](CLI.md#inspect)).

## Errors and resilience

- **Error kinds.** `AtalantaError` (`atalanta-common/src/error.rs`) classifies each error as transient (retried by `ResilientDriver`), permanent or ambiguous, and maps it to an exit code.
- **Circuit breaker.** Each device has one. It opens after repeated failures (`ATALANTA_BREAKER_THRESHOLD`), and an open breaker is an infrastructure failure (exit 3).
- **Command deadline.** Device commands time out after `ATALANTA_COMMAND_TIMEOUT_MS` (60 s).
- **Panics** are caught and exit 5.

## Settings

Named driver and runtime settings live in one table (`atalanta-common/src/settings.rs`). Each has a key, an environment variable, a default and its allowed values. They can be set:

- in the suite (`config: settings:`);
- per run (`--set`);
- per test (`setConfig`);
- from the environment.

`atalanta settings` prints the table, and [DSL-REFERENCE.md](DSL-REFERENCE.md#settings) is generated from it.
