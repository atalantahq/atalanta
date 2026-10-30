# Changelog

All notable changes to Velocity are recorded in this file.

The format follows [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/), and Velocity uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0, a minor release (0.x.0) may change behaviour; such changes are listed under "Behaviour changes that can fail existing suites".

## [Unreleased]

### Added

- **API testing: the `http` step and the `velocity-api` crate.** A step sends a request (`method`, `url`, `headers`, `query`, a `json`/`body`/`form` body) and checks the response with `expect:` (`status`, `headers`, RFC 9535 JSONPath values under `json`, `body`, `maxMs`). Every check runs, and a failure lists each one that failed with the start of the body. Matchers are a plain value, or a map of `eq`, `ne`, `gt`, `gte`, `lt`, `lte`, `contains` (substring, element or object subset), `matches`, `exists`, `type`, `length` and `oneOf`. `capture:` sets `${NAME}` for the test's later steps, UI steps included, and a captured value whose name looks secret is masked like an environment variable's. Each test has its own cookie jar. `retry: { count, intervalMs }` polls a backend that catches up. `config.http` sets `baseUrl`, `headers`, `timeoutMs` and `insecure` for the suite. A suite whose selected tests have only `http` steps (with `wait`/`setConfig`) runs without a device, may leave out `appId`, and works with `--parallel`. See [docs/DSL.md](docs/DSL.md#http-and-api-tests). The tools compared are in `docs/research/landscape-api-testing.md`.
- `velocity` now names the `http` step when a migrated Maestro flow uses `runScript`/`evalScript`.
- `velocity run --env-file PATH` (repeatable; also on `shard-plan`) reads `KEY=VALUE` files: `#` comments, `export `, quotes, no shell expansion. `--env` wins over a file, a later file over an earlier one, any file over the environment, and secret-looking names are masked as usual. Nothing is loaded unless named (Maestro's most-voted request after drag, #2817).
- **`velocity explore` shows the model the project's own tests most like the goal** as demonstrations (LearnAct: one demonstration took a model from 19% to 52%). At most two tests and 1,500 characters, typed text only as `${NAME}`, never in `--benchmark`, off with `--no-demos`, looked for under `--demos-from` (default: the folder `--out` writes to). Grounding is unchanged. The prompt version is now `goal-3`.
- `explore` tells the model when its last action changed nothing on the screen list, or when it is back on a screen it saw in the last 4 turns.
- **Flakiness per failure kind.** History (format 3, `failureKinds`) keeps the kind of each failed run: not-found, assertion, timeout, crash, infrastructure, http or other. Flaky tests are reported as `flakiness 36% over 6 runs (timeout 27%, not-found 9%)` in the console, in report warnings and in `history merge`. Version 2 files load, with their failures of unknown kind.
- `velocity migrate maestro`: `killApp` becomes `stopApp`, and the warnings for `runScript`/`evalScript` point to the `http` step. `eraseText` becomes one `pressKey: backspace` per character (50 by default, up to 100), and `clearState: { appId }` keeps its app. A `tapOn` point is converted only when it is in absolute pixels with no selector; a point with a selector is inside that element, and is now skipped with a warning (it used to become a screen tap).
- **Accessibility audit:** `config.accessibility: { audit: true }`. It checks each distinct screen a test reads once, with no extra device reads, for:
  - unnamed controls;
  - unlabeled image buttons;
  - touch targets that are too small (48 dp Android, 44 pt iOS, 24 CSS px web; WCAG 2.2 2.5.8);
  - duplicate names among sibling controls;
  - unlabeled or placeholder-only fields.

  Findings are test warnings in every report, at most 20 per test. `fail: true` fails a test on error findings; `density` sets Android px per dp (default: a 411 dp short side). [DSL.md](docs/DSL.md#accessibility-audit).
- **`velocity explore` checks what the model expects.** With each action, the goal model may say what should follow: a text appears, an element is gone, a switch is on or off. Code checks this against the next screen.
  - **Not met:** the model is told, and the turn counts toward the stuck limit.
  - **Met:** an `assertVisible`/`assertNotVisible`/`checked:` assertion is written after the step, built from the real element and never from the model's text.
  - The counts are in the summary and the JSON report.
  - The prompt version is now `goal-4`.

  Source: Q-Reflector, arXiv 2608.05587.
- **`--hunt` explains flakes.** For each flaky test it reports where passing and failing rounds diverge and ranks the likely causes: the screen not done changing, a timeout too tight, order dependence, stress only. `--suggest` adds the YAML change for the top cause and hunts again with it applied in memory; the suite is never edited. Source: DoeFL, ICST 2026; FlakyGuard, ASE 2025.
- **`velocity api scaffold openapi.yaml`** writes an API-only suite from an OpenAPI 3 document:
  - requests ordered so producers come first;
  - one operation's response linked to another's parameters through `capture:`;
  - bodies taken from the spec's examples;
  - checks mined from the response schemas (types, required fields, enums, ranges, lengths, formats).

  `--confirm` sends the safe operations (GET/HEAD only) to a running server and drops every check it violates, reporting each mismatch. Source: AutoRestTest, SATORI, RBCTest. [CLI.md](docs/CLI.md).
- `velocity init` writes `velocity.schema.json`, puts the `yaml-language-server` schema line at the top of the suite, and adds the mapping to `.vscode/settings.json`. It merges only valid JSON, never overwrites a setting, and skips JSONC with a note.
- `bench/head-to-head/`: a reproducible Velocity vs Maestro benchmark on one emulator, alternating runs and reporting medians and p90.
- `.github/workflows/install-smoke.yml`: after each release, installs it through install.sh, Homebrew, npm and cargo on Linux and macOS, then runs an API-only suite.

### Changed

- **Faster settles.**
  - A fresh read matching the screen that last settled counts as settled at any age. Before, that only held for 2 s, and any longer step paid the quiet window again.
  - `scrollUntilVisible` settles once instead of twice: with no swipe needed, 15 tree reads and 355 ms became 2 reads and 32 ms in the executor tests.
  - A configuration error from the driver fails a lookup or assertion at once instead of being retried until the step timeout (10.2 s on a headless suite without `launchApp`). `sync.mode: native` is unchanged.
- **The headless and Flutter drivers report settled at once.** Their rendering is synchronous, so they no longer wait out the 300 ms reaction window: 20 headless taps went from 6.82 s to 0.022 s per test.
- **Android `launchApp`** stops the app and reads the clock in one shell, takes the pid with the focus check, keeps the probe's port forward per device, and reuses a screen state seen ready in the last 2 s. That is 9 → 7 adb round trips on a first launch and 7 → 3 right after another (fake adb). On an API 34 emulator a launch took about 230 ms less, and `scrollUntilVisible` with its row already shown went from 326 ms to 8 ms.
- **iOS `launchApp`** stops the app through the runner session when there is one instead of spawning `xcrun simctl terminate`. Typing polls for focus every 40 ms instead of sleeping (VelocityRunner, or with the focus check on); plain WebDriverAgent keeps its 100 ms wait. Not yet run on a simulator.
- Selector matching prepares the selector once per lookup rather than once per element, and compares ASCII text without allocating: 2–3× faster per lookup on a 5,000-element tree (release build).
- A test that history flags as flaky gets a warning with its per-kind score even when it passed, so it shows in the HTML, JUnit, GitHub and Allure reports.
- **Internal:**
  - Workspace lints: `undocumented_unsafe_blocks`, `await_holding_lock`, `dbg_macro`, `todo`, `unimplemented`, `unsafe_op_in_unsafe_fn`.
  - 11 unused dependencies removed (`lru` from the workspace entirely); shared versions for `libc`, `tempfile`, `png`, `sha2` and `ring`.
  - The largest futures boxed (CLI dispatch 55 KB, executor steps 30 KB → under 5 KB).
  - Heavy file I/O moved off the async threads.
  - `velocity run`'s 846-line `execute_file` split into named steps.
  - The console's routing driver forwards every driver method.
  - CI no longer runs a duplicate `check` job, or the fat-LTO release build on pull requests.
- **CI:**
  - The toolchain is pinned (1.98.1), so a new Rust release can't break every open PR; a weekly canary runs newest stable and beta, and the advisories.
  - Tests run under cargo-nextest (one process per test, binaries in parallel, a JUnit report per OS, a retry reported as flaky), with doctests run separately.
  - A docs-only change skips the Rust jobs. Every job has a timeout and `--locked`. One `CI OK` gate job serves branch protection. actionlint checks the workflows.
  - cargo-deny advisories pass, with the two unfixable "unmaintained" notices (serde_yaml, ttf-parser) ignored with reasons. Dependabot opens grouped weekly updates for cargo, npm and the actions.

### Security

- **VelocityRunner refuses cross-site and unauthenticated requests (R-1).** Velocity makes a random 128-bit token for every runner launch (simulators included) and sends it on every request; the runner refuses to start without one. It also answers 403 to any request with an `Origin` header, a `Sec-Fetch-Site` other than `none`/`same-origin`, or a `Host` that isn't `127.0.0.1`, `localhost` or `[::1]` with its own port (DNS rebinding). The token is kept in `~/.velocity/wda/running/<udid>.token` (mode `0600`) so a later Velocity process can reattach. WebDriverAgent is third-party and still unauthenticated; it stays bound to loopback (see `docs/SECURITY.md`, R-1W).
- **A cabled iPhone's runner binds the phone's loopback (R-4)**, which the USB (usbmux) forward reaches, instead of every interface, Wi-Fi included.
- **The Android tree server drops non-protocol input (R-2), protocol 13.** The first line that isn't a command (the `POST / HTTP/1.1` of a browser's blind request to the forwarded port) closes the connection, so the body is never run. Every connection must also present a per-start token (`AUTH`) that the server leaves in a shell-only file on the device. Older servers (protocol 7 to 12) still work.

### Fixed

- The workspace builds on the newest Rust toolchains: `AtomicU32::fetch_update`, deprecated there, is replaced by a compare-exchange loop that also works on the 1.89 MSRV.
- quick-xml 0.41 (RUSTSEC-2026-0194, -0195) and zip 9 (RUSTSEC-2026-0367, -0368, -0369). Since quick-xml 0.38, a text with an entity in it comes as separate events; the xib and storyboard parsers now keep `&amp;` and the like (`Line one & two` read as `Line one  two`).
- **A secret leak in redaction.** A short secret (a PIN or OTP under 8 characters) written right next to a long secret or a token shape was left in clear: `LONGSECRETpin` became `[redacted]pin`. Found by new property tests.
- iOS `launchApp` waits at most 5 s for the runner to stop the app before falling back to `simctl`, instead of the client's 30 s timeout.
- A crash or ANR message with a multi-byte character at byte 300 (CJK, emoji) panicked Android's logcat reader, and the crash evidence was lost. Also fixed: the same kind of panic in AWS request signing (a `%` escape before a multi-byte character) and in shortening an app hash.
- The console's driver forwarded half of the driver interface; the rest silently fell back to defaults (cached trees, crash checks, app liveness, screenshots for comparison, page refresh).
- A poisoned lock made the resilience layer's health check report healthy.
- Every web run printed chromiumoxide's `WARN WS Invalid message … untagged enum Message` with current Chrome. `RUST_LOG` still shows it.
- A WebDriverAgent reply with no element id is a device command failure, retried once, not a configuration error.
- A run with `--device` prepared the driver before taking the device's lease. On a simulator another Velocity process was using, it stopped that process's WebDriverAgent and started a runner on its port. The lease is now taken first.
- An iOS runner is never launched into a taken port. A runner of the same simulator (either kind, or one whose token this process doesn't have) is stopped first; any other process holding the port is named in a device error. A runner that fails to come up 3 times within 2 minutes is no longer relaunched. The step fails with the new `RunnerFailing` device error (exit 3, not retried), naming the last failure. Before, a WebDriverAgent that couldn't bind its port aborted and was relaunched every few seconds.
- Tests no longer write `.velocity/healer-snapshots.json` into the source tree: `RuntimeConfig::state_dir` and `TestExecutor::with_state_dir` put the self-healing state elsewhere.
- `perf_budget` was flaky (`tap` read 6 or 7 trees). The cause was the healing snapshot file that earlier runs, or the other test running at the same time, left in the crate directory, not timing: the first lookup of a selector reads one extra tree to record its snapshot. Each test now keeps that state in a temp dir, budgets the steady state, and checks that the first run costs at most that one read.

### Behaviour changes that can fail existing suites

- **[may fail suites]** **Cabled iPhones bind loopback by default.** `VELOCITY_IOS_DEVICE_LOOPBACK` flips: it used to opt in with `=1`; now loopback is the default and `=0` opts out. That usbmuxd delivers to the phone's loopback hasn't been checked on a real iPhone yet (P5). If the runner never answers on a cabled iPhone, set `VELOCITY_IOS_DEVICE_LOOPBACK=0`.
- **[may fail suites]** **VelocityRunner needs a token.** A VelocityRunner you start yourself (for `VELOCITY_WDA_URL`) must get `VELOCITY_RUNNER_TOKEN`, with the same value in `ios.runnerToken` for Velocity. Scripts that call the runner directly must send `Authorization: Bearer <token>` and no `Origin` header. Without a token the runner exits at launch.
- **[may fail suites]** **Tree server protocol 13.** An older Velocity that finds a protocol 13 server running asks it to quit and starts its own, as for any other version. Third-party tools that talked to `@velocity_tree` directly must send `AUTH <token>` first.

## [0.2.0] - unreleased

0.2.0 is a large release. Suites written for 0.1.0 keep their format (`appId`, `config`, `flows`, `tests`), but several checks are now stricter. Read [Behaviour changes that can fail existing suites](#behaviour-changes-that-can-fail-existing-suites) and [Migrating from 0.1.0](#migrating-from-010) before you upgrade a CI pipeline.

### Highlights

- **`velocity explore "<goal>" --app-id <id>`**: an AI agent works through a goal on a device and writes an ordinary YAML test with assertions. Velocity then replays that test with **no model calls** to check it holds, and repairs it once if a replay fails. CI runs the test like any other suite, with no model. `--script plan.json` plays a fixed plan instead of a model. See `docs/EXPLORE.md`.
- **The console (`velocity inspect --open`)**: one browser page for every booted Android device, iOS simulator and the web browser. It shows a live mirror you can click, drag and type on (Android H.264 at about 30 fps, iOS simulator MJPEG, Chrome screencast) and an element overlay that gives selectors and YAML. It can also run suites with live steps, run explore sessions you can steer, and show the `velocity doctor` diagnostics.
- **MCP server upgrade**: the tool count goes from 14 to 31. New are compact `snapshot`s with stable `@e` refs and delta mode, background tasks (`run_task`, `task_status`, `task_stop`, `task_inject`) that run suites or explore sessions, `describe_failure`, `inspect_trace` and `diagnose`. `velocity mcp install` registers the server with Claude Code, Cursor, Codex, Windsurf, VS Code, Cline and Gemini CLI.
- **Failure evidence**: a failed test keeps `video.mp4`, device and app logs, and a self-contained `trace.html` in `velocity-results/traces/<test>/`. The trace shows every step's screenshot with the tap point, the matched element, the video seeked to the step and the logs around it. Android video comes from Velocity's own encoder; no ffmpeg is needed. `--pause-on-failure` stops at the failing step so you can look at the device.
- **AI steps from 19 provider presets**: natural-language steps (`do:`, `assert:`), `verify:` checks and `explore` can use Anthropic, OpenAI, Azure OpenAI, Gemini, Bedrock, Vertex, OpenRouter, Groq, Mistral, DeepSeek, xAI, Together, Fireworks, DashScope, Perplexity, Ollama, LM Studio, vLLM or any OpenAI-compatible endpoint. Model output never becomes a selector or a typed value directly, and verified plans are cached so stable screens replay without calling the model.
- **`velocity init` and `velocity doctor`**: `init` goes from a running app to a first passing test plus a GitHub Actions example. `doctor` checks adb, Xcode, devices, runner builds, Chrome, AI keys and your suites, and `--fix` applies the safe fixes.
- **New platforms and drivers**: web testing in Chrome over CDP, a headless driver, a Flutter bridge, an on-device Android tree server (a dump takes about 3 ms; `uiautomator dump` takes about 2 s), and VelocityRunner, an alternative to WebDriverAgent on iOS.
- **CI**: real multi-device `--parallel`, `--shard M/N` balanced by past durations, `--changed-since` impact selection, history shared between jobs, flaky detection, quarantine, and HTML, GitHub, Allure, JUnit and JSON reports. Exit codes are documented and stable.

### Added

#### Commands

- `velocity init`, `velocity doctor [--fix] [--deep] [--json]`, `velocity explore`, `velocity nl-eval` (compares models on saved traces or a built-in task set).
- `velocity run [PATH]` takes a suite file or a folder (`-c` still works).
- `velocity lint`: finds fragile selectors and fixed waits. `velocity schema`: prints the suite JSON Schema for editor completion, or the DSL reference. `velocity settings`: lists the named settings.
- `velocity shard-plan`, `velocity history merge`, `velocity trace view`, `velocity heal`, `velocity baselines review|accept|reject` (`review --serve` gives a side-by-side review page).
- `velocity devices up|down` (boot or clone the devices a parallel run needs), `velocity repl`, `velocity daemon start|stop|status` (keeps drivers warm for `run --daemon`), and `velocity signing` (iOS signing with an App Store Connect API key).
- `velocity mcp install|config|rules`, `velocity migrate waits` (removes redundant `wait:` steps).
- The global `--color auto|always|never` flag. `NO_COLOR` and `CLICOLOR` are honoured.

#### Suite DSL

- Actions: `assertScreenshot`, `saveState`/`restoreState`, `setConfig`, `assertSystemState`, `tapXY`, `inputRaw`, `openLink`. Web only: `goto`, `goBack`, `refresh`, `mockRequest`, `clearMocks`.
- Natural-language steps: `do:`, `act:`, `assert:`, `expect:`, or a bare string, with `data:` and `waitUntil:`. AI verification with `verify: off|final|checkpoints|strict` and per-step `checkpoint:`.
- Selectors: `role:` and `name:` (Playwright's role names, mapped per platform), `textMatches:` and `idMatches:` (regex), the state filters `checked:`, `focused:`, `selected:` and `enabled:`, the relations `below`, `above`, `leftOf`, `rightOf`, `childOf` and `containsChild`, and `css:`/`xpath:` on the web.
- Step modifiers: `timeoutMs`, `settle`, `optional`, `force` and `appInFront`. Test keys: `dependsOn`, `sideEffects` and `description`.
- `${VAR:-default}` and `${VAR:?message}`. `$${` writes a literal `${`.
- Config: `healing.mode` (`auto | report | off`), `actionability`, `retry` (including `flakyResult: fail`), `artifacts.video`/`logs`/`videoSeconds`, `dialog`, `screenshots` (including `freezeStatusBar`), `web`, `headless`, `flutter`, `ai`, `nl` and `settings`.
- Screenshot baselines are stored per platform and screen size (`velocity-baselines/<platform>/<W>x<H>/`), with `mask:`, `crop:`, `threshold`, `tolerance` and `comparator: strict | perceptual`.

#### Running and CI

- `--parallel N` across devices, with cross-process device leases, so runs, the console and MCP agents never drive the same device at once. On the web, N browser contexts.
- `--shard M/N` (1-based), `--shard-timings` (from a history file or JUnit XML), `--changed-since REF` with `velocity-impact.yaml`, `--fail-if-none`, `--last-failed`, `--failed-first`, and `--history-from`.
- `--retries` (a pass on retry is reported as flaky), `--fail-on-flaky`, `--detect-new-flakes`, `--shuffle`, `--hunt`, `--quarantine`, `--cache-results`, `--set KEY=VALUE`, `--provision`, `--app PATH` (install by hash), `--update-baselines[=missing]`, `--junit-classname`, and `--profile` (Chrome/Perfetto trace).
- Reports: `html`, `github` (annotations and step summary; added automatically in GitHub Actions) and `allure`, alongside `junit` and `json`. Each links the failure evidence.
- Self-healing of broken selectors, reported as `PASSED (healed n)`. `velocity heal` reviews, accepts or rejects heals.
- Automatic handling of system permission prompts (`config.dialog`).
- Opt-in GitHub workflows for device smoke tests, the Flutter bridge and probe builds.

#### Failure evidence

- `--trace`, `--video` and `--logs` take `off|on-failure|always`; the default is `on-failure`. Video comes from Android's H.264 ring buffer (falling back to `screenrecord`), `simctl io recordVideo` on iOS simulators, or the CDP screencast on the web.
- Logs: logcat (`device.log` and `app.log`, with crashes and ANRs named in the failure message), the simulator's log stream, or the browser console with failed requests.
- `--pause-on-failure`: Enter continues, `r` retries the step, `q` aborts.

#### Platforms

- Web driver: Chrome over CDP, with 207 device profiles (`web.device`), cross-origin iframes, network mocks and touch swipes.
- Headless driver for smoke runs without a device, and a Flutter bridge (semantics tree by default, Flutter 3.19 or newer).
- Android: an embedded tree server for dumps, input and screenshots, Compose `testTag` reads, split-APK installs, emulator management, and an in-app sync probe that also tracks React Native.
- iOS: VelocityRunner (`ios.runner: velocity`) as an alternative to WebDriverAgent; WebDriverAgent pinned to a checked tag; opt-in tap verification (`ios.verifyTap`); physical iPhones through `devicectl` and usbmux, with crash-log collection.
- React Native and Flutter apps run on the native device drivers. Set `framework:` for framework-aware sync.

#### MCP and console

- MCP: 31 tools with read-only and destructive annotations, `--read-only`, `--notify` (webhook, script, JSONL, desktop), `type_text` with `secret:` (an environment variable name), session export as YAML with assertions, and MCP protocol versions up to 2025-11-25 plus the stateless 2026-07-28 revision.
- Console: a mirror with input under the device's lease, an element overlay, flow recording, a Run panel, an Explore panel (steer the agent with *Inject*, then *Save to project*), task history, Diagnostics, and a `/review` page for screenshot baselines.

### Changed

- **Exit codes** follow one contract: 0 passed; 1 a test failed; 2 usage or configuration; 3 device or infrastructure (including a run where every failure was infrastructure); 4 suite timeout; 5 internal error (a caught panic, or an error of no known kind); 129, 130 and 143 for SIGHUP, SIGINT and SIGTERM. A folder run returns its most severe file's code. See `docs/CLI.md`.
- Ctrl-C and SIGTERM stop the run, write partial reports and clean up. A second signal exits at once. `--timeout` now interrupts the running test, writes reports and exits 4.
- Every step waits for its element and for the screen to settle, and retry loops use the whole step timeout. A failing step can therefore take up to the full timeout before it fails.
- Text matching is the same on every platform. Zero-width characters are dropped and whitespace and newlines are collapsed. `text:` tries an exact match first, then a case-insensitive one. `className:` accepts the short form (`Button`).
- When a selector matches several elements, the first in tree order is used and a warning suggests an `index:` or `childOf:` that picks one. `index:` counts in reading order: rows top to bottom, each row left to right.
- An unknown `config:` key is a warning that names the closest known key.
- Shards are balanced by test count unless every shard is given the same `--shard-timings` file, so tests may land in different shards than in 0.1.0.
- Failure video and logs are captured by default and kept for failed tests. On an Android emulator this added about 15% to step time; `--video off --logs off` (or `artifacts.video: off` and `artifacts.logs: off`) turns them off.
- `velocity inspect` listens on `127.0.0.1` and needs the per-session token printed in its URL. It shows every platform's devices unless `-p` is given.
- `velocity mcp` picks its platform from the device id. Without one, it uses iOS.
- MCP: an unknown tool is a JSON-RPC `-32602` error, not a tool result. `screenshot` returns an image content block, not base64 inside the text. `run_flow` and `run_test` interpolate `${VAR}`, and an unset variable fails the call.
- Output written to a pipe contains no ANSI colour codes.
- iOS: WebDriverAgent sessions use `snapshotMaxDepth: 60`, so more deeply nested elements are visible.
- The in-app sync probes wait for short delayed work (Android `postDelayed`, iOS `performSelector:afterDelay:`, React Native timers and animations). Some steps can take longer, up to the settle cap.
- TLS uses rustls, so nothing is linked against OpenSSL.

#### Behaviour changes that can fail existing suites

Most suites that turn red here were passing by mistake. Each item gives the fix, and the opt-out where there is one.

- **[may fail suites]** **`id:` no longer matches part of an id.** In 0.1.0, `id: "login"` matched any id that contained `login`, such as `com.app:id/login_button`. Now `id:` matches the whole id or its short form after `:id/`. Fix: write the full short id, or use `idMatches:` for a pattern.
- **[may fail suites]** **`assertNotVisible` is stricter.** In 0.1.0 it passed whenever the lookup failed for any reason. Now it waits for the element to go, and fails in three cases: the screen can't be read until the deadline, the app has crashed, or another app is in front. Fix: for a step that leaves the app on purpose, add `appInFront: false` to the `assertNotVisible` step. The check is skipped automatically after `openLink`/`goto`, `launchApp` of another app and `pressKey: home`.
- **[may fail suites]** **App crashes fail the test.** In 0.1.0, a crash was noticed only when a later step failed. Now a test whose app crashed fails with "after the last step: App … crashed", even when the crash came after its last step. There is no opt-out; the crash is a real failure.
- **[may fail suites]** **Actions wait for an actionable target.** A `tap`, `doubleTap`, `longPress`, `inputText` or `clearText` waits until its target is enabled and its position is stable, and fails with "not actionable" if that never happens. In 0.1.0 it acted at once. Fix: wait for the real ready state. Opt-out: `force: true` on the step, or `actionability: lenient` in `config` (wait, then act anyway with a warning).
- **[may fail suites]** **`inputText` checks where the focus went.** When the tap before typing leaves the focus on a different element, the step fails rather than typing into the wrong field. Opt-out: `VELOCITY_ANDROID_FOCUS_CHECK=off` or `VELOCITY_IOS_FOCUS_CHECK=off`.
- **[may fail suites]** **iOS taps on covered elements fail.** In 0.1.0, iOS tapped the centre of the element's frame, even under the keyboard, a navigation or tab bar, an alert, the status bar, or off screen. Now a fully covered target fails with "covered by …", and a partly covered one is tapped on its visible part. Fix: dismiss the keyboard or scroll first. Opt-out: `VELOCITY_IOS_TAP_GUARD=off` turns off every guard; `VELOCITY_IOS_TAP_GUARD_STATUS_BAR=off` turns off only the status-bar rule.
- **[may fail suites]** **`assertVisible` is no longer satisfied by part of a merged label.** An element found only as one part of a merged accessibility label (for example "Home" inside "Home, tab, 1 of 4") now fails, and the message names the full label. Fix: assert the full label, or use `textContains:` or `textMatches:`.
- **[may fail suites]** **Empty selector values match nothing.** `text: ""`, `textContains: ""` and `accessibilityId: ""` used to match elements with empty text or labels (and, for `textContains`, every element). Fix: remove the step, or use a real value.
- **[may fail suites]** **`textContains:` ignores case.** It can now match more elements, so a bare selector may pick a different element (with a warning). Fix: narrow the selector with `text:`, `index:` or `childOf:`.
- **[may fail suites]** **`index:` counts in reading order.** In 0.1.0 it counted in tree order, so `index: N` may now pick a different element in grids and centred rows. Fix: check each `index:` with `velocity lint` or the console's element overlay.
- **[may fail suites]** **Stricter suite parsing.** Unknown keys inside an action (a misspelt `txet:`, or `timeoutMs` nested in the wrong map) are errors; in 0.1.0 they were silently ignored. `runFlow` with `when:` or another unknown key is an error; in 0.1.0 the condition was ignored and the flow always ran. Two tests with the same name are also an error. Fix: run `velocity validate` on every suite, which exits 2 and names each problem.
- **[may fail suites]** **Environment values are interpolated once.** A value that itself contains `${…}` (`BASE='${HOST}/api'`) is no longer expanded a second time. Fix: pass the final value. Use `$${` when you need a literal `${`.
- **[may fail suites]** **`--env KEY` without a value exits 2.** In 0.1.0 the flag was silently dropped. A comma not followed by `KEY=` stays inside the value, and `A=1,B=2` in one flag still sets both. Fix: write `--env KEY=VALUE`.
- **[may fail suites]** **Exit codes changed for CI scripts.** Configuration and usage errors exit 2, device problems (busy, unknown, lost) exit 3, unclassified errors and caught panics exit 5, and signals exit 129, 130 or 143. In 0.1.0 most of these exited 1. Fix: update scripts that treat any non-zero code as "tests failed", or that expect 1.
- **[may fail suites]** **An empty selection exits 2.** `--tags` or `--filter` that match no test now exit 2 and list the suite's tags; in 0.1.0 the run passed with zero tests. An empty shard, or an empty `--changed-since` selection without `--fail-if-none`, still exits 0. Fix: correct the tag or filter.
- **[may fail suites]** **Sharding flags are checked.** `--shard-index` and `--shard-total` must be given together, the total must be at least 1, and an out-of-range index exits 2; in 0.1.0 it ran zero tests and exited 0. Fix: pass both flags, or switch to `--shard M/N` (1-based).
- **[may fail suites]** **An unknown `--device` exits 3 at once.** 0.1.0 spent about 30 s per test in retries before failing. Fix: pass an id from `velocity device list`.
- **[may fail suites]** **Android typing is checked.** Text goes in through the tree server, so any Unicode works. A field that reads back different text, other than formatting such as a mask, fails the step instead of being typed again; this includes text cut by `maxLength`. When the tree server can't run, the `adb shell input text` fallback refuses non-ASCII text, password fields and secrets. Fix: assert the formatted value, keep text within the field's limit, and check `velocity doctor --deep` if the tree server doesn't start.
- **[may fail suites]** **Android app ids are validated.** An `appId` that isn't at least two dot-separated segments of letters, digits and `_` is a configuration error (exit 2). Fix: use the real package name.
- **[may fail suites]** **iOS `assertText` on an empty field reads `""`.** An empty text field, secure field, search field or text view no longer falls back to its placeholder or accessibility label. Fix: assert the placeholder with `assertVisible`, or assert `""`.
- **[may fail suites]** **iOS simulator `clearState` fails when the app's data can't be wiped.** In 0.1.0 it warned and ran on the old state. Fix: check that the app is installed and the simulator is booted; `velocity doctor` reports both.
- **[may fail suites]** **WebDriverAgent and VelocityRunner listen on loopback only on simulators.** A tool on another machine that reached a simulator's WDA over the LAN can no longer connect. Fix: run that tool on the Mac.
- **[may fail suites]** **The inspector is local and token-protected.** In 0.1.0 it listened on every interface with permissive CORS. Now it binds `127.0.0.1`, every API call needs the token from the printed URL, and requests from other origins are refused. Scripts that called the API without the token get 401. Fix: use the printed URL, or pass `--host 0.0.0.0` for LAN access, which still needs the token.
- **[may fail suites]** **`velocity migrate maestro` writes only suites that load.** `inputRandom*`, and an `inputText` with no literal text, become required variables, so a migrated run stops until you set them with `--env`. Fix: set the variables the migration report lists.
- **[may fail suites]** **A Wi-Fi-only iPhone needs a cable.** Running on a physical iPhone over Wi-Fi fails with "connect a cable" unless `ios.allowWifiRunner: true` (VelocityRunner with a runner token) is set.

### Deprecated

- **Ambiguous action selectors.** A tap or other action whose selector matches several elements warns that this will be an error in 1.0. The named setting `selectors.strict` chooses now: `1` (or `VELOCITY_SELECTORS_STRICT=1`, or `--set selectors.strict=1`) makes it an error today, and `0` keeps the current behaviour and silences the notice.
- **JUnit `classname`.** Every test currently has the classname `velocity`. At 1.0 the default becomes the suite file's name; `--junit-classname suite` switches now. The change resets test history in CI tools once.
- **Flutter `hierarchy: widgets`** (the widget tree in headless Flutter runs) still works with a warning. The semantics tree is the default.

### Removed

- The experimental React Native headless sidecar, which was on `main` after 0.1.0 but never released, is gone. `--framework react_native --driver headless` is a configuration error (exit 2). React Native apps run on the device drivers, where the in-app sync probe has React Native support.
- `build.rs` no longer runs npm. Release binaries embed the console UI. A source build without `crates/velocity-inspector/frontend/dist` serves the API and a page explaining how to build the UI.

### Fixed

- A test whose app crashed after its last step passed.
- `assertNotVisible` passed when the screen couldn't be read, or when the element was briefly visible but on its way out.
- `runFlow: { flowId, when: … }` was accepted, and the flow always ran.
- `--tags`, `--filter` or an out-of-range shard that matched nothing ran zero tests and exited 0.
- Configuration and device errors exited 1 ("tests failed"), and errors of unknown kind also exited 1.
- `--timeout` didn't stop a running test, and a timed-out run wrote no reports or history.
- A panic inside a test could hang a parallel run. It now fails that test and the run continues.
- A failing report writer stopped the other reports and cleanup. A corrupt history file is now moved aside, not overwritten.
- `scrollUntilVisible` didn't check after its last scroll, and failed for elements near the top or bottom edge of a screen that can't scroll.
- A cached element's old position was tapped after the layout moved.
- Android: a failed `uiautomator dump` could reuse an earlier dump file, so assertions ran against an old screen.
- Android: the `adb shell input text` fallback ran `$`, backticks and newlines as device shell code, and the app id was passed to the shell unquoted.
- Android: text in masked or reformatting fields was typed twice.
- Android: an unauthorized device is now a configuration error with a hint about the RSA prompt.
- iOS: taps landed on keyboard keys, the status bar or points off screen and reported success.
- iOS: `pressKey: back` clicked the first button of any navigation bar (Delete, Edit, Save, …).
- iOS: a WebDriverAgent that died on a real iPhone was never detected or restarted. A lost session is now recreated without relaunching the app.
- iOS: WebDriverAgent bootstrap failures were configuration errors (exit 2) instead of device errors (exit 3).
- iOS: a new WebDriverAgent session waited up to 10 s for the app to go idle before the first step.
- iOS simulators: an app without the in-app sync probe waited out the probe's grace period on every `launchApp`; it now waits once per run.
- MCP: `run_test` and `run_flow` typed `${VAR}` literally. Malformed UTF-8 or very long lines killed the server, and JSON-RPC error codes were wrong.
- MCP: after a tap, tools could read a cached screen from before the tap.
- Migration from Maestro: two sub-flows with the same file name collapsed into one, `stopApp`'s app id and other Maestro semantics were dropped, and output that didn't parse was reported as success.
- A UTF-8 byte order mark at the start of a suite file failed parsing, and very large timeouts panicked.
- Secrets containing `"` or `\` were never masked.

### Security

- The inspector no longer accepts requests from any website or LAN host. It binds `127.0.0.1` and checks a per-session token, `Host` and `Origin` on every request and WebSocket. It sends no CORS headers, and it serves artifacts only from the run's folder, under a Content-Security-Policy.
- On simulators, WebDriverAgent and VelocityRunner listen on loopback. Velocity patches its WebDriverAgent build so the MJPEG screen stream, which upstream serves on every interface without authentication, also listens on loopback. A WebDriverAgent you start yourself (`VELOCITY_WDA_URL`), or one kept running from an older build (`VELOCITY_KEEP_WDA`), still exposes the stream until it is restarted. On real iPhones, WebDriverAgent listens on the phone's interfaces unless `VELOCITY_IOS_DEVICE_LOOPBACK=1`.
- Secrets come from environment variables. They are masked in every report, log, trace and GitHub Actions log (`::add-mask::`, multi-line secrets line by line), never sent to a model, and written into generated YAML only as `${NAME}`. Over MCP, an agent can type only the variables listed in `VELOCITY_SECRETS`.
- Values are interpolated in one pass, so a variable's value can't inject another variable.
- Device shell commands no longer interpolate unquoted app ids or typed text.
- History, cache, lease and profile files are created `0600` (directories `0700`), and atomic writes replace a symlink instead of following it.
- AI provider keys are read only from environment variables, never logged, and refused when a key value is put where a variable name belongs (`apiKeyEnv`).

### Platform support

- **Prebuilt binaries:** macOS arm64 and x86_64; Linux x86_64 and arm64. The Linux binaries are statically linked against musl. Release binaries include the console UI.
- **Windows is not supported.**
- **Install:**
  - `curl -fsSL https://raw.githubusercontent.com/omkar-dev/velocity/main/install.sh | sh` (verifies the SHA-256; `VELOCITY_VERSION` pins a version, `--dir` picks where, never needs sudo)
  - Homebrew: `brew install omkar-dev/velocity/velocity` (the prebuilt binary; `--HEAD` builds `main` from source, UI included)
  - npm: `npx @velocity-test/cli --version`, or `npm install -D @velocity-test/cli`
  - `cargo install velocity-cli --locked` (builds from source; the published crate carries the built console UI, so no npm is needed)
- **Release assets:** `velocity-<version>-<target>.tar.gz` for `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`, a `SHA256SUMS` file, and GitHub build provenance attestations (`gh attestation verify <file> --repo omkar-dev/velocity`).
- **Rust crates:** the MCP server crate is published as `velocity-mcp-server` (the name `velocity-mcp` was taken); its library is still `velocity_mcp`. The minimum supported Rust version is 1.89.
- **Devices:**
  - Android emulators and devices need `adb`; Java is not needed.
  - iOS simulators and devices need macOS with Xcode.
  - The web driver needs Chrome or Chromium.
- **Verification:** Android 14 emulators, iOS 26 simulators and headless Chrome on macOS were exercised for this release. Real iPhones and Android hardware have not been tested yet. `docs/STATUS.md` lists what has been verified, and on what.

### Migrating from 0.1.0

1. Install 0.2.0 and run `velocity doctor -p android,ios` (or the platforms you use). Apply what it suggests, or run `velocity doctor --fix`.
2. Run `velocity validate <suite-or-folder>`. Fix every reported key: misspelt keys inside actions, `runFlow` with `when:`, duplicate test names.
3. Run `velocity lint <suite-or-folder>` and review its output. Then search your suites for `id:` values that relied on substring matching, and replace each with the full short id or an `idMatches:` pattern.
4. Run each suite once locally and fix what turns red, using the list above. Usual fixes:
   - `appInFront: false` on an `assertNotVisible` that runs while another app is in front;
   - `force: true` on an action whose target never becomes enabled or still (or `actionability: lenient` while you investigate);
   - a narrower selector where `textContains:` or `index:` now picks a different element.
5. Update CI scripts:
   - handle exit codes 2, 3, 5, 129, 130 and 143;
   - expect exit 2 for a tag or filter that matches nothing;
   - pass `--shard-index` and `--shard-total` together, or move to `--shard M/N`;
   - pass `--env` as `KEY=VALUE`.
6. Upload `velocity-results/` as a CI artifact so failure videos, logs and `trace.html` are kept, or turn capture off with `--video off --logs off`. Add `velocity-results/` and `.velocity/healed-selectors.json` to `.gitignore`.
7. Decide on self-healing in CI. Its default there is `report`: a test that passes only through a heal fails and prints the `velocity heal --accept` command. Set `healing.mode: auto` or `off` to choose otherwise.
8. For the inspector, open the tokened URL it prints. Add `--host 0.0.0.0` only if you need LAN access.
9. For MCP clients, run `velocity mcp install` to register the server and the agent rules. List the secrets agents may type in `VELOCITY_SECRETS`.

## [0.1.0] - 2026-03-16

First public release.

- A YAML test DSL (`appId`, `config`, `flows`, `tests`) with `launchApp`, `stopApp`, `tap`, `doubleTap`, `longPress`, `inputText`, `clearText`, `assertVisible`, `assertNotVisible`, `assertText`, `scrollUntilVisible`, `swipe`, `screenshot`, `pressKey`, `wait` and `runFlow`. Selectors: `id`, `text`, `textContains`, `accessibilityId`, `className` and `index`, and combinations of them. `${VAR}` environment interpolation.
- An Android driver (adb and UIAutomator) and an iOS simulator driver (simctl and WebDriverAgent), behind retry policies and a circuit breaker.
- `velocity run` with `--tags`, `--filter`, `--shard-index`/`--shard-total`, `--retries`, `--timeout`, `--fail-fast`, `--env`, and JUnit and JSON reports with test history.
- `velocity validate`, `velocity device list|boot|shutdown|screenshot`, and `velocity migrate maestro`.
- `velocity mcp`: an MCP server on stdio with 14 tools for driving a device and running flows and tests.
- `velocity inspect`: an inspector web UI for the element tree and selectors.
- An install script, a Homebrew formula, and CI and release workflows.

[Unreleased]: https://github.com/omkar-dev/velocity/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/omkar-dev/velocity/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/omkar-dev/velocity/releases/tag/v0.1.0
