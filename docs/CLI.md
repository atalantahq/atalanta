# CLI

`velocity <command>`. The `--color auto|always|never` flag works with every command. Logs go to stderr and are filtered by `RUST_LOG` (default `warn`, with chromiumoxide's message handler at `error`: Chrome versions it can't parse made it warn on every web run). `velocity <command> --help` lists every flag.

## Commands

| Command | What it does |
|---|---|
| `init` | Create a starter project from a running app: a first test read from its launch screen, a CI example, `.gitignore` entries ([below](#init)) |
| `doctor [PATH]` | Check the machine and project: tools, devices, runner caches, suites ([below](#doctor)) |
| `run [PATH]` | Run a suite file, or every `*.yaml` in a folder (default `-c velocity.yaml`). A folder run skips `velocity-impact.yaml` and `velocity-quarantine.yaml` |
| `validate <file\|folder>` | Parse and validate suites without running them |
| `api scaffold <spec> [-o FILE] [--base-url URL] [--confirm]` | Write an API test suite from an OpenAPI 3 spec ([below](#api)) |
| `lint [PATH]` | Find fragile selectors and fixed waits |
| `schema [-o FILE] [--markdown]` | Print the JSON Schema for suite YAML, or the [DSL reference](DSL-REFERENCE.md) |
| `settings [filter]` | Show named settings, their defaults and current values |
| `shard-plan [PATH] --total N` | Show each shard's tests and the plan fingerprint (no devices needed) |
| `device list\|boot\|shutdown\|screenshot` | Manage simulators and emulators (alias `devices`) |
| `devices up --android N --ios N` / `devices down` | Boot or clone the devices a parallel run needs, and stop them afterwards |
| `inspect [--open]` | Start the console: live device mirror with input, element tree and selectors, recording, suite runs with live steps, explore, task history, `/review` ([below](#inspect)) |
| `baselines review [--serve [--baselines-dir DIR]]` | List failed `assertScreenshot` comparisons, or serve the review page (`--baselines-dir`: its Accept writes into that `velocity-baselines` folder, for CI artifacts) |
| `baselines accept\|reject <name\|all>` | Make the actual screen the baseline, or keep the old baseline |
| `heal [--accept\|--discard\|--reject N]` | Review selectors that self-healing fixed during passing tests |
| `history merge <files…>` | Merge the history files of several CI jobs |
| `trace view [PATH] [--no-open]` | Open the trace viewer: a test's `traces/<test>/` folder (or its `steps.json`/`trace.html`), or a run's artifacts folder or `results.json` (an index of every kept trace) |
| `repl -d <device>` | Run steps one at a time (`:tree`, `:find`, `:shot`, `:save`) |
| `explore "<goal>" --app-id <id>` | An AI agent reaches the goal on a device and writes a deterministic YAML test with assertions, then replays it with no model calls ([EXPLORE.md](EXPLORE.md), flags below) |
| `mcp [-d DEV] [-p PLATFORM] [-c SUITE] [--read-only] [--notify …] [--tasks-dir DIR]` | Start the MCP server on stdio ([MCP.md](MCP.md)) |
| `mcp install [all\|claude\|cursor\|codex\|windsurf\|vscode\|cline\|gemini] [--dry-run] [--uninstall]` | Register the MCP server with agent clients and install the Velocity agent rules |
| `mcp config <client>` / `mcp rules` | Print a client's config snippet / the agent rules |
| `migrate maestro <dir>` / `migrate waits <suite>` | Convert Maestro flows; remove redundant `wait:` steps |
| `nl-eval` | Measure planner consistency (pass^k) on saved natural-language traces, or compare models on the built-in task set (`--model provider:model …`, see below) |
| `signing check\|devices\|register-device\|profile` | iOS signing with an App Store Connect API key |
| `daemon start\|stop\|status` | Keep drivers warm between runs (`run --daemon`) |
| `version` | Print the version |

## `init`

From a running device to a first passing test. Interactive on a terminal; every answer is also a flag.

| Flag | Meaning |
|---|---|
| `-p ios\|android\|web` | Platform. Default: the one with a running device (asked when both) |
| `--app-id ID` | Package / bundle id. Default: the app in front (Android `dumpsys activity`; iOS the one running user app), else asked from `pm list packages -3` / `simctl listapps` |
| `--url URL` | Web: the start page |
| `-d ID` | Device. Default: the only running one, or asked (iOS: simulators; pass a real iPhone's UDID) |
| `--dir PATH` | Suite folder (default `velocity/`) |
| `-y, --yes` | Ask nothing; take the defaults (CI) |
| `--force` | Overwrite existing files |
| `--run` | Run the new suite at once and print the time from `init` to green |
| `--impact` | Also write a `velocity-impact.yaml` stub (nothing mapped yet, so every test still runs) |

It writes `<dir>/<app>.yaml` (`com.android.settings` → `settings.yaml`), `<dir>/ci/github-actions.example.yml` (sharded, JUnit + GitHub reports; copy it to `.github/workflows/`) and adds `velocity-results/` and the local heal files to `.gitignore`. The test launches the app and asserts one or two elements of its first screen: an id (`id:`, layout wrappers last) first, then a short text that matches exactly one element and carries no digits, times or prices. Commented `tap` / `inputText` examples with `${TEST_EMAIL}` placeholders follow. Every file is emitted by serde_yaml, and the suite is parsed and validated before it is written. While it reads the screen it holds the device's lease (waiting up to a minute for a run that has it).

Exit codes: 0 (or the `--run` result), 2 bad flag or a file that exists, 3 no running device.

## `doctor`

Checks, by area, each with a deadline (`--timeout MS`, default 3000 per command), areas in parallel; a run takes about a second.

| Area | Checks |
|---|---|
| Host | OS and arch, Velocity version, free memory and disk against the `devices up` bar (35% / 3 GB, 3 GB), state / lease / artifact dirs writable, lease files left by exited processes |
| Android | adb (PATH, `VELOCITY_ADB_PATH`, or found only in the SDK), server version and response, devices (online / unauthorized / offline), API level, tree server push + handshake (`--deep`), emulator and AVDs; Java is not needed |
| iOS (macOS) | Xcode and `xcode-select -p`, license, simctl, booted simulators, the WebDriverAgent / VelocityRunner build cache and whether the pinned tag fits this Xcode, devicectl and paired iPhones, `VELOCITY_IOS_TEAM_ID` when one is connected |
| Web | Chrome / Chromium (`CHROME`, PATH, /Applications) |
| AI | Which provider keys are set (names only), the model, the API host's reachability (`--deep`) |
| Project | `velocity.yaml` or `velocity/` (or the PATH given): suites parse and validate, their app is installed on a running device, `velocity-impact.yaml` reads and names real tests |

Marks: `✓` ok, `!` warning, `✗` failure, `-` skipped; a fix prints as `Run: <command>` or `Guidance: <text>`. A missing toolchain fails only for a platform you named (`-p`) or a suite uses; otherwise it is skipped.

| Flag | Meaning |
|---|---|
| `--json` | The report as JSON: `verdict`, `checks[]` (`id`, `area`, `status`, `summary`, `detail`, `fix: {kind: run\|guidance, value, safe_auto}`) |
| `--deep` | Also push the tree server to a device and PING it (it is only started when no other process holds the device), and connect to the AI API host |
| `--fix` | Apply the safe fixes, then check again: restart a wedged adb server, `adb reconnect offline`, remove dead-pid lease files, re-push the tree server. Nothing interactive or needing sudo |
| `--strict` | A degraded verdict (warnings) exits non-zero too |
| `-p ios,android,web` | Platforms you test on |

Exit codes: ready or degraded 0 (degraded is 3, or 2 for project warnings, with `--strict`); blocked 2 when a project check failed, else 3.

## `run` flags

### Target

| Flag | Meaning |
|---|---|
| `-p ios\|android\|web` | Platform. Default: the suite's `platform`, otherwise from the device id |
| `-d` | Device id or name. With `--parallel` it also takes `a,b` or a `*glob*` |
| `--parallel N` | Use up to N devices. On the web, N browser contexts |
| `--provision` | Boot what the run is missing first |
| `--app PATH` | Install this build (skipped when the device already has it) |
| `--driver device\|headless` | Device or headless driver |
| `--framework native\|react_native\|flutter` | App framework |

### Selection

| Flag | Meaning |
|---|---|
| `--tags`, `--filter GLOB` | Choose tests. A selection that matches nothing exits 2 |
| `--last-failed`, `--failed-first` | Use the last run's failures (from the history) |
| `--changed-since REF` | Only tests the change can affect, plus risky and unmapped ones (`velocity-impact.yaml`) |
| `--fail-if-none` | With `--changed-since`: exit 2 when no test is affected |
| `--shard M/N` | 1-based shard. `--shard-index/--shard-total` is the 0-based form |
| `--shard-timings FILE` | Balance shards by duration, from a history JSON or JUnit XML. Give every shard the same file |
| `--history-from PATH\|GLOB` | Merge history from other jobs before anything reads it. See [CI.md](CI.md) |

### Execution

| Flag | Meaning |
|---|---|
| `--retries N` | Retry failures (at most 10). A pass on retry counts as flaky |
| `--fail-on-flaky` | A flaky pass exits 1 |
| `--timeout MS` | Suite timeout. Exceeding it exits 4 |
| `--fail-fast` | Stop at the first failure |
| `--env K=V` | Variables for `${K}` |
| `--env-file PATH` | `KEY=VALUE` lines (`#` comments, `export `, quotes; no shell expansion) as variables (repeatable). `--env` wins over a file, a later file over an earlier one, and any file over the environment. Never loaded unless named. A bad line exits 2 with its line number, never its text |
| `--set KEY=VALUE` | A named setting or any config path (`sync.quietMs=100`) |
| `--quarantine FILE` | Known failures to forgive (default `velocity-quarantine.yaml`) |
| `--cache-results` | Skip tests that passed when nothing has changed |
| `--update-baselines[=missing]` | Rewrite missing and changed `assertScreenshot` baselines, or only the missing ones |
| `--verify off\|final\|checkpoints\|strict` | AI verification for every test, over `verify:` in the suite and tests ([DSL.md](DSL.md#ai-verification-verify)). Anything but `off` needs a model key; without one the run exits 2 |
| `--daemon` | Run through a running `velocity daemon` |

### Flakiness

| Flag | Meaning |
|---|---|
| `--detect-new-flakes` | Run tests that are new to the history several times |
| `--shuffle [--seed S]` | Shuffle the test order. `--jitter MS` adds random delays |
| `--hunt N [--stress]` | Run N perturbed rounds and report outcome flips (exits 1 if any). For each flaky test it says where passing and failing rounds diverge (the failing step, the timings before it, order and stress) and ranks the likely causes |
| `--suggest` | With `--hunt`: the YAML change for each flaky test's top cause (a `timeoutMs`, an `assertVisible` before the step, `isolated`/`dependsOn`), re-checked by hunting again with it applied in memory. The suite is never edited |

### Output

| Flag | Meaning |
|---|---|
| `--report junit,json,html,github,allure` | Reports to write (default `junit`). `github` is added automatically in GitHub Actions |
| `--artifacts-dir DIR` | Where everything goes (default `./velocity-results`) |
| `--trace off\|on-failure\|always` | Per-test traces (default `on-failure`), with a `trace.html` viewer next to each |
| `--video off\|on-failure\|always` | Per-test screen video (default `on-failure`, or `artifacts.video`). See [Failure evidence](#failure-evidence) |
| `--logs off\|on-failure\|always` | Per-test device logs (default `on-failure`, or `artifacts.logs`) |
| `--pause-on-failure` | Stop at the first failing step, print where the evidence is, and wait: Enter continues, `r` retries the step, `q` aborts the run (exit 130). Interactive terminals only; ignored with a warning otherwise and with `--daemon` |
| `--profile FILE`, `--profile-steps` | A Chrome/Perfetto trace of the run; where time went per action |

## `nl-eval`

| Flag | Meaning |
|---|---|
| `-k N`, `-n N` | pass^k, and re-plans per task (default k) |
| `--traces DIR` | Saved traces to re-plan (default `.velocity/nl-traces`) |
| `--model PROVIDER:MODEL[@URL]` | Compare models (repeatable). Any provider in [AI-PROVIDERS.md](AI-PROVIDERS.md); the key comes from that provider's variable |
| `--tasks builtin\|DIR` | The task set: `builtin` (fixed screens and expected steps, the default with `--model`) or a trace directory |
| `--price PROVIDER:MODEL=IN,OUT` | Dollars per million input/output tokens, for the cost column (repeatable) |
| `--recommend` | Name the default: best pass^k, then the cheapest within 2 points of it (by cost, else tokens, else latency) |
| `--json FILE` | The full report (per model: pass@1, pass^k, consistency, errors, p50/p95 latency, tokens, cost) |
| `--min-pass-k F`, `--max-error-rate F` | Exit 1 below the pass^k floor; exit 3 when calls mostly failed |

## `explore`

`velocity explore "<goal>" --app-id <id> [flags]`. How it works, and what the test it writes contains: [EXPLORE.md](EXPLORE.md).

| Flag | Meaning |
|---|---|
| `--app-id ID` | The app under test (required) |
| `-p, --platform ios\|android\|web`, `-d, --device ID` | Default: the platform from the device id; the first booted device no other Velocity process holds. A held device is waited for (`--lease-wait`, default 600 s) |
| `--model PROVIDER:MODEL[@URL]` | The goal model (default: the configured provider's goal model, `VELOCITY_NL_EXPLORE_MODEL`) |
| `--script PLAN.json` | Play a fixed plan instead of a model (no key needed) |
| `-o, --out FILE`, `--force` | Where the test goes (default `<goal-as-file-name>.yaml`); an existing file is only overwritten with `--force` |
| `--name NAME` | The test's name (default: the goal) |
| `--data NAME=VALUE` | A value the agent may type; the test types `${NAME}` (repeatable) |
| `--secret NAME` | A secret from `$NAME` the agent may type; never sent to a model, shown or written; the test types `${NAME}` (repeatable) |
| `--max-steps N` | Planning turns (default 40) |
| `--timeout SECS` | Exploration time (default 600; exit 4 when hit) |
| `--max-tokens N`, `--max-cost USD`, `--price IN,OUT` | Token and spend budgets; `--max-cost` needs `--price` (dollars per million input,output tokens) |
| `--verify off\|final\|checkpoints\|strict` | AI check of the goal (default `final`): success needs the checker to cite real elements |
| `--replay N` | Replays of the written test, no model calls (default 1; 0 skips) |
| `--no-repair` | No repair round when a replay fails |
| `--no-demos` | Don't show the model the project's tests most like the goal (on by default; never with `--benchmark`) |
| `--demos-from DIR` | Where those tests are looked for (repeatable; default the folder `--out` writes to, else the current one) |
| `--clear-state` / `--no-launch` | Start clean (`launchApp: { clearState: true }`), or from the screen as it is (no launch step) |
| `--keep-nl` | Also write `<out>.nl.yaml`: the same steps in plain English |
| `--dry-run` | Explore and report only; no test written or replayed |
| `--json` | The result as JSON on stdout (live lines go to stderr) |
| `--inject-file PATH` | Lines appended to this file while the agent runs are given to it as instructions |
| `--benchmark` | For an outside harness (AndroidWorld): start from the screen as it is, no test written or replayed, no auto-dismissed dialogs, `launch`/`assert` off, screenshots every turn ([EXPLORE.md](EXPLORE.md#benchmark-mode---benchmark)) |
| `--values-from-goal` | The model may propose text to type; accepted only when grounded in the goal or the screens seen ([EXPLORE.md](EXPLORE.md#--values-from-goal-off-by-default)) |
| `--open-app NAME=ID`, `--apps-file PATH` | Apps the agent may open by name (`openApp`); the file is a JSON object `{"Name": "app.id"}` |
| `--screenshots never\|poor\|always` | When a screenshot goes with the screen list (default `poor`; `always` with `--benchmark`) |

Exit codes: 0 when the goal was reached, the test written and every replay passed; 1 otherwise (gave up, stuck, unverified, out of budget, a replay still failing after the repair); 2 configuration; 3 device; 4 `--timeout`.

## AI environment

`VELOCITY_NL_PROVIDER`, `VELOCITY_NL_MODEL` (may be `provider:model`), `VELOCITY_NL_EXPLORE_MODEL`, `VELOCITY_NL_API_URL`, `VELOCITY_NL_API_KEY`, `VELOCITY_NL_IMAGES=0`, `VELOCITY_NL_OUTPUT=json_schema|tools|text`, `VELOCITY_NL_EFFORT` (Anthropic), `VELOCITY_NL_CACHE`, `VELOCITY_NL_TRACES=0`, and each provider's key variable. See [AI-PROVIDERS.md](AI-PROVIDERS.md).

## `api`

`velocity api scaffold <openapi.yaml|json> [-o suite.yaml] [--base-url URL] [--confirm]` writes an API-only suite ([HTTP and API tests](DSL.md#http-and-api-tests)): no `appId`, `config.http.baseUrl` from `--base-url` or the spec's first server.

- **One test per resource** (`/pets`, `/pets/{petId}` → `pets`), creates first, reads next, changes after, deletes last. A response field is linked to a parameter of a later request by name (`id` of `POST /pets` → `{petId}`, `petId` → `petId`, `id` under `/pets/{id}`), scored, the best above a threshold kept: the producer `capture`s it and the consumer uses `${PET_ID}`. A need only another resource's create fills runs that create first as a setup step (captures belong to one test).
- **Bodies**: the media type's `example`/`examples`, else the least valid value (required fields only, an enum's first value, a value of the right `format`; `readOnly` fields left out). A non-JSON body is left for you, with a comment.
- **Secrets** are never written: each security scheme becomes a header (or query parameter) with a variable, `Authorization: Bearer ${API_TOKEN}`, `X-API-Key: ${API_KEY}`. Required parameters nothing links become variables (`${STORE_ID}`); every variable is listed, with what it is for, at the top of the file.
- **Checks** come from each operation's documented success response: `expect.status`, and for the required fields (top level, then one level down) `type`, `exists: true`, `enum` → `oneOf`, `minimum`/`maximum` → `gte`/`lte` (`gt`/`lt` when exclusive), `minLength`/`maxLength`/`minItems` → `length`, and `format` (uuid, date-time, date, email, uri) → a loose `matches`. Optional and nullable fields get no type or format check: a correct response may leave them out or null. At most `--max-checks` (default 12) per request; a comment says how many more were left out.
- **`--confirm`** sends the suite's GET and HEAD requests to the base url, never any other method, and drops each check a response disproves: those are where the server and its spec disagree, and they are printed and left as comments in the suite. `${VARS}` come from the environment (`API_TOKEN`, or `PET_ID` for an existing pet: a GET that needs a value only a POST captures is skipped otherwise). A response that isn't the documented success keeps its checks, unconfirmed.

The suite is parsed and validated before it is written; `-o` refuses an existing file without `--force`; without `-o` it goes to stdout. OpenAPI 3.0 and 3.1, YAML or JSON, local `#/…` `$ref`s only (Swagger 2.0 is refused with how to convert it).

Exit codes: 0 written; 2 an unreadable spec, an existing `-o`, or `--confirm` with no server set or reachable.

## `inspect`

`velocity inspect` serves the console and prints its URL with a per-session token (`http://localhost:9876/?token=…`).

| Flag | Meaning |
|---|---|
| `-p ios\|android\|web` | Only this platform's devices. Default: every platform's, in one console |
| `-d` | The device to start on. Default: the first booted phone or simulator |
| `--port N` | Port (default 9876) |
| `--host IP` | Address to listen on (default `127.0.0.1`). Anything else exposes the device to the network, still behind the token, and prints a warning |
| `--open` | Open the console in the default browser |
| `--url URL` | The page the web device opens when it starts |
| `--flows-dir DIR` | Where recorded flows are saved (new files only; default `flows`) |
| `--results-dir DIR` | What `/review` reads (default `velocity-results`). Console runs write to `<DIR>/console/<time>-<task>/` |
| `--app-id ID` | App for the performance chart (Android) |

**Mirror.** The page opens a WebSocket to `/api/mirror/<device>`:

| Device | Stream | Measured (2026-10-25) |
|---|---|---|
| Android | H.264 Annex-B from the on-device video server (shared with failure videos), decoded by WebCodecs `VideoDecoder` | emulator-5554: ~30 fps while the screen moves, < 2 ms from arrival to paint, a key press visible ~80–120 ms after the click |
| iOS simulator | WebDriverAgent's MJPEG server (30 fps at half scale), on loopback (see Security) | iPhone 17: ~26 fps, ~2 ms from arrival to paint |
| Web | Chrome's screencast (a JPEG per repaint) | 60 fps on an animated page |
| Anything else, or no WebCodecs | Screenshots, sent only when they change: every 80 ms after a change or an input, backing off to 1 s | — |

A new viewer gets the current picture at once (the last group of pictures from the stream's buffer, so a still screen shows up without waiting for a change). A viewer that falls behind skips to the next key frame; a hidden tab pauses its stream. Several tabs share one stream per device. Android video settings are the `VELOCITY_ANDROID_VIDEO_*` variables under [Failure evidence](#failure-evidence).

**Input and leases.** In *Interact* mode a click taps, a drag swipes, a long press long-presses, the wheel scrolls and typing types (Enter, Backspace, Tab, Escape and the arrows are keys); *Inspect* mode only selects elements. The first input takes the device's lease (the same lock `velocity run` takes) and keeps it until *Release control* or the console exits. A device another process holds answers `busy: pid=… cmd=…` and can still be watched. A console run releases the console's lease for its `velocity run`.

**Run panel and tasks.** Suites are the YAML files with a `tests:` key under the folder `velocity inspect` started in (not `node_modules`, `target`, hidden folders or `velocity-results`). A run is `velocity run <suite> -d <device> --report json,html --video on-failure --logs on-failure` in that folder, with live events (`VELOCITY_EVENTS_FILE`, one JSON line per test and step start and end: see `velocity_common::live_events`). Tasks on one device run one at a time. A finished run lists its artifacts; *Replay* opens its `trace.html`, which plays the video synced to the steps.

**Explore panel.** A goal, the app (default: `--app-id`), and either a plan file from the project (`--script`, no model) or a model (`provider:model`, default the configured one). It runs `velocity explore <goal> --app-id … -d <device> --json --inject-file <task>/inject.txt --out <task>/<goal>.yaml` in the project folder, which takes the device's lease like a run. The agent's turns (observed, planned, acted, asserted, checked, replays) stream in from the same events file; the *Inject* box appends a line to the inject file, which the agent takes in at its next turn; *Cancel* is a Ctrl-C (the session ends `stopped` and still reports). At the end the page shows the outcome, the YAML, each replay (no model calls), and the model's calls and tokens, and *Save to project* writes the YAML to a new `.yaml` path inside the project folder (`POST /api/tasks/<id>/save`; no `..`, no symlinked folder out of the project, no overwrite unless confirmed).

**Diagnostics** (Devices tab) are `velocity doctor`'s checks, run in the console's process (`velocity_doctor::run_checks`, without `--deep`): the verdict, then each check with its fix.

**Security.** Every `/api` route and WebSocket upgrade needs the token and a `Host` naming the server; requests from another origin (`Origin`, or `Sec-Fetch-Site: cross-site`/`same-site`) are refused, and there is no CORS. Artifacts are served only from the run's own folder (no `..`, no symlinks out), with a Content-Security-Policy that lets `trace.html` play its video but not call the API.

The iOS mirror reads WebDriverAgent's MJPEG stream, which has no authentication. Upstream WDA listens for it on every interface whatever `USE_IP` says, and a simulator shares the Mac's network, so the simulator's screen was visible to the LAN. Velocity patches its WDA clone before building it so the stream listens where `USE_IP` says (loopback for simulators, like WDA's HTTP server); a cached build from before the patch is rebuilt once. VelocityRunner serves no MJPEG. On a cabled iPhone WDA (and its stream) binds the phone's loopback unless `VELOCITY_IOS_DEVICE_LOOPBACK=0` (see [DRIVERS.md](DRIVERS.md)).

## Artifacts (`--artifacts-dir`, default `velocity-results/`)

| File | Contents |
|---|---|
| `junit.xml`, `results.json`, `report.html`, `allure-results/` | Reports |
| `velocity-history.json` | Durations and outcomes per test (scheduling, flaky detection, `--last-failed`) |
| `velocity-result-cache.json` | `--cache-results` |
| `traces/<test>/` | Step-by-step trace of a failed test (`steps.json`, `trees/`), its viewer `trace.html`, `video.mp4` (web: `screencast.json` + `screencast/`), `app.log`, `device.log` (Android) or `console.log` (web). A retry's files end in `-retry<N>` |
| `trace-index.html` | Written by `velocity trace view <artifacts dir>`: every kept trace |
| `diffs/<name>-expected/-actual/-diff.png` | Failed `assertScreenshot` comparisons |
| `diffs/<name>-review.json` | What `velocity baselines` needs to accept a failed comparison |
| `crashlogs/<test>/` | Crash reports copied from the device |

## Failure evidence

Each test's video and device logs are captured while it runs and written next to its trace when it fails (`on-failure`, the default), for every test (`always`), or not captured (`off`). Reports link them: `report.html` (a "Traces of failed tests" index, links and an inline video per test), JUnit (`[[ATTACHMENT|path]]` lines in `<system-out>`), `results.json` (`attachments`) and Allure (attachments).

| Platform | Video | Logs |
|---|---|---|
| Android | H.264 from Velocity's on-device video server (scrcpy-style: a virtual display into MediaCodec, streamed over `adb forward`), muxed to MP4 in Velocity. One stream per device for the whole run; each test's clip comes from an in-memory ring buffer of the last `artifacts.videoSeconds` (default 30), cut at key frames. Falls back to `adb shell screenrecord` (one recording per test in 180 s segments) when the device's encoder can't be used | `logcat -v threadtime` from the test's start (`-T` at the device's time): `device.log` (main, system, crash) and `app.log` (the app's pids plus the crash buffer). The app's crashes, native crashes and ANRs are added to the failure message |
| iOS simulator | `simctl io recordVideo --codec h264`, stopped with SIGINT at the test's end; deleted when not kept | `log stream --style compact` for the app's process (`app.log`); crash reports are copied to `crashlogs/<test>/` as before |
| iOS device | Not supported yet | Not supported yet |
| Web | The CDP screencast (JPEG frames of the last `videoSeconds`), as `screencast.json` + `screencast/*.jpg`, played by `trace.html` | Console calls, uncaught page errors, browser log entries, failed and 4xx/5xx requests (`console.log`) |

Log files and the lines embedded in `trace.html` go through the same secret masking as reports. Android video settings: `VELOCITY_ANDROID_VIDEO_SIZE` (long edge, default 1280), `VELOCITY_ANDROID_VIDEO_BITRATE` (default 2000000), `VELOCITY_ANDROID_VIDEO_FPS` (default 30); `VELOCITY_ANDROID_VIDEO=screenrecord` skips the video server and records with `adb shell screenrecord`.

`trace.html` is one self-contained file (no network, works from `file://`): the steps with their timing, each step's screenshot with a marker where it tapped and the matched element boxed, the selector and the element it resolved to (or the closest ones), heals, warnings and errors, the video seeked to the selected step, the log lines from 1 s before to 1 s after it, and a search box over steps and logs (`/` focuses it, arrow keys or `j`/`k` move between steps).

## Exit codes

A run's exit code comes from its results (`velocity-common/src/result.rs`). Any other error is mapped by its kind (`VelocityError::exit_code` in `velocity-common/src/error.rs`).

| Code | Meaning |
|---|---|
| 0 | Everything passed. Quarantined failures are forgiven only when every failure is quarantined |
| 1 | A test failed: element not found, assertion, not actionable, app crash, step or sync timeout. Also a flaky pass under `--fail-on-flaky`, or a `--hunt` flip |
| 2 | Usage or configuration: bad YAML, unknown flow, invalid selector, missing env var, bad flag, empty selection |
| 3 | Device or infrastructure: device not found or locked, boot failure, lost connection, open circuit breaker. A run where every failure was infrastructure also exits 3 |
| 4 | Suite timeout (`--timeout`) |
| 5 | Internal error: a panic (caught; never Rust's 101), or an error of no known kind |
| 129 / 130 / 143 | SIGHUP / SIGINT / SIGTERM. Reports are still written; see [CI.md](CI.md) |

A folder run returns its most severe file's code. The order is 5 > 2 > 1 > 4 > 3 > 0.
