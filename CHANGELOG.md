# Changelog

All notable changes to Velocity are recorded in this file.

The format follows [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/), and Velocity uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0, a minor release (0.x.0) may change behaviour; such changes are listed under "Behaviour changes that can fail existing suites".

## [Unreleased]

0.2.0 is a large release. Suites written for 0.1.0 keep their format (`appId`, `config`, `flows`, `tests`), but several checks are now stricter. Read [Behaviour changes that can fail existing suites](#behaviour-changes-that-can-fail-existing-suites) and [Migrating from 0.1.0](#migrating-from-010) before you upgrade a CI pipeline.

### Behaviour changes that can fail existing suites

- **[may fail suites]** **iOS refuses taps behind sheets, popovers and in-app alerts** (and their dimming view) instead of dismissing the sheet. Not yet run on a simulator or device. [Details](docs/research/changelog-0.2.0-details.md#ios-sheets)
- **[may fail suites]** **iOS `pressKey: back` taps only a real Back button**, else swipes from the left edge; closing a sheet matches whole names only. Not yet run on a simulator or device. [Details](docs/research/changelog-0.2.0-details.md#ios-back)
- **[may fail suites]** **MCP screenshots are files.** `screenshot` (and `observe {what: screenshot}`) saves the PNG and returns its path and size; the image is in the reply only with `inline: true`, and then downscaled below 2,000 px on its longest edge.
- **[may fail suites]** **`velocity explore` trusts a test only after its gates** (5 replays, a wrong-path replay that must fail, a goal assertion); an untrusted test exits 1. `--no-gates` skips them ([EXPLORE.md](docs/EXPLORE.md#trust-gates)). [Details](docs/research/changelog-0.2.0-details.md#explore-gates)
- **[may fail suites]** **No silent passes.** A shard that owns no test (more shards than tests) exits 2, naming it, instead of passing empty. A planned test that ends with no result (a worker that stopped) is reported failed as an internal error (exit 5) instead of missing from the totals.
- **[may fail suites]** **`velocity mcp` serves five tools by default** (`observe`, `act`, `run`, `artifacts`, `verify_change`); a call to a full tool gets an error naming the core call. `--profile full` serves all 33 ([MCP.md](docs/MCP.md#core-tools)). [Details](docs/research/changelog-0.2.0-details.md#mcp-core)
- **[may fail suites]** **`explore --values-from-goal` no longer copies screen text**; add `--values-from-screen` for the old behaviour. [Details](docs/research/changelog-0.2.0-details.md#values-goal)
- **[may fail suites]** **The memory check fails the test** (was a warning) when peak PSS rises past `pssGrowthThresholdPct`, in both runners; `heapGrowthThresholdPct` still works with a warning. Android only. [Details](docs/research/changelog-0.2.0-details.md#memory-check)
- **[may fail suites]** **Cabled iPhones bind loopback by default**; `VELOCITY_IOS_DEVICE_LOOPBACK=0` opts out if the runner never answers. Unchecked on a real iPhone. [Details](docs/research/changelog-0.2.0-details.md#iphone-loopback)
- **[may fail suites]** **VelocityRunner needs a token**: one you start yourself needs `VELOCITY_RUNNER_TOKEN` (and `ios.runnerToken`); direct callers send `Authorization: Bearer` and no `Origin`. [Details](docs/research/changelog-0.2.0-details.md#runner-token)
- **[may fail suites]** **Tree-server protocol changes**: an older Velocity replaces a newer server with its own; third-party tools on `@velocity_tree` must do the protocol 18 `HELLO`/`PROOF`/`AUTH` handshake. [Details](docs/research/changelog-0.2.0-details.md#tree-protocol)

Most suites that turn red here were passing by mistake. Each item gives the fix, and the opt-out where there is one.

- **[may fail suites]** **`id:` no longer matches part of an id.** In 0.1.0, `id: "login"` matched any id that contained `login`, such as `com.app:id/login_button`. Now `id:` matches the whole id or its short form after `:id/`. Fix: write the full short id, or use `idMatches:` for a pattern.
- **[may fail suites]** **`assertNotVisible` is stricter**: it fails when the screen can't be read, the app crashed or another app is in front. Fix: `appInFront: false` on a step that leaves the app on purpose. [Details](docs/research/changelog-0.2.0-details.md#assert-not-visible)
- **[may fail suites]** **App crashes fail the test.** In 0.1.0, a crash was noticed only when a later step failed. Now a test whose app crashed fails with "after the last step: App … crashed", even when the crash came after its last step. There is no opt-out; the crash is a real failure.
- **[may fail suites]** **Actions wait for an actionable target** (enabled and still) and fail "not actionable" otherwise. Opt-out: `force: true`, or `actionability: lenient`. [Details](docs/research/changelog-0.2.0-details.md#actionable)
- **[may fail suites]** **`inputText` checks where the focus went.** When the tap before typing leaves the focus on a different element, the step fails rather than typing into the wrong field. Opt-out: `VELOCITY_ANDROID_FOCUS_CHECK=off` or `VELOCITY_IOS_FOCUS_CHECK=off`.
- **[may fail suites]** **iOS taps on covered elements fail** ("covered by …"); a partly covered one is tapped on its visible part. Opt-out: `VELOCITY_IOS_TAP_GUARD=off` or `VELOCITY_IOS_TAP_GUARD_STATUS_BAR=off`. [Details](docs/research/changelog-0.2.0-details.md#ios-covered)
- **[may fail suites]** **`assertVisible` no longer matches part of a merged label**; assert the full label, or use `textContains:`/`textMatches:`. [Details](docs/research/changelog-0.2.0-details.md#merged-label)
- **[may fail suites]** **Empty selector values match nothing.** `text: ""`, `textContains: ""` and `accessibilityId: ""` used to match elements with empty text or labels (and, for `textContains`, every element). Fix: remove the step, or use a real value.
- **[may fail suites]** **`textContains:` ignores case.** It can now match more elements, so a bare selector may pick a different element (with a warning). Fix: narrow the selector with `text:`, `index:` or `childOf:`.
- **[may fail suites]** **`index:` counts in reading order.** In 0.1.0 it counted in tree order, so `index: N` may now pick a different element in grids and centred rows. Fix: check each `index:` with `velocity lint` or the console's element overlay.
- **[may fail suites]** **Stricter suite parsing**: unknown keys, `runFlow` with `when:` and duplicate test names are errors. Fix: `velocity validate` names each one. [Details](docs/research/changelog-0.2.0-details.md#strict-parsing)
- **[may fail suites]** **Environment values are interpolated once.** A value that itself contains `${…}` (`BASE='${HOST}/api'`) is no longer expanded a second time. Fix: pass the final value. Use `$${` when you need a literal `${`.
- **[may fail suites]** **`--env KEY` without a value exits 2.** In 0.1.0 the flag was silently dropped. A comma not followed by `KEY=` stays inside the value, and `A=1,B=2` in one flag still sets both. Fix: write `--env KEY=VALUE`.
- **[may fail suites]** **Exit codes changed for CI scripts**: 2 config, 3 device, 5 internal, 129/130/143 signals (most were 1). [Details](docs/research/changelog-0.2.0-details.md#exit-codes-ci)
- **[may fail suites]** **An empty selection exits 2**: `--tags`/`--filter` matching no test now fail and list the suite's tags. [Details](docs/research/changelog-0.2.0-details.md#empty-selection)
- **[may fail suites]** **Sharding flags are checked.** `--shard-index` and `--shard-total` must be given together, the total must be at least 1, and an out-of-range index exits 2; in 0.1.0 it ran zero tests and exited 0. Fix: pass both flags, or switch to `--shard M/N` (1-based).
- **[may fail suites]** **An unknown `--device` exits 3 at once.** 0.1.0 spent about 30 s per test in retries before failing. Fix: pass an id from `velocity device list`.
- **[may fail suites]** **Android typing is checked**: text that reads back different (including a `maxLength` cut) fails the step; the `adb input text` fallback refuses non-ASCII, passwords and secrets. [Details](docs/research/changelog-0.2.0-details.md#android-typing)
- **[may fail suites]** **Android app ids are validated.** An `appId` that isn't at least two dot-separated segments of letters, digits and `_` is a configuration error (exit 2). Fix: use the real package name.
- **[may fail suites]** **iOS `assertText` on an empty field reads `""`.** An empty text field, secure field, search field or text view no longer falls back to its placeholder or accessibility label. Fix: assert the placeholder with `assertVisible`, or assert `""`.
- **[may fail suites]** **iOS simulator `clearState` fails when the app's data can't be wiped.** In 0.1.0 it warned and ran on the old state. Fix: check that the app is installed and the simulator is booted; `velocity doctor` reports both.
- **[may fail suites]** **WebDriverAgent and VelocityRunner listen on loopback only on simulators.** A tool on another machine that reached a simulator's WDA over the LAN can no longer connect. Fix: run that tool on the Mac.
- **[may fail suites]** **The inspector is local and token-protected**: it binds `127.0.0.1` and every API call needs the printed URL's token (401 otherwise). [Details](docs/research/changelog-0.2.0-details.md#inspector-token)
- **[may fail suites]** **`velocity migrate maestro` writes only suites that load.** `inputRandom*`, and an `inputText` with no literal text, become required variables, so a migrated run stops until you set them with `--env`. Fix: set the variables the migration report lists.
- **[may fail suites]** **A Wi-Fi-only iPhone needs a cable.** Running on a physical iPhone over Wi-Fi fails with "connect a cable" unless `ios.allowWifiRunner: true` (VelocityRunner with a runner token) is set.
- **[may fail suites]** **Android dialog auto-dismiss covers only system packages**; tap an app's own dialog buttons in the test. [Details](docs/research/changelog-0.2.0-details.md#android-dialogs)
- **[may fail suites]** **Web `css:`, `id:` and `className:` with `index:` count only rendered, on-screen matches, in reading order**, as `explore` counted them when it wrote the test. A hidden or off-screen match no longer takes an index. Fix: re-check each such `index:`.
- **[may fail suites]** **Web popups and new tabs are followed.** A click that opens a popup or tab drives it next; when it closes, the opener is driven again.
- **[may fail suites]** **`velocity devices down` stops only the caller's devices.** The owner is `--owner`, else `VELOCITY_OWNER`, else the CI job id; `--all` stops every device. CI scripts that relied on `down` stopping everything must pass `--all`.
- **[may fail suites]** **MCP selectors take only the flat DSL form** (`{text: …}`, `{id: …}`); tagged serde selectors are refused.
- **[may fail suites]** **Agent commands exit 3 for device failures.** `velocity tap` and the other agent commands exit 3 (was 1) when the reply's code is `DEVICE_*` or the new `SESSION_BUSY` (another command of the same session held it past `VELOCITY_LEASE_WAIT_MS`, 10 s, where it used to wait forever). Scripts that treated every failure as 1 should accept 3, or retry on it.
- **[may fail suites]** **`--cache-results` runs more tests.** Tests with an `exec`, `http` or natural-language step are never served from the cache, the key includes the screen size and, in CI, the branch (`VELOCITY_CACHE_SCOPE` overrides it), and `VELOCITY_CACHE_MAX_AGE` (`6h`, `2d`) shortens the 30-day reuse. A suite that relied on those cached passes now runs them.

- **[may fail suites]** **Web: Chrome refuses invalid TLS certificates.** A suite against a self-signed dev server now fails to load it. Fix: `config.settings: { web.insecureCerts: 1 }` (or `VELOCITY_WEB_INSECURE_CERTS=1`).
- **[may fail suites]** **`http` steps with their own headers don't follow a redirect to another origin**, and no step follows `https` → `http`: the step sees the 3xx. Fix: request the final URL, or check the `location` header.

### Highlights

- **`velocity explore "<goal>" --app-id <id>`**: an AI agent reaches a goal once and writes a YAML test that replays with no model calls ([EXPLORE.md](docs/EXPLORE.md)). [Details](docs/research/changelog-0.2.0-details.md#hl-explore)
- **The console (`velocity inspect --open`)**: a live, clickable mirror of every booted device and browser, with selectors, suite runs, explore sessions and diagnostics. [Details](docs/research/changelog-0.2.0-details.md#hl-console)
- **MCP server upgrade**: 14 → 32 tools (five by default), `@e` refs, background tasks, and `velocity mcp install` for seven clients. [Details](docs/research/changelog-0.2.0-details.md#hl-mcp)
- **Failure evidence**: a failed test keeps `video.mp4`, logs and a self-contained `trace.html`; `--pause-on-failure` stops at the failing step. [Details](docs/research/changelog-0.2.0-details.md#hl-evidence)
- **AI steps from 19 provider presets** for `do:`/`assert:`, `verify:` and `explore`; model output never becomes a selector or typed value ([AI-PROVIDERS.md](docs/AI-PROVIDERS.md)). [Details](docs/research/changelog-0.2.0-details.md#hl-providers)
- **`velocity init` and `velocity doctor`**: `init` goes from a running app to a first passing test plus a GitHub Actions example. `doctor` checks adb, Xcode, devices, runner builds, Chrome, AI keys and your suites, and `--fix` applies the safe fixes.
- **New platforms and drivers**: web testing in Chrome over CDP, a headless driver, a Flutter bridge, an on-device Android tree server (a dump takes about 3 ms; `uiautomator dump` takes about 2 s), and VelocityRunner, an alternative to WebDriverAgent on iOS.
- **CI**: real multi-device `--parallel`, `--shard M/N` balanced by past durations, `--changed-since` impact selection, history shared between jobs, flaky detection, quarantine, and HTML, GitHub, Allure, JUnit and JSON reports. Exit codes are documented and stable.

### Added

#### Commands

- `velocity init`, `velocity doctor [--fix] [--deep] [--json]`, `velocity explore`, `velocity nl-eval` (compares models on saved traces or a built-in task set).
- `velocity run [PATH]` takes a suite file or a folder (`-c` still works).
- `velocity lint`: finds fragile selectors and fixed waits. `velocity schema`: prints the suite JSON Schema for editor completion, or the DSL reference. `velocity settings`: lists the named settings.
- `velocity shard-plan`, `velocity history merge`, `velocity history flaky` (rolling flakiness over a test's last N runs, retries plus pass/fail flips as `velocity run` reports it, CI-gated with `--max-rate`), `velocity trace view`, `velocity heal`, `velocity baselines review|accept|reject` (`review --serve` gives a side-by-side review page).
- `velocity devices up|down` (boot or clone the devices a parallel run needs), `velocity repl`, `velocity daemon start|stop|status` (keeps drivers warm for `run --daemon`), and `velocity signing` (iOS signing with an App Store Connect API key).
- `velocity mcp install|config|rules`, `velocity migrate waits` (removes redundant `wait:` steps).
- `velocity gate <file> --goal "<text>"`: the trust gates for a one-test suite YAML that was never explored (hand-written, or exported from an agent session): burn-in replays, a wrong-path replay that must fail, and an assertion naming goal text. Exit 0 trusted, 1 not trusted ([CLI.md](docs/CLI.md#gate)). Over MCP, `run_task` with `gate: true` runs it as a background task (`summary.gates`), and `export_session_flow` says its test is not gated yet and gives the call that gates it.
- The global `--color auto|always|never` flag. `NO_COLOR` and `CLICOLOR` are honoured.

#### Suite DSL

- `launchApp` takes `arguments` (Maestro's form): Android intent extras and iOS launch arguments; `velocity migrate maestro` carries them over. ([DSL.md](docs/DSL.md#launch-arguments)).
- Actions: `assertScreenshot`, `saveState`/`restoreState`, `setConfig`, `assertSystemState`, `tapXY`, `inputRaw`, `openLink`. Web only: `goto`, `goBack`, `refresh`, `mockRequest`, `clearMocks`.
- Natural-language steps: `do:`, `act:`, `assert:`, `expect:`, or a bare string, with `data:` and `waitUntil:`. AI verification with `verify: off|final|checkpoints|strict` and per-step `checkpoint:`.
- Selectors: `role:`/`name:`, `textMatches:`/`idMatches:`, state filters, relations (`below`, `childOf`, …), and `css:`/`xpath:` on the web. [Details](docs/research/changelog-0.2.0-details.md#selectors)
- Step modifiers: `timeoutMs`, `settle`, `optional`, `force` and `appInFront`. Test keys: `dependsOn`, `sideEffects` and `description`.
- `runFlow: { flow: <id>, env: { <NAME>: <value> } }` passes parameters to one call of a flow (and the flows it runs), filled in when the suite loads; a parameter the call doesn't pass comes from the environment. Decimals must be quoted (`"1.50"`), and `flow` and `flowId` together are an error. MCP `run_flow` takes the same `env`. `velocity migrate maestro` carries Maestro's `runFlow` `env` over. ([DSL.md](docs/DSL.md#parameters))
- `${VAR:-default}` and `${VAR:?message}`. `$${` writes a literal `${`.
- Config: `healing.mode` (`auto | report | off`), `actionability`, `retry` (including `flakyResult: fail`), `artifacts.video`/`logs`/`videoSeconds`, `dialog`, `screenshots` (including `freezeStatusBar`), `web`, `headless`, `flutter`, `ai`, `nl` and `settings`.
- Screenshot baselines are stored per platform and screen size (`velocity-baselines/<platform>/<W>x<H>/`), with `mask:`, `crop:`, `threshold`, `tolerance` and `comparator: strict | perceptual`.

#### Running and CI

- `--parallel N` across devices, with cross-process device leases, so runs, the console and MCP agents never drive the same device at once. On the web, N browser contexts.
- `--shard M/N` (1-based), `--shard-timings` (from a history file or JUnit XML), `--changed-since REF` with `velocity-impact.yaml`, `--fail-if-none`, `--last-failed`, `--failed-first`, and `--history-from`.
- `velocity run` flags: `--retries`, `--fail-on-flaky`, `--detect-new-flakes`, `--shuffle`, `--hunt`, `--quarantine`, `--cache-results`, `--set`, `--provision`, `--app`, `--update-baselines`, `--junit-classname`, `--profile`. [Details](docs/research/changelog-0.2.0-details.md#run-flags)
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

- MCP `observe {what: a11y}` (full profile: `audit_accessibility`) audits the current screen with the accessibility rules and names each finding's element by its snapshot ref. Read-only.
- MCP: 32 tools with read-only/destructive annotations, `--read-only`, `--notify`, `type_text` with `secret:`, session export, protocol versions up to 2025-11-25 and 2026-07-28. [Details](docs/research/changelog-0.2.0-details.md#mcp-added)
- Console: a mirror with input under the device's lease, an element overlay, flow recording, a Run panel, an Explore panel (steer the agent with *Inject*, then *Save to project*), task history, Diagnostics, and a `/review` page for screenshot baselines.

#### Agents, API tests, CI and more

- **Agent commands: drive a device from the shell, by ref** (`velocity snapshot`, `tap @e7`, `export`, …), with settle, deltas and `--json` ([CLI.md](docs/CLI.md#agent-commands)). [Details](docs/research/changelog-0.2.0-details.md#agent-commands)
- **Web agent commands run in `velocity daemon`,** which keeps one Chrome between commands; the first web command starts the daemon if none answers, and `daemon stop` closes the browser. iOS agent commands leave WebDriverAgent running between commands (`snapshot` 3.7 s → 0.6 s on a simulator).
- **An agent command holds the device for its whole length**; a second one waits, or gets `DEVICE_BUSY` after 10 s (`VELOCITY_LEASE_WAIT_MS`, which `velocity run` also uses). [Details](docs/research/changelog-0.2.0-details.md#agent-lease)
- Failures report `delivered: yes` when the call changed the screen before failing (a `scroll` that never found its target).
- **Failure codes for agents**: `structuredContent.error` with `code`, `retry`, `delivered` and `next` ([MCP.md](docs/MCP.md#failures)). [Details](docs/research/changelog-0.2.0-details.md#agent-codes)
- **`snapshot` narrows:** `within: "@e12"` (only what lies inside that element), `find` (lines containing a text) and `limit` (with an `incomplete` header). The core `observe` takes `within` and `find`.
- **`velocity skills list|get <name>|stub`:** the agent guides compiled into the binary, so an agent reads the one matching the installed version.
- The core `act` waits for the screen to settle before it reports what changed.
- `bench/killtest/`: the kill test's pre-registration (`PREREG.md`, `prereg.json` with the GO thresholds) and `score.py`, which scores the runs for S0, S7, S_rel, cost and login secrecy and says GO or NO-GO. The apps, goals and model are left for the owner to choose and freeze.
- **MCP calls can't wedge the session**: a 55 s deadline per device call (`VELOCITY_MCP_CALL_TIMEOUT_MS`), then the session is rebuilt. [Details](docs/research/changelog-0.2.0-details.md#mcp-deadline)
- **`drag: { from, to, holdMs, moveMs }`** reorders lists on Android, iOS and the web ([DSL.md](docs/DSL.md#reordering-drag)). [Details](docs/research/changelog-0.2.0-details.md#drag)
- **`exec` step**: run an allow-listed host program with no shell, check it, and `capture` its output ([DSL.md](docs/DSL.md#host-commands-exec)). [Details](docs/research/changelog-0.2.0-details.md#exec)
- **`failures.md` on every run**: each failure with its step, `file:line`, log tail and evidence files, or "insufficient evidence". [Details](docs/research/changelog-0.2.0-details.md#failures-md)
- **`velocity verify-change "<description>"`** (and MCP `verify_change`): replays a trusted saved test, or explores a new gated one, and leaves PR evidence in one folder. [Details](docs/research/changelog-0.2.0-details.md#verify-change)
- **Logins an agent can call without seeing the password**: `velocity login`, `explore --after`, `${keychain:NAME}` and `${totp:NAME}` ([DSL.md](docs/DSL.md#logging-in)). [Details](docs/research/changelog-0.2.0-details.md#logins)
- **Docs site and `llms.txt`**: mdBook build and GitHub Pages deploy (`docs.yml`), `docs/llms.txt`, and [docs/AGENTS.md](docs/AGENTS.md). [Details](docs/research/changelog-0.2.0-details.md#docs-site)
- **The Velocity GitHub Action** (`action.yml`) and **`velocity init --ci github`**; `install.sh --sha256` pins the archive ([CI.md](docs/CI.md#the-velocity-action)). [Details](docs/research/changelog-0.2.0-details.md#action)
- `device-smoke.yml` runs nightly (Android emulator and a new headless-Chrome smoke, `tests/smoke/web-smoke.yaml`) and weekly on the iOS simulator, then checks every job reported.
- **`velocity migrate maestro <dir> --dry-run`** converts every flow and writes nothing: how many flows convert cleanly, with steps skipped or changed, or not at all, and the Maestro commands that don't convert, ranked by the flows they affect (`--report-json` for JSON).
- **`velocity results check --expect-shards N <paths…>`**: run after a sharded CI matrix, it fails (exit 1) when a shard reported nothing, a shard number is missing or doubled, a shard ran no tests, or a `results.json` / `junit.xml` is damaged. See [docs/CI.md](docs/CI.md#no-silent-passes).
- **The Velocity GitHub Action's `check-shards` mode**: the Action runs no tests and, after a sharded matrix, downloads every shard's `velocity-results-<platform>-shard-M-of-N` artifact and runs `velocity results check --expect-shards N`, so a shard that died or uploaded nothing fails the run. `velocity init --ci github` now uses it in the `results` job (no more download-artifact + curl|sh install).
- **API testing: the `http` step** (request, `expect:` checks with JSONPath, `capture:`); an `http`-only suite needs no device ([DSL.md](docs/DSL.md#http-and-api-tests)). [Details](docs/research/changelog-0.2.0-details.md#http)
- `velocity` now names the `http` step when a migrated Maestro flow uses `runScript`/`evalScript`.
- `velocity run --env-file PATH` (repeatable; also on `shard-plan`) reads `KEY=VALUE` files; `--env` wins over a file. [Details](docs/research/changelog-0.2.0-details.md#env-file)
- **`velocity explore` shows the model up to two of the project's tests most like the goal** as demonstrations (`--no-demos`, `--demos-from`); prompt `goal-3`. [Details](docs/research/changelog-0.2.0-details.md#demos)
- `explore` tells the model when its last action changed nothing on the screen list, or when it is back on a screen it saw in the last 4 turns.
- **Flakiness per failure kind**: history format 3 records each failure's kind, and flaky tests are reported with the split. [Details](docs/research/changelog-0.2.0-details.md#failure-kinds)
- `velocity migrate maestro`: `killApp`, `eraseText`, `clearState: { appId }` and absolute `tapOn` points convert; a point inside a selector is skipped with a warning. [Details](docs/research/changelog-0.2.0-details.md#migrate)
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
- **`--hunt` explains flakes**, ranking likely causes; `--suggest` tries the top fix in memory without editing the suite. [Details](docs/research/changelog-0.2.0-details.md#hunt)
- **`velocity api scaffold openapi.yaml`** writes an API-only suite from an OpenAPI 3 document:
  - requests ordered so producers come first;
  - one operation's response linked to another's parameters through `capture:`;
  - bodies taken from the spec's examples;
  - checks mined from the response schemas (types, required fields, enums, ranges, lengths, formats).

  `--confirm` sends the safe operations (GET/HEAD only) to a running server and drops every check it violates, reporting each mismatch. Source: AutoRestTest, SATORI, RBCTest. [CLI.md](docs/CLI.md).
- `velocity init` writes `velocity.schema.json`, puts the `yaml-language-server` schema line at the top of the suite, and adds the mapping to `.vscode/settings.json`. It merges only valid JSON, never overwrites a setting, and skips JSONC with a note.
- `bench/head-to-head/`: a reproducible Velocity vs Maestro benchmark on one emulator, alternating runs and reporting medians and p90.
- `.github/workflows/install-smoke.yml`: after each release, installs it through install.sh, Homebrew, npm and cargo on Linux and macOS, then runs an API-only suite.
- **Duration flags take units** (`500ms`, `30s`, `10m`, `1h`). A bare number keeps each flag's old unit (ms on `run`, `doctor` and the agent commands; s on `explore`), with a warning when it looks like the other unit.
- `results.json` carries `format_version` (now 2) and the run's `shard_plan`; `velocity results check` fails when shards computed different partitions. Format 2 adds an integer `duration_ms` beside every `duration` and writes the resource keys in snake_case; the `{secs, nanos}` object stays for format-1 readers (deprecated), and `results check` and `trace view` read both formats.
- `velocity run --test-timeout DURATION`: one attempt of one test that runs longer fails (and may be retried) while the rest of the run goes on; `--timeout` still ends the whole run.
- `run-metrics.json` next to `results.json` (with `--report json`): per-device tests, busy time, utilisation, retries and infrastructure failures, and run totals.
- `VELOCITY_LOG_FORMAT=json` writes log lines as JSON objects with the current test, device and attempt.
- `VELOCITY_ARTIFACTS_MAX_BYTES` caps the evidence kept in the artifacts folder (oldest earlier runs go first, this run's stay); a run warns at start when the artifacts disk has under 3 GB free. Allure attachments are hard links when on the same disk instead of copies.
- `velocity run --daemon` prints `=> queued: …` after 2 s when the daemon is busy with another run, instead of waiting silently.
- A shared lease dir for several users of one host: in a group- or world-writable `VELOCITY_LEASE_DIR` (`chmod 2770` or `1777`), lock files are made group- or world-writable, so the users contend for one lock instead of the second failing. [GUIDE.md](docs/GUIDE.md) now says what a lease covers: one host and one lease dir, not hosts sharing an adb server.
- `failures.md` gives a `Try:` line for not-found, not-actionable, unsettled-screen, test-timeout and infrastructure failures; the CLI's hint names `VELOCITY_ADB_PATH` when adb is missing.
- CLI errors print a `Try: …` hint under them where there is a known fix.
- `velocity migrate maestro`: `setPermissions`, `addMedia`, `clearKeychain`, `setAirplaneMode` and `setOrientation` become `exec` steps (adb / `xcrun simctl`), and a flow's header `env:` becomes `${NAME:-value}` defaults.
- `velocity migrate maestro`: `setLocation` becomes `exec` steps, `runFlow` with `when: visible/notVisible` becomes a guarded optional flow, and `copyTextFrom`, `runScript` and similar get more specific hints.
- `velocity version` prints a second line with the commit, target and rustc of a release build (`unknown` for other builds); the first line is unchanged.
- [docs/GUIDE.md](docs/GUIDE.md#troubleshooting): a troubleshooting table for first-run failures, and the environment variables outside the settings table.

### Changed

- `velocity-core` no longer depends on `velocity-nl` (and its HTTP clients): the executor runs natural steps through the `NaturalStepResolver` trait in `velocity-common`, which `NaturalResolver` implements. No behaviour change.
- Closed-set flags are checked when the command line is parsed, and `--help` lists their values: `run --platform`, `--driver`, `--framework`, `--report`, `explore --verify`, `--screenshots`, `mcp --profile`. A typo fails before any device or model work.
- `velocity init` no longer asks about `velocity-impact.yaml` before the first test exists; it is a next step instead, beside `velocity explore`. On iOS it says when the first run will build WebDriverAgent. The suite header points at docs/DSL.md, and the CI workflow's header says which release it is pinned to.
- `--help` and docs say "saved test" for what `verify-change` and the console save, and "suite file" for what `run`, `validate` and `lint` read.
- **Faster settles.**
  - A fresh read matching the screen that last settled counts as settled at any age. Before, that only held for 2 s, and any longer step paid the quiet window again.
  - `scrollUntilVisible` settles once instead of twice: with no swipe needed, 15 tree reads and 355 ms became 2 reads and 32 ms in the executor tests.
  - A configuration error from the driver fails a lookup or assertion at once instead of being retried until the step timeout (10.2 s on a headless suite without `launchApp`). `sync.mode: native` is unchanged.
- **The headless and Flutter drivers report settled at once.** Their rendering is synchronous, so they no longer wait out the 300 ms reaction window: 20 headless taps went from 6.82 s to 0.022 s per test.
- **Android `launchApp`** needs fewer adb round trips (9 → 7 first, 7 → 3 after another app). [Details](docs/research/changelog-0.2.0-details.md#launch-app)
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
  - Every third-party action is pinned to a commit SHA. CI checks that the committed tree-server jar rebuilds from source byte for byte, and that the third-party licence inventory (`scripts/third-party-rust.py --check`) is current.
- **JUnit:** infrastructure and internal failures are `<error>`, not `<failure>`; the suite `timestamp` is the run's start.
- **`velocity explore`:** screenshots sent to the model are downscaled to at most 1,000 px; the prompt is ordered for provider caching, and cost accounting counts cache reads and writes apart. An idle `wait` no longer counts as progress. Prompt version `goal-7`.
- **Explore answers are tiered.** An answer part is accepted when found on a screen, as a number only for a quantity goal, as yes/no only for a yes/no goal, and as words only when they came from a screen; parts code could not verify are left to the final check.
- **Explore's final check can hold by the log.** When the result is in another app or on an earlier screen, the check cites log turns instead of an element; such a pass writes no assertion from the final screen and the report says `finalBasis: log`. A pass that cites neither is still `unknown`.
- **Explore asserts a screen answer the final screen still shows.** Outside `--benchmark`, each answer part code saw on a screen, still on the final screen as a stable text, becomes an `assertVisible`.
- **Explore stops a loop that learns nothing.** A `back` is refused once only after a loop with no memo change, answer or changed text, and the same loop coming round 3 times that way ends the run as `stuck` (`revisitTurns` in the report). Reading loops are never taxed.
- **The benchmark gets its own prompt.** `--benchmark` sends a variant of the goal instructions without assert, launch and the test framing, instead of a note every turn. The prompt also merges two duplicate rules, splits `expect` into short bullets, adds scroll-inside and drag examples, and explains web drop-downs and file fields. Prompt version `goal-8`.
- **OpenAI-compatible requests are bounded.** They send `max_completion_tokens`, `reasoning_effort: "low"`, `temperature: 0` with a seed, and `detail: "low"` on goal screenshots. A parameter the endpoint rejects by name is dropped for the rest of the run. A key sent over plain http to another host is warned about.
- **`explore --price IN,OUT[,CACHE_READ[,CACHE_WRITE]]`** takes the provider's cache rates.
- **Explore notes are decisions.** Each turn's notes are a `notes` event of typed values (`loopBack`, `noEffect`, …), rendered to prose in one place. The tests assert on them, and run on tokio's paused clock (11 s to under 0.1 s).
- Explore decodes each screenshot once per turn, off the async workers. Computed numbers are typed only with `--values-from-goal` or `--values-from-screen`. A run with secrets and `--values-from-screenshots` or `--values-from-model` says so. Above the 150-element cap, layout rows are dropped first and the model is told what kinds were dropped. `ExploreOutcome` is the new name of `Outcome` (alias kept). The fake app is behind the `testkit` feature and its screenshots are real PNGs.
- Explore on iOS matches against the app's frame, and swatch colours use one absolute scale. Not yet run on a simulator.
- Benchmarks: every harness defaults to the same Sonnet price (2,10). The Claude bridge passes the request's `reasoning_effort` to `claude -p --effort` and logs hidden output tokens per call.

- **Exit codes** follow one contract: 0 pass, 1 test failure, 2 config, 3 device, 4 suite timeout, 5 internal, 129/130/143 signals ([CLI.md](docs/CLI.md)). [Details](docs/research/changelog-0.2.0-details.md#exit-codes)
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
- **Selectors in `steps.json` traces and the inspector API use the DSL's flat form** (`{"text": "Pay", "index": 1}`), not the enum's derived serde form (`velocity_core::selector_wire`). Older traces still load; a selector a later version can't read is dropped from that step instead of failing the trace.
- **Recorded selectors and typed text are exported literally.** MCP `export_session_flow` and the console recorder write `${` in a selector or text as `$${`, so a label like `Pay ${amount}` matches itself and reads no variable; refs keep the raw form.
- **MCP tool descriptions say what a tool changes, returns and whether to repeat it**; `act` lists `clear_state`, `until_visible` and `max_scrolls`, `idempotentHint` is set for `open_inspector`, `close_inspector`, `stop_app` and `task_stop`, `export_session_test` is an alias of `export_session_flow`, and `velocity skills get mcp-tools` has a per-tool table. `velocity skills get yaml` prints a guide to writing tests.
- The console mirror acknowledges `pause`/`resume` with `paused`/`resumed` in H.264 mode. MCP failures to write its own files (a task folder, a screenshot, an inline suite) are internal errors, not `INVALID_ARGS`.

### Deprecated

- **`results.json` `duration` as `{"secs", "nanos"}`.** Read the integer `duration_ms` (format 2); the object form will go in a later format.
- **`velocity inspect --flows-dir`** is now `--tests-dir` (it saves tests, not `flows:`); the old name still works.

- **Ambiguous action selectors** warn that they will be an error in 1.0; `selectors.strict` (`1` error now, `0` silence). [Details](docs/research/changelog-0.2.0-details.md#ambiguous)
- **JUnit `classname`.** Every test currently has the classname `velocity`. At 1.0 the default becomes the suite file's name; `--junit-classname suite` switches now. The change resets test history in CI tools once.
- **Flutter `hierarchy: widgets`** (the widget tree in headless Flutter runs) still works with a warning. The semantics tree is the default.

### Removed

- The experimental React Native headless sidecar, which was on `main` after 0.1.0 but never released, is gone. `--framework react_native --driver headless` is a configuration error (exit 2). React Native apps run on the device drivers, where the in-app sync probe has React Native support.
- `build.rs` no longer runs npm. Release binaries embed the console UI. A source build without `crates/velocity-inspector/frontend/dist` serves the API and a page explaining how to build the UI.

### Fixed

- **`velocity api scaffold` names every placeholder taken from the spec under `API_`** (a parameter `storeId` becomes `${API_STORE_ID}`). A spec could name a parameter `aws_secret_access_key`, and `--confirm` read that variable from your environment and sent it to the spec's server.
- **A missing element's report commits only as far as the evidence goes.** In this order:
  - **Earlier screen:** if the text was on an earlier step's screen, the report names that step.
  - **Typo:** if a near-spelling is on screen (two edits at most), the selector is most likely misspelled.
  - **Shared word:** if a visible row shares a word with the selector, the report names it and says the wording or the screen is wrong.
  - **Otherwise:** the step most likely ran on a different screen.

  Measured on three blind sets of seeded failures: 1 wrong cause in 17, against 3 in 19 when the report always committed.
- `failures.md` and MCP's `describe_failure` offer a closest element as the intended target only when it resembles the selector (score 0.5 or more); otherwise they say the step most likely ran on a different screen or app. A missing element names the app that was in front when it isn't the app under test.
- Healing no longer refuses a row whose label and cell show the same text as an ambiguous tie.
- Android: another app's crash or ANR dialog over the app under test is closed ("Close app") by the system-dialog handler; the app under test's own crash is still reported as a crash.
- **`failures.md` explains failures with the evidence that shows the cause.** The "Why" quoted any line containing "error" from the whole app log (21 of 25 seeded failures). It now gives the closest element and what the screen shows for a missing element, the expected and actual values for a failed check, the configuration problem, or the error lines logged during the failed step. On 30 seeded failures the cited evidence held the cause in 22 of 25 reports, against 3 before.
- Launching an app that isn't installed on an iOS simulator is a configuration error naming it; it was reported as "App … crashed: the app's process exited unexpectedly".
- Real iPhones: `launchApp` is a cold start, as on simulators and Android: a running app was only brought to the front, so a test inherited the previous test's screen and its launch arguments never arrived. On the devicectl route, arguments follow `--` (devicectl read `-name` as its own options) and a filler word (devicectl hands the `--` to the app, where it swallowed the first argument). Checked on an iPhone 12 with a test app that shows its arguments.
- Android: adb losing the device during a `launchApp` with arguments is a device error (retried, moved to another device), not a configuration error. A crash of an app Velocity didn't launch (a link cold-start) is reported again, dated from Velocity's first look.
- iOS: under a sheet, rows a list loaded since the last visibility read no longer count as visible.
- A locked iPhone no longer fails the runner start at once (xcodebuild may wait for the unlock); it is named if the start times out.
- `velocity history flaky` refuses a `--window` under 5 or a `--max-rate` outside 0..1, which disabled its gate.
- MCP `observe {what: a11y}` gives one element's findings one ref; agent sessions notice a device switch even before their first snapshot.
- GitHub masks secret-named `runFlow` parameters in the job log.
- `velocity migrate maestro` drops a `launchApp` argument name Velocity rejects, with a warning, instead of failing the file.
- `velocity settings` lists `nl.model`'s default as the provider's step model.
- **`velocity devices up --ios N` counts only the caller's clones.** A `velocity-<type>-<n>` simulator another owner booted (another CI job's `--owner`) is no longer counted as up or booted again; two `up`s at once no longer pick the same clone or clone number.
- **A cut-short `--parallel --stress` run reports on time.** After `--timeout` or a stop, the actors are stopped and every device's CPU stress turned off at once, under one 15 s limit, instead of one device after another with a 60 s limit each.
- **Pure-YAML suites no longer build the AI client.** With a model key set, every parallel and sequential run built the planner, whose HTTP client loads the system's TLS roots (3–4 s from the macOS keychain, more under load), even with no natural-language step or `verify:`. It is now built only when the suite needs it. This made two parallel-runner timeout tests fail on loaded machines.
- **Quarantine `expires:` dates are UTC**, so an entry expires at the same moment on a developer's machine and a CI runner.
- A history file that can't be locked is an internal error (exit 5), not a configuration one.

- **Android tree server protocol 19.** Another app's floating window (freeform, desktop windowing) no longer joins the dump as the app's popup, and the tap guard counts it as a cover; windows come from the active window's display; a resent request waits only as long as the host does. A step cancelled while the host reconnected or resent no longer kills the server or leaves an `adb forward` behind. Not run on a device.
- **iOS: what is hidden behind a sheet no longer matches.** With a sheet, popover or in-app alert up, the step's lookup tree reads WebDriverAgent's `visible` (one more read; `VELOCITY_IOS_SHEET_VISIBILITY=off`). A row switch whose trailing-end tap changed nothing (an RTL row) is tapped at its leading end. The crash watcher no longer waits behind a slow request. Not run on a simulator.
- **iOS picker wheels: a dead WebDriverAgent is a device error** (exit 3); under VelocityRunner a picker-wheel step is a config error. Not yet run on a simulator or device. [Details](docs/research/changelog-0.2.0-details.md#picker-wheels)
- **Explore reads a swatch's colour** from the screenshot in code, for unlabeled single-colour elements. [Details](docs/research/changelog-0.2.0-details.md#swatch)
- **Pixel changes are judged in colour, on a finer grid**, so a thin stroke counts as a change and a blinking cursor still doesn't. [Details](docs/research/changelog-0.2.0-details.md#pixel-grid)
- **Repeated swipes inside one element take new lanes** (code picks the lane). [Details](docs/research/changelog-0.2.0-details.md#swipe-lanes)
- **A tap in a web page that shows no change is not doubted**: the model is told it may have set hidden state. [Details](docs/research/changelog-0.2.0-details.md#web-no-change)
- **Explore waits up to 2 s for a result that comes after a return** to an earlier screen. [Details](docs/research/changelog-0.2.0-details.md#late-return)
- **An empty Android field no longer reads as filled with its placeholder** (it stays the `hint`). Tree-server protocol 17. [Details](docs/research/changelog-0.2.0-details.md#placeholder)
- **The final check confirms actions, not answers**: facts in an answer are grounded by code; asked-for actions must still be shown done. [Details](docs/research/changelog-0.2.0-details.md#check-actions)
- **The final check no longer passes on an assumption** that an app saved or sent something by itself. [Details](docs/research/changelog-0.2.0-details.md#check-assumption)
- **The final check sees what each action brought on screen** (up to ten texts per log line, redacted). [Details](docs/research/changelog-0.2.0-details.md#check-log)
- **Android reads the screen from a cleared accessibility cache** on API 34+ only (a no-op before). Tree-server protocol 16. [Details](docs/research/changelog-0.2.0-details.md#clear-cache)
- **Explore waits for web content.** Chrome and WebViews update the accessibility tree 1 to 1.3 s after a click, later than the 0.6 s second look explore gave a screen that seemed unchanged. On a screen holding a web view, explore keeps looking for up to 2 s.
- **A changed number is progress** in explore, and the history names it (`(text changed: …)`). [Details](docs/research/changelog-0.2.0-details.md#changed-number)
- **Explore stops walking a loop through `back`**: one more `back` from that screen is refused once. [Details](docs/research/changelog-0.2.0-details.md#back-loop)
- **An explore answer goes through the final check** before it ends the run (`--verify off` skips it). [Details](docs/research/changelog-0.2.0-details.md#answer-check)
- **iOS taps a settings-row switch on its knob.** A SwiftUI `Toggle` row is one `Switch` the width of the row; a tap at its centre lands on the label and does nothing. A switch more than three times as wide as tall is tapped on its knob first.
- **iOS sets picker wheels.** `type` into an `XCUIElementTypePickerWheel` now sets it (an exact value, else stepping it to the value, numbers matched by their leading digits) instead of failing; date and time pickers in iOSWorld tasks stalled on it.
- **iOS "back" closes a sheet.** With no navigation back button, `back` taps a Close, Cancel, Dismiss or ✕ button in the top quarter of the screen before falling back to the edge swipe, which never closes a sheet. Done and Save are never tapped: they commit.
- **iOS taps into a sheet over a navigation bar.** The navigation bar of the screen under a presented sheet no longer blocks taps on the sheet's own elements (a bar whose container is not an ancestor of the target).

- **A snapshot ref no longer moves to another element after a navigation**; it fails `STALE_REF`. [Details](docs/research/changelog-0.2.0-details.md#stale-ref)
- **A failed MCP step reports what failed it.** A `scroll` whose target never appeared, or a step the app crashed during, was reported as `INVALID_ARGS` ("fix the arguments"); it is now `NOT_FOUND`, `ASSERTION_FAILED`, `APP_CRASHED` and so on, from the kind of the error behind it.
- **Exported sessions no longer assert on a container's joined text** (or iOS's `BackButton` id). [Details](docs/research/changelog-0.2.0-details.md#joined-text)
- **Found by the trust-benchmark pilot (`bench/pilot/`, 2026-11-07), in `explore` and its gates:**
  - The final AI check judged the goal text against the last screen alone, so every multi-step goal ("open Network & internet and then Internet") came out `unverified`: the last screen can't show an earlier one was opened. It now judges the end state with the runner's own log of the actions done (as benchmark runs already did); the trust gates still decide trust.
  - The wrong-path gate found no neighbour for an element with a label and no text (`None == None` ruled out every iOS cell), so it was skipped on iOS; it now compares text and label together.
  - The wrong-path gate picked the summary line of the tapped row ("34 apps installed" under "All apps"), which opens the same screen, and called good Android tests weak. A neighbour stacked inside the same one-row container no longer counts, and the search goes up to five levels (an Android title sits two levels inside its row).
  - A wrong-path lookup that failed ended the whole explore with no report; the gate is now reported as not checked.
  - A value the goal names ("check that the yearly plan costs $99") may be asserted although it looks volatile. Refused, the assertion fell back to the element's id, and the test passed with the price broken.
  - Web labels (`aria-label`) are written as `accessibilityId:`, not `text:`, which the page matches against visible text only: such taps waited out their 10 s timeout and the gate's lookup failed.
  - Explore's screen and expectation assertions skip a container whose text is its children's joined (a web `<body>` "shows" every text on the page), which failed every replay.
- **Found by running AndroidWorld, MiniWoB++ and iOSWorld (2026-11-07/08):**
  - **Android taps are a finger.** The tree server injected touches with an unknown tool type, and apps that route events by tool type dropped them: tapping a file in the Files app (DocumentsUI) did nothing, which blocked every task that opens a file. Touches now carry `TOOL_TYPE_FINGER` and a pressure, as `adb shell input tap` does (tree server protocol 14).
  - **iOS taps into a sheet over a tab bar.** The tap guard refused a chat sheet's message field as "covered by the tab bar": the tab bar of the screen underneath stays in the tree, visible. A target in a container listed after the tab bar that covers it is no longer covered, and a bar reported not visible covers nothing. Content scrolled under a screen's own tab bar or navigation bar is still refused.
  - **Android popups are in the tree.** The tree server dumped the active window and side panes only, so a window drawn in front of it (the text-selection toolbar's Cut, Copy, Paste, Select all; other popups) was on the screen but not in the tree, and nothing could tap Paste. Application windows in front of the active one that overlap it are dumped too; what is behind it stays hidden (tree server protocol 15).
  - **A multi-item goal isn't passed from its last item.** "Add these three recipes" ended on the last recipe's page, and the final check accepted that one item as the result: 5 of AndroidWorld's 9 false successes were this. The final check now requires every item the goal names in its expected state, and the agent is told to finish on a screen that shows them all (prompt goal-6).
  - **An agent command re-reads an empty screen.** Pressing back into a Flutter app on a real tablet reported "0 elements": the tree was read while the app attached its accessibility nodes. An action's delta that reads an empty screen is read again (up to 3 times, 0.5 s apart).
  - **Explore taps read-only pickers.** An Android WebView `<input readonly>` date field reports enabled=false (clickable, focusable), so explore refused to pick it and the executor's strict check refused to tap it: every date picker task failed. A tap on a text field that reports disabled is allowed and runs with `force` (typing into it is still refused). MobileMiniWoB's four date tasks went from 0 to 4.
  - **A swipe inside a slider reaches its end.** `swipe: { within: … }` drew its 20%–80% stroke on a slider too, so "brightness to maximum" stopped at 80% of the track. On a slider (Android SeekBar or Slider, iOS Slider, the web's `role=slider`) the stroke now runs from a fifth in to past the far edge, where the slider clamps (brightness 100 → 255 and → 1 on the AndroidWorld emulator).
  - **Explore can drag**: `drag` moves one listed element onto another (the DSL's `drag: { from, to }`), for reordering lists, sliders and drop targets.
  - **Web icons are visible to agents.** A clickable element with no text, label or id (`<span class="trash">` drawn by CSS, `<i class="fa-star">`) got no entry in the agent's element list. It is now captured with a name from its title, alt, class words or image file (as `hint`), listed, and selected by its class (`Element.css_class`).
  - **`className:` on the web also matches an HTML tag**, as Velocity's own tree matching does: the selector builder's fallback for a bare checkbox (`className: input`) found nothing in the page.
  - **Explore: progress the tree can't see counts.** When a turn took a screenshot, an action that changed pixels but not the tree (a canvas stroke, a maze move) is progress, not a no-op. An action that seemed to change nothing is looked at again 0.6 s later (a chooser or another activity opening). Actions that changed the screen but missed the model's expectation (looking through menus) are allowed twice the stuck limit.
  - **`velocity explore --values-from-screenshots`:** the model may type text it read from a screenshot sent in this run (a receipt, a gallery image, a video frame). Code can't check pixels, so it is a separate opt-in, and only after a screenshot was sent.
  - **`VELOCITY_EXPLORE_SHOTS_DIR`** saves each turn's screenshot, for an outside judge (iOSWorld).
  - **`--model openai-compatible:m` uses `VELOCITY_NL_API_URL`.** `--model` hid the environment's endpoint from every provider, so a provider with none of its own could never be used that way, and the error asked for the variable it ignored.
  - **`velocity run` waits for a held device** (up to `VELOCITY_LEASE_WAIT_MS`, 10 s) instead of failing at once; agent commands wait as long.
- **Web elements keep their `id` attribute apart from their label**: written tests use `id:` (`Element.dom_id`). [Details](docs/research/changelog-0.2.0-details.md#dom-id)
- **A replugged iPhone no longer stays unreachable**: the USB forward looks the device up again. Not yet checked on hardware. [Details](docs/research/changelog-0.2.0-details.md#replug)
- **Corporate proxies and custom CAs**: HTTPS trusts the OS store, or `SSL_CERT_FILE`/`SSL_CERT_DIR` ([CLI.md](docs/CLI.md#proxies-and-certificates)). [Details](docs/research/changelog-0.2.0-details.md#proxies)
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
- An iOS runner is never launched into a taken port; one failing 3 times in 2 minutes stops with `RunnerFailing` (exit 3). [Details](docs/research/changelog-0.2.0-details.md#runner-port)
- Tests no longer write `.velocity/healer-snapshots.json` into the source tree: `RuntimeConfig::state_dir` and `TestExecutor::with_state_dir` put the self-healing state elsewhere.
- `perf_budget` is no longer flaky: healing snapshot state is kept per test in a temp dir. [Details](docs/research/changelog-0.2.0-details.md#perf-budget)

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
- **Android:**
  - The tree server clears its accessibility cache only after a window change or at most once a second, instead of before every read. On API 26–33 it clears through `AccessibilityInteractionClient` by reflection (before, nothing was cleared below API 34). Not run on a device.
  - A failed `screencap` is a screen-capture error, no longer classified as a lost connection.
  - `ScreenRecord` stops and cleans up its device recording when dropped.
  - Key events carry the keyboard input source.
  - The tap guard ignores windows that take no touches (not touchable, `ScreenDecorOverlay…`).
  - Upgrading a running older tree server waits for it to quit after `QUIT` before starting the new one.
- **Web:**
  - A banner over a same-origin iframe that takes the click now fails the tap (and it is retried), instead of reporting success.
  - `aria-hidden` text is left out of accessible names; a wrapper element no longer repeats its children's text in the tree.
  - A crashed renderer is `BrowserConnectionLost` at once.
  - Printable ASCII is typed as real key events, so `keydown`/`keyup` listeners fire; other text goes in with `insertText`. `VELOCITY_WEB_TYPING=insert` restores insert-only typing.
  - An icon button's `hint` comes from what it holds (an svg `<title>`, an `<img alt>`, a child's `aria-label`, a sprite's `<use href="#icon-…">`, an icon font's class) before its own classes, and utility or hashed classes (`p-2`, `hover:bg-gray-100`, `Button_root__x3F9a`, `css-1x2y3z`) are no longer used as a name. An svg `<title>` outside `aria-hidden` also names its button for `accessibilityId:`.
  - `confirm()` and `prompt()` can be accepted: `web.dialogs: accept` (setting, `VELOCITY_WEB_DIALOGS`), with `web.promptText` as the prompt's answer. The default is still to dismiss them.
  - On a touch profile, a tap whose point something else covers no longer runs the cover's touch handlers: the hit-target check blocks `touchstart`/`touchmove`/`touchend` too.
  - `inputText` on an `<input type=file>` (or its label) sets its files: the text is the paths, one per line. A missing file is a config error.
  - A `tap` on an `<option>` of a closed drop-down chooses it; it failed as "zero size".
  - Downloads are refused with a warning naming the file; `web.downloads: <folder>` (`VELOCITY_WEB_DOWNLOADS`) saves them there. Before, they went into the temporary profile and were deleted.
- `velocity daemon start` takes a lock, so a second daemon can't start; a daemon that goes away mid-run is a device error (exit 3).
- `velocity devices up` picks ports and launches under a host lock, so two processes no longer take the same port.
- A retry of a test that begins with `launchApp` cold-starts the app.
- **`velocity explore`:** the written YAML keeps a swipe's `direction`, `from`, `to` and `within`, and `scrollUntilVisible` writes its `maxScrolls`.

### Security

- **The Rule of Two for agent sessions**: the default and read-only MCP profiles list no tool that changes the machine ([SECURITY.md](docs/SECURITY.md#9-agents-the-rule-of-two)). [Details](docs/research/changelog-0.2.0-details.md#rule-of-two)
- **An explore task's `out` stays the agent's suite.** Running it later (`run_task suite`, or as an `after`) gets the checks an agent-written suite gets, so its `${…}` can't read the server's environment; the list of such files lives in the tasks folder (`agent-written.txt`) and survives a restart. Outside `--profile full`, an explored test that types `${EMAIL}` now needs `EMAIL` in `VELOCITY_SECRETS`, as a suite in the tasks folder already did.
- **MCP `run_task` keeps model-chosen `out`/`script` paths inside the project.** [Details](docs/research/changelog-0.2.0-details.md#run-task-paths)
- **VelocityRunner refuses cross-site and unauthenticated requests**: a per-launch token, and 403 for an `Origin`, cross-site fetch or foreign `Host`. [Details](docs/research/changelog-0.2.0-details.md#runner-auth)
- **A cabled iPhone's runner binds the phone's loopback (R-4)**, which the USB (usbmux) forward reaches, instead of every interface, Wi-Fi included.
- **The Android tree server drops non-protocol input** and requires a per-start token (protocol 13). [Details](docs/research/changelog-0.2.0-details.md#tree-input)

- The inspector no longer accepts requests from any website or LAN host. It binds `127.0.0.1` and checks a per-session token, `Host` and `Origin` on every request and WebSocket. It sends no CORS headers, and it serves artifacts only from the run's folder, under a Content-Security-Policy.
- On simulators, WebDriverAgent, its MJPEG stream and VelocityRunner listen on loopback; cabled iPhones too, by default (`VELOCITY_IOS_DEVICE_LOOPBACK=0` opts out). [Details](docs/research/changelog-0.2.0-details.md#loopback)
- Secrets come from environment variables, are masked everywhere, never sent to a model, and written into YAML only as `${NAME}`; MCP agents type only `VELOCITY_SECRETS`. [Details](docs/research/changelog-0.2.0-details.md#secrets)
- Values are interpolated in one pass, so a variable's value can't inject another variable.
- Device shell commands no longer interpolate unquoted app ids or typed text.
- History, cache, lease and profile files are created `0600` (directories `0700`), and atomic writes replace a symlink instead of following it.
- AI provider keys are read only from environment variables, never logged, and refused when a key value is put where a variable name belongs (`apiKeyEnv`).
- **Android tree server protocol 18: mutual HMAC handshake**; the token never crosses the socket. Not run on a device. [Details](docs/research/changelog-0.2.0-details.md#tree-18)
- **The Android video server authenticates both ways** (video server 2): a per-start token file and an HMAC handshake before the stream, so another local process can't watch the screen through its `adb forward` or hold its client slot (R-3 residual). iOS never pastes a secret or into a secure field (the simulator's pasteboard is the Mac's), and building an unpinned `ios.wdaRef` warns. Not run on a device.
- **Agent-written suites over MCP** (outside `--profile full`) can't read arbitrary `${ENV}`/keychain values or send secrets anywhere but typed text. [Details](docs/research/changelog-0.2.0-details.md#mcp-suites)
- **MCP tasks:** `run_task` suite paths must resolve inside the project; a task that panics ends as an error and frees its device; `notifications/cancelled` rebuilds the session.
- **Explore and prompt injection:** screen titles and names are flattened to one line and quoted in the prompt. `--values-from-screenshots` accepts text only after a screenshot of a screen with pixel-only content (an image, a canvas).
- **MCP `run_task {suite}` in the tasks folder** (a file an agent's task wrote, such as the `task_inject` lines) gets the checks an inline `yaml` gets: no `exec`, no host secrets.
- **Console:** the explore goal is passed after `--`, so a goal starting with `-` is never read as a flag (I-2), and the console UI is served with a Content-Security-Policy (`script-src 'self'`, I-3).
- **Automated security checks** (`.github/workflows/security.yml`, `scripts/security-check.sh` locally): RustSec advisories, OSV-Scanner over every lockfile, `npm audit`, gitleaks over the whole history, zizmor on the workflows, Semgrep SAST, an `unsafe` inventory per crate, CycloneDX SBOMs and weekly cargo-fuzz runs of six parsers; OpenSSF Scorecard; every workflow job under harden-runner with credentials not persisted. Releases add the SBOMs, a signed `SHA256SUMS.sigstore.json`, and `cargo auditable` dependency data in the binaries. hyper 1.12 (drops the unmaintained `pin-utils`, RUSTSEC-2026-0441), `rand` 0.9.5 (RUSTSEC-2026-0097), `memmap2` 0.9.12 (RUSTSEC-2026-0186), `grid` 1.0.1 and the frontend's build tooling (vite, postcss, nanoid and others, via `npm audit fix`) are updated. The Velocity Action passes `platform` and `shard` to its scripts as variables and checks `shard` is `M/N`. [SECURITY.md](SECURITY.md#automated-security-checks)

- **Security audit 2026-11** ([docs/research/security-audit-2026-11.md](docs/research/security-audit-2026-11.md)):
  - MCP (outside `--profile full`): explore's and `verify_change`'s `after` file in the tasks folder gets the inline-suite checks; an agent-written suite may not set the browser binary, its flags, a mock file, a non-http start page, `app`, the headless/Flutter/React Native sources, `ios.wdaRef` or a non-http(s) `appId`/`goto` URL, nor read `${…}` in `tags`; its `http` steps can't reach link-local or cloud-metadata addresses; a plain `launch_app` takes only http(s) web URLs and `type_text` can't name a host file outside the project; the explore goal and the `verify_change` description go after `--`.
  - `velocity explore` writes screen text and goal literals with `${` escaped, so a page showing `${AWS_SECRET_ACCESS_KEY}` can't make the replay read it.
  - The NL cache (`.velocity/nl-cache.json`) replays only actions grounding can produce (no `http`, `launchApp`, `setConfig` from an edited cache).
  - `http` steps: a request with headers of its own follows redirects on its own origin only, an `https` → `http` redirect stops, and a response body over 32 MiB fails the step. AI provider clients follow no redirects; `http://127.attacker.example` is no longer taken for loopback.
  - Web: Chrome refuses invalid TLS certificates (`web.insecureCerts: 1` opts out); console lines are cut at 16 KiB and the log buffer at 64 MiB; a page nested deeper than 512 elements is flattened instead of overflowing the stack.
  - Lease files, the daemon lock and log are opened without following symlinks; a lease file that is a link or special file is refused; the daemon and its clients refuse a peer of another uid; with `HOME` unset, `~/.velocity` is a private `velocity-home-<uid>` dir instead of the shared `/tmp/.velocity`.
  - `exec`: a line with `${…}` is split before it is filled, so a value is one argument; a relative program is resolved before `cwd` applies.
  - `results check` escapes control characters (no GitHub workflow-command injection from a `results.json`); `trace view` reads evidence files only inside the trace folder; the inspector's review-only server serves only the review routes.
  - Android: the tree/video server token file is created `0600` in one call and deleted on quit; a squatter's endless ping reply is capped; a secret's read-back is never quoted; `uiautomator dump` runs under `umask 077`; crash notes are capped. Not run on a device.
  - Hardening: nested `${A:-${B:-…}}` defaults are limited to 32 levels; PNGs over 64 Mpx and APK entries over 64 MiB are refused; `#![forbid(unsafe_code)]` in the nine crates without `unsafe`.

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
- **Verification:** Android 14 emulators, iOS 26 simulators and headless Chrome; one Android tablet (Settings only); real iPhones never ([STATUS.md](docs/STATUS.md)). [Details](docs/research/changelog-0.2.0-details.md#verification)

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

## [0.2.0] - unreleased

Everything planned for 0.2.0 is under [Unreleased] above. `scripts/bump-version.sh 0.2.0 --date` moves it here and dates this heading; delete this paragraph then. (CI needs a non-empty `[0.2.0]` section while the workspace version is 0.2.0.)

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
