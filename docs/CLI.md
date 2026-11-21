# CLI

`velocity <command>`. The `--color auto|always|never` flag works with every command. Logs go to stderr and are filtered by `RUST_LOG` (default `warn`, with chromiumoxide's message handler at `error`: Chrome versions it can't parse made it warn on every web run). `VELOCITY_LOG_FORMAT=json` writes one JSON object per log line instead, with the current span's fields (`test`, `device`, `attempt`), for a log pipeline. `velocity <command> --help` lists every flag.

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
| `devices up --android N --ios N` / `devices down` | Boot or clone the devices a parallel run needs, and stop them afterwards. Each device is recorded with an owner (`--owner ID`, else `$VELOCITY_OWNER`, else the CI job id, else none); `down` stops only the caller's own (`--all`: every owner's), so two CI jobs on one host never stop each other's emulators |
| `inspect [--open]` | Start the console: live device mirror with input, element tree and selectors, recording, suite runs with live steps, explore, task history, `/review` ([below](#inspect)) |
| `baselines review [--serve [--baselines-dir DIR]]` | List failed `assertScreenshot` comparisons, or serve the review page (`--baselines-dir`: its Accept writes into that `velocity-baselines` folder, for CI artifacts) |
| `baselines accept\|reject <name\|all>` | Make the actual screen the baseline, or keep the old baseline |
| `heal [--accept\|--discard\|--reject N]` | Review selectors that self-healing fixed during passing tests |
| `history merge <files…>` | Merge the history files of several CI jobs |
| `history flaky [--file FILE] [--window N] [--max-rate R] [--json]` | Each test's flake rate over its last N recorded runs; exit 1 when any test with at least 5 runs exceeds --max-rate ([below](#history-flaky)) |
| `verify-change "<description>" --app-id <id> [--after login.yaml]` | Did the change do what it says? Replays the saved test for the description with no model, or explores and saves one that passes its trust gates; writes PR evidence (`manifest.json`, `failures.md`, `comment.md`, video) to `--bundle` (default `velocity-results/verify-change`), saved tests to `--cache` (default `.velocity/verify-cache`). Exit 0 passed, 1 failed |
| `gate <file> --goal "<text>"` | Run the trust gates on a one-test suite YAML (hand-written or exported): burn-in replays, a wrong-path replay that must fail, and an assertion naming goal text ([below](#gate)). Exit 0 trusted, 1 not trusted |
| `login <suite> [--test NAME] [-d DEVICE] [--env K=V]` | Run a stored login flow once (no retry, no cache) and leave the app logged in; `${NAME}`s from the environment, `${keychain:NAME}` or `${totp:NAME}`, masked everywhere ([DSL.md](DSL.md#logging-in)) |
| `results check [--expect-shards N] <paths…>` | After a sharded run: fail (exit 1) when a shard reported nothing, ran no tests, left a damaged `results.json` / `junit.xml`, or the shards computed different partitions (their `shard_plan` fingerprints differ) ([CI.md](CI.md#no-silent-passes)) |
| `trace view [PATH] [--no-open]` | Open the trace viewer: a test's `traces/<test>/` folder (or its `steps.json`/`trace.html`), or a run's artifacts folder or `results.json` (an index of every kept trace) |
| `repl -d <device>` | Run steps one at a time (`:tree`, `:find`, `:shot`, `:save`) |
| `explore "<goal>" --app-id <id>` | An AI agent reaches the goal on a device and writes a deterministic YAML test with assertions, then replays it with no model calls ([EXPLORE.md](EXPLORE.md), flags below) |
| `mcp [--device/-d DEV] [--platform/-p ios\|android\|web] [--config/-c SUITE] [--profile core\|full] [--read-only] [--notify …] [--tasks-dir DIR]` | Start the MCP server on stdio ([MCP.md](MCP.md)). `--profile core` (default) serves five tools, `full` all of them |
| `mcp install [all\|claude\|cursor\|codex\|windsurf\|vscode\|cline\|gemini] [--dry-run] [--uninstall]` | Register the MCP server with agent clients and install the Velocity agent rules |
| `mcp config <client>` / `mcp rules` | Print a client's config snippet / the agent rules |
| `migrate maestro <dir> [--dry-run]` / `migrate waits <suite>` | Convert Maestro flows (`--dry-run`: write nothing, rank the commands that don't convert by the flows they affect); remove redundant `wait:` steps |
| `nl-eval` | Measure planner consistency (pass^k) on saved natural-language traces, or compare models on the built-in task set (`--model provider:model …`, see below) |
| `signing check\|devices\|register-device\|profile` | iOS signing with an App Store Connect API key |
| `daemon start\|stop\|status` | Keep drivers warm between runs (`run --daemon`), and the browser of the web agent commands. One daemon per socket: `start` holds `daemon.sock.lock` for the daemon's life and fails (naming the pid) while another daemon has it, even one busy with a run. A client whose daemon goes away mid-run exits 3 |
| `snapshot`, `tap`, `type`, `press`, `swipe`, `scroll`, `launch`, `stop`, `assert`, `wait`, `screenshot`, `export`, `session` | Drive an Android or iOS device one shell command at a time, by ref, and save the session as a test ([below](#agent-commands)) |
| `skills [list\|get <name>\|stub]` | Print the agent guides built into this binary (`velocity skills get cli`) |
| `version` | Print the version |

## `history flaky`

`velocity history flaky [--file FILE] [--window N] [--max-rate R] [--json]` prints each test's flake rate over its last `N` recorded runs (default 20), sorted worst first:

| Flag | Meaning |
|---|---|
| `--file FILE` | History file, or a folder holding `velocity-history.json` (default `./velocity-results/velocity-history.json`, where `velocity run` writes it) |
| `--window N` | Recent runs per test (default 20) |
| `--max-rate R` | Fail when any test with at least 5 runs exceeds this rate (default 0.05) |
| `--json` | Print one JSON object: `{window, maxRate, tests:[{name, runs, flaky, rate}], over:[names]}` |

A test's `runs` is the window's run count, `flaky` the runs that passed only after a retry, and `rate = flaky / runs`. Infrastructure failures are not in the history, so they count in neither. The table marks each test above `--max-rate` with `*`.

Exit 0 when no test with at least 5 runs exceeds `--max-rate`; exit 1 when at least one does (the names are listed under `warning:`); exit 2 when the history file is missing or unreadable. Tests with fewer than 5 runs get a rate but never trip the threshold.

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
| `--ci github` | Write the CI workflow itself instead of an example: `.github/workflows/velocity.yml` (the Velocity Action, two shards and a `results check` job) |

It writes `<dir>/<app>.yaml` (`com.android.settings` → `settings.yaml`), `<dir>/ci/github-actions.example.yml` (sharded, JUnit + GitHub reports; copy it to `.github/workflows/`) and adds `velocity-results/` and the local heal files to `.gitignore`. The test launches the app and asserts one or two elements of its first screen: an id (`id:`, layout wrappers last) first, then a short text that matches exactly one element and carries no digits, times or prices. Commented `tap` / `inputText` examples with `${TEST_EMAIL}` placeholders follow. Every file is emitted by serde_yaml, and the suite is parsed and validated before it is written. While it reads the screen it holds the device's lease (waiting up to a minute for a run that has it).

Exit codes: 0 (or the `--run` result), 2 bad flag or a file that exists, 3 no running device.

## `doctor`

Checks, by area, each with a deadline (`--timeout DURATION`, default 3000 ms per command; see [durations](#durations)), areas in parallel; a run takes about a second.

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
| `PATH`, `-c, --config FILE` | The suite file (or folder) to run; the two are the same. Default `velocity.yaml` |
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
| `--impact-map FILE` | With `--changed-since`: the map from tests and flows to source patterns (default `velocity-impact.yaml` next to the suite) |
| `--fail-if-none` | With `--changed-since`: exit 2 when no test is affected |
| `--shard M/N` | 1-based shard. `--shard-index/--shard-total` is the 0-based form |
| `--shard-timings FILE` | Balance shards by duration, from a history JSON or JUnit XML. Give every shard the same file |
| `--history-from PATH\|GLOB` | Merge history from other jobs before anything reads it. See [CI.md](CI.md) |

### Execution

| Flag | Meaning |
|---|---|
| `--retries N` | Retry failures (at most 10). A pass on retry counts as flaky |
| `--fail-on-flaky` | A flaky pass exits 1 |
| `--timeout DURATION` | Suite timeout (`30m`, `90s`; a bare number is milliseconds, see [durations](#durations)). Exceeding it ends the whole run and exits 4 |
| `--test-timeout DURATION` | Per-test timeout: an attempt that runs longer fails (`test timed out after …`) and may be retried; the rest of the run goes on. A failure, so exit 1. Off by default |
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
| `--flaky-confidence P` | With `--parallel`: run copies of the tests history flags as flaky, enough that at least one passes with probability `P` (`0.99`). A test passes if any copy does, and is then reported as flaky |
| `--shuffle [--seed S]` | Shuffle the test order. `--jitter MS` adds random delays |
| `--hunt N [--stress]` | Run N perturbed rounds and report outcome flips (exits 1 if any). For each flaky test it says where passing and failing rounds diverge (the failing step, the timings before it, order and stress) and ranks the likely causes |
| `--suggest` | With `--hunt`: the YAML change for each flaky test's top cause (a `timeoutMs`, an `assertVisible` before the step, `isolated`/`dependsOn`), re-checked by hunting again with it applied in memory. The suite is never edited |

### Output

| Flag | Meaning |
|---|---|
| `--report junit,json,html,github,allure` | Reports to write (default `junit`). `github` is added automatically in GitHub Actions |
| `--junit-classname test\|suite` | JUnit `classname`: `test` (default) is `velocity` for every test; `suite` is the suite file's stem, so CI groups tests by suite. Changing it resets CI test history once |
| `--clean-allure` | With `--report allure`: remove the results already in `allure-results/` first. By default they are kept, so reruns and shards add up into one report |
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
| `-p, --platform ios\|android\|web`, `-d, --device ID` | Default: the platform from the device id; the first booted device no other Velocity process holds. A held device is waited for (`--lease-wait DURATION`, default 600 s; a bare number is seconds) |
| `--model PROVIDER:MODEL[@URL]` | The goal model (default: the configured provider's goal model, `VELOCITY_NL_EXPLORE_MODEL`) |
| `--script PLAN.json` | Play a fixed plan instead of a model (no key needed) |
| `-o, --out FILE`, `--force` | Where the test goes (default `<goal-as-file-name>.yaml`); an existing file is only overwritten with `--force` |
| `--name NAME` | The test's name (default: the goal) |
| `--data NAME=VALUE` | A value the agent may type; the test types `${NAME}` (repeatable) |
| `--secret NAME` | A secret from `$NAME` the agent may type; never sent to a model, shown or written; the test types `${NAME}` (repeatable) |
| `--max-steps N` | Planning turns (default 40) |
| `--timeout DURATION` | Exploration time (`10m`, `1h`; a bare number is seconds; default 600 s; exit 4 when hit) |
| `--max-tokens N`, `--max-cost USD`, `--price IN,OUT` | Token and spend budgets; `--max-cost` needs `--price` (dollars per million input,output tokens) |
| `--verify off\|final\|checkpoints\|strict` | AI check of the goal (default `final`): success needs the checker to cite real elements |
| `--replay N` | Replays of the written test, no model calls, the later ones with timing jitter (default 5; 0 skips) |
| `--no-gates` | Skip the trust gates: the wrong-path replay and the goal-text rule ([EXPLORE.md](EXPLORE.md#trust-gates)) |
| `--no-repair` | No repair round when a replay fails |
| `--no-demos` | Don't show the model the project's tests most like the goal (on by default; never with `--benchmark`) |
| `--demos-from DIR` | Where those tests are looked for (repeatable; default the folder `--out` writes to, else the current one) |
| `--clear-state` / `--no-launch` | Start clean (`launchApp: { clearState: true }`), or from the screen as it is (no launch step) |
| `--keep-nl` | Also write `<out>.nl.yaml`: the same steps in plain English |
| `--dry-run` | Explore and report only; no test written or replayed |
| `--json` | The result as JSON on stdout (live lines go to stderr) |
| `--inject-file PATH` | Lines appended to this file while the agent runs are given to it as instructions |
| `--benchmark` | For an outside harness (AndroidWorld): start from the screen as it is, no test written or replayed, no auto-dismissed dialogs, `launch`/`assert` off, screenshots every turn ([EXPLORE.md](EXPLORE.md#benchmark-mode---benchmark)) |
| `--after SUITE` (`--after-test NAME`) | Replay that suite's first test (a login) with no model before exploring; the model never sees its values, and the written test starts with the same steps by name ([DSL.md](DSL.md#logging-in)) |
| `--values-from-goal` | The model may propose text to type, copied from the goal; accepted only when grounded there ([EXPLORE.md](EXPLORE.md#--values-from-goal-and---values-from-screen-off-by-default)) |
| `--values-from-screen` | The same for text shown on a screen in this run (copying between apps). Screen text is untrusted, so this is its own opt-in |
| `--values-from-screenshots` | The model may type text it read from a screenshot sent in this run (an image, a receipt). Code can't check pixels, so the model's reading is trusted: a separate, wider opt-in. Needs a provider that takes images |
| `--values-from-model` | The model may type text it writes itself (a reply, a message body). Nothing grounds it and it goes into the test as is: the widest opt-in. Use only when composing the text is the task |
| `--open-app NAME=ID`, `--apps-file PATH` | Apps the agent may open by name (`openApp`); the file is a JSON object `{"Name": "app.id"}` |
| `--screenshots never\|poor\|always` | When a screenshot goes with the screen list (default `poor`; `always` with `--benchmark`) |

Exit codes: 0 when the goal was reached, the test written and every replay passed; 1 otherwise (gave up, stuck, unverified, out of budget, a replay still failing after the repair); 2 configuration; 3 device; 4 `--timeout`.

## `gate`

`velocity gate <file> --goal "<text>" [flags]`. Gates a one-test suite YAML, hand-written or exported from an agent session (`velocity export`, MCP `export_session_flow`), before it is trusted. Nothing is explored and no model is called: it replays the test N times (the later ones with timing jitter), replays it once on the wrong path (a neighbouring tap instead of the last navigational one, which must fail), and checks that an assertion names text from the goal.

| Flag | Meaning |
|---|---|
| `<file>` | The suite YAML to gate: exactly one test |
| `--goal <TEXT>` | What the test is meant to show (required); an assertion must name text from it |
| `-p, --platform ios\|android\|web`, `-d, --device ID` | Default: the platform from the device id, and the first booted device no other Velocity process holds |
| `--lease-wait DURATION` | How long to wait for a device another Velocity process holds (default 600 s; a bare number is seconds) |
| `--replay N` | Burn-in replays (default 5; 0 is a config error) |
| `--jitter-ms N` | Upper bound of the random pause before each step, on replays after the first (default 150 ms) |
| `--env NAME=VALUE` | A value for a `${NAME}` (repeatable) |
| `--env-file PATH` | KEY=VALUE lines for the `${NAME}`s (repeatable) |
| `--json` | One JSON object on stdout: `{file, goal, trusted, gates, replay}` |

Exit codes: 0 trusted, 1 not trusted, 2 configuration/usage, 3 device.

## AI environment

`VELOCITY_NL_PROVIDER`, `VELOCITY_NL_MODEL` (may be `provider:model`), `VELOCITY_NL_EXPLORE_MODEL`, `VELOCITY_NL_API_URL`, `VELOCITY_NL_API_KEY`, `VELOCITY_NL_IMAGES=0`, `VELOCITY_NL_OUTPUT=json_schema|tools|text`, `VELOCITY_NL_EFFORT` (Anthropic), `VELOCITY_NL_CACHE`, `VELOCITY_NL_TRACES=0`, and each provider's key variable. See [AI-PROVIDERS.md](AI-PROVIDERS.md).

## Proxies and certificates

Outbound HTTPS (AI providers, `http` steps, the App Store Connect API, MCP task webhooks) trusts Mozilla's bundled roots **and** the operating system's trust store: the macOS keychain, Windows' store, or the system bundle on Linux. When `SSL_CERT_FILE` (a PEM bundle) or `SSL_CERT_DIR` is set, those files are read **instead of** the operating system's store (Mozilla's roots stay). So a corporate proxy's or mitmproxy's CA works once the machine trusts it, or once the variable names a bundle that holds it. Velocity uses rustls; it never links OpenSSL.

`HTTPS_PROXY`, `HTTP_PROXY` and `ALL_PROXY` (or their lowercase forms) send those requests through a proxy, and `NO_PROXY` lists hosts that skip it. Traffic to the iOS runner (WebDriverAgent or VelocityRunner, on loopback, a usbmuxd forward or the local network) never goes through a proxy, whatever these say. Android's tree server is a direct socket through `adb forward` and is not HTTP. An `http` step to `localhost` does follow the proxy variables, like curl; add `localhost,127.0.0.1` to `NO_PROXY` when a proxy is set.

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
| `--tests-dir DIR` | Where recorded tests are saved (new files only; default `flows`). `--flows-dir` still works |
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

## Agent commands

For coding agents that run shell commands, and for people at a terminal. Each command runs one MCP tool in process (`McpServer::call`), so refs, leases, masking and the recording are the MCP server's ([MCP.md](MCP.md#snapshots-and-refs)).

```bash
velocity snapshot                         # one line per element, each with a ref (@e7)
velocity tap @e7                          # waits for the screen to settle, prints what changed
velocity type @e3 "qa@example.com"        # or --secret NAME, or --stdin
velocity assert "Welcome"                 # --not, --timeout 5s
velocity export --out velocity/flow.yaml  # the session as a test, with assertions
```

- **Targets:** a ref (`@e7`), `key=value` for one selector key as the DSL writes it (`id=login`, `textContains=net`), a JSON selector (`'{"role":"button","name":"Pay"}'`), else exact text.
- **After an action** (`tap`, `type`, `press`, `swipe`, `scroll`, `launch`, `stop`): the command waits for the screen to settle (the device's signal on Android, else two equal reads 100 ms apart, at most 3 s), then prints a delta snapshot. `--no-settle` skips both.
- **Snapshot flags:** `--delta`, `--within @eN` (only what lies inside that element), `--find TEXT`, `--limit N` (the header then says `incomplete`), `--bounds`.
- **Sessions:** the device, the refs and the recording are saved under `~/.velocity/agent/sessions/` (`VELOCITY_AGENT_DIR`), keyed by `--session NAME` (or `VELOCITY_SESSION`), else by the git worktree (else the directory). The first command picks `--device`, else the first booted Android device, then iOS; later commands keep it. Two commands of one session take turns on a file lock. `velocity session` shows it, `session reset` starts a new one, `session list` lists them.
- **Several agents on one device:** each command holds the device's lease from the action to the delta that reports it, so another agent's command waits (up to 10 s, `VELOCITY_LEASE_WAIT_MS`, then `DEVICE_BUSY`, `retry: safe`, naming the process that holds it) instead of acting in between. Sessions keep their own refs: after another agent navigates, a ref from before fails `STALE_REF` rather than naming what now sits in its place.
- **Output:** text by default. `--json` prints one object: `{"v":1,"ok":true,"command","session","device","data","settle","screen"}`, or `{"v":1,"ok":false,…,"error":{"code","message","retry","delivered","next"}}` ([MCP.md](MCP.md#failures)).
- **Exit status:** 0 ok; 3 when the device is the problem (a `DEVICE_*` code such as `DEVICE_BUSY`, `DEVICE_NOT_FOUND` or `DEVICE_LOST`) or the session is busy (`SESSION_BUSY`: another command of the same session held it past `VELOCITY_LEASE_WAIT_MS`), so CI retries it like any infrastructure failure; 1 any other failure the reply describes; 2 a malformed command line.
- **Platforms:** Android, iOS and the web.
  - **iOS:** the commands leave WebDriverAgent running when they exit (`VELOCITY_KEEP_WDA=1` unless you set it yourself), so the next command reattaches instead of relaunching it.
  - **Web** (`-p web`, device `web`, or a session that drives the web): Chrome dies with the process that started it, so the command is sent to `velocity daemon`, which keeps one browser for the agent commands (apart from the drivers `run --daemon` pools). If no daemon answers, the command starts one in the background (its own process group, output in `~/.velocity/daemon.log`) and stops early with the log's last line if it exits at start. `velocity daemon stop` closes the browser. `type --stdin` is read by the client and forwarded. The daemon serves one request at a time: web sessions share one page, and a `run --daemon` in progress makes agent commands wait. `launch URL` opens a page.
- **Measured** (debug build, 2026-11-07), each command in a fresh process:
  - Android 14 emulator: `snapshot` 0.26 s, `tap` with settle and delta 0.56–0.67 s.
  - iOS 26.0.1 simulator (iPhone 17, WebDriverAgent): first `launch` 17 s with a cached runner build; then `snapshot` 0.6 s (3.7 s while each command relaunched WebDriverAgent), `press back` with settle and delta 2.2 s.
  - Chrome, a local page: the first command 2.8 s (daemon and Chrome start), then 0.03–0.36 s a command.
  - An exported session replayed with `velocity run`: Android passed; iOS Settings and the web page passed 3 of 3.

## Artifacts (`--artifacts-dir`, default `velocity-results/`)

| File | Contents |
|---|---|
| `junit.xml`, `results.json`, `report.html`, `allure-results/` | Reports. Allure attachments are hard links to the evidence when on the same disk |
| `run-metrics.json` | With `--report json`: per device, the tests run, busy time, utilisation, retries and infrastructure failures; for the run, wall time, retries used, cached and quarantined tests. Derived from the results (lease waits and installs are in the debug log) |
| `failures.md` | Every run: each failed test with its step and `file:line`, the error, the end of its app log (error lines first, with line numbers) and the files that show it, and a one-line *why* that cites one of them or says "insufficient evidence" |
| `velocity-history.json` | Durations and outcomes per test (scheduling, flaky detection, `--last-failed`) |
| `velocity-result-cache.json` | `--cache-results` |
| `traces/<test>/` | Step-by-step trace of a failed test (`steps.json`, `trees/`), its viewer `trace.html`, `video.mp4` (web: `screencast.json` + `screencast/`), `app.log`, `device.log` (Android) or `console.log` (web). A retry's files end in `-retry<N>` |
| `trace-index.html` | Written by `velocity trace view <artifacts dir>`: every kept trace |
| `diffs/<name>-expected/-actual/-diff.png` | Failed `assertScreenshot` comparisons |
| `diffs/<name>-review.json` | What `velocity baselines` needs to accept a failed comparison |
| `crashlogs/<test>/` | Crash reports copied from the device |

Evidence is never deleted unless you ask: `VELOCITY_ARTIFACTS_MAX_BYTES` (`500M`, `2G`) removes the oldest earlier runs' `traces/<test>/` folders and `screenshots/` files after a run until the folder is under the cap (this run's evidence always stays). A run starts with a warning when the disk holding the artifacts has less free space than `devices up` needs (3 GB, `VELOCITY_MIN_FREE_DISK_GB`).

`results.json` has a top-level `format_version` (now 2). It goes up only when a field changes meaning or is removed; new fields are added without a bump, so readers should ignore fields they don't know. A file without `format_version` is version 1. Version 2 adds an integer `duration_ms` beside every `duration` (the run, each test, each step) and writes the resource keys in snake_case like the rest of the file (`resource_peak.total_pss_kb`, was `totalPssKb`). The `{"secs", "nanos"}` `duration` object is still written for version-1 readers but is deprecated: read `duration_ms`. `velocity results check` and `trace view` read both versions. A sharded run also records `shard_plan` (`fingerprint`, this shard's planned `tests`, `planned_total`), which `velocity results check` compares across shards.

In `junit.xml`, a test that failed because the device, the driver or Velocity failed is an `<error type="InfrastructureError">` or `<error type="InternalError">` and is counted in `errors`; `<failure type="AssertionError">` and `failures` are for the test's own checks. `timestamp` is when the run started.

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

## Durations

`--timeout` (on `run`, `explore`, `doctor` and the agent commands) and `explore --lease-wait` take a duration with a unit: `500ms`, `30s`, `10m`, `1h` (`1.5s` works; `2m30s` doesn't). A bare number keeps the unit the flag always had, so existing scripts keep working: milliseconds on `run`, `doctor` and the agent commands, seconds on `explore`. A bare number that looks like the other unit (under 1000 on a milliseconds flag, a day or more on a seconds flag) prints a warning naming the suffixed form. Write the unit in new scripts.

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

### Exit codes by command

| Command | 0 | 1 | 2 | 3 | Other |
|---|---|---|---|---|---|
| `run`, `verify-change` | passed | a test failed | usage or configuration | device or infrastructure | 4 suite timeout, 5 internal, 129/130/143 signals |
| `explore` | goal reached, test written, every replay passed | gave up, stuck, unverified, out of budget, replay failing | configuration | device | 4 `--timeout` |
| `gate` | trusted: every replay passed, the wrong path failed, an assertion names the goal | not trusted (the reasons are printed) | usage or configuration (no such file, not one test, `--replay 0`) | device | 5 internal |
| `doctor` | ready, or degraded without `--strict` | | a project check failed (or, with `--strict`, project warnings) | blocked by the environment (or, with `--strict`, degraded) | |
| agent commands (`snapshot`, `tap`, …) | ok | a failure the reply describes | malformed command line | `DEVICE_*`, `SESSION_BUSY` | |
| `validate`, `schema` | valid | | a parse or validation error | | |
| `lint` | no error findings (warnings allowed) | an error finding | the suite doesn't parse | | |
| `results check` | every shard reported and agrees | a shard missing, empty, damaged, or a partition disagreement | | | |
| `init` | written (or the `--run` result) | | bad flag, or a file that exists | no running device | |
