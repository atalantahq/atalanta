# CLI

`atalanta <command> --help` lists every flag of a command.

Behaviour that applies to every command:

- `--color auto|always|never` sets colour ([CI.md](CI.md#colour) gives the full rule).
- Logs go to stderr and are filtered by `RUST_LOG`. The default is `warn`, with chromiumoxide's message handler at `error`, because Chrome versions it cannot parse made it warn on every web run.
- `ATALANTA_LOG_FORMAT=json` writes one JSON object per log line instead, with the current span's fields (`test`, `device`, `attempt`), for a log pipeline.

## Commands

| Command | What it does |
|---|---|
| `init` | Create a starter project from a running app: a first test read from its launch screen, a CI example, `.gitignore` entries ([below](#init)) |
| `doctor [PATH]` | Check the machine and project: tools, devices, runner caches, suites ([below](#doctor)) |
| `run [PATH]` | Run a suite file, or every `*.yaml` in a folder (default `-c atalanta.yaml`). A folder run skips `atalanta-impact.yaml` and `atalanta-quarantine.yaml` |
| `validate <file\|folder>` | Parse and validate suites without running them |
| `api scaffold <spec> [-o FILE] [--base-url URL] [--confirm]` | Write an API test suite from an OpenAPI 3 spec ([below](#api)) |
| `lint [PATH]` | Find fragile selectors and fixed waits |
| `schema [-o FILE] [--markdown]` | Print the JSON Schema for suite YAML, or the [DSL reference](DSL-REFERENCE.md) |
| `settings [filter]` | Show named settings, their defaults and current values |
| `shard-plan [PATH] --total N` | Show each shard's tests and the plan fingerprint (no devices needed) |
| `device list\|boot\|shutdown\|screenshot` | Manage simulators and emulators (alias `devices`) |
| `devices up --android N --ios N` / `devices down` | Boot or clone the devices a parallel run needs, and stop them afterwards. Each device records an owner: `--owner ID`, else `$ATALANTA_OWNER`, else the CI job id, else none. `down` stops only the caller's own devices (`--all`: every owner's), so two CI jobs on one host never stop each other's emulators |
| `inspect [--open]` | Start the console: live device mirror with input, element tree and selectors, recording, suite runs with live steps, explore, task history, `/review` ([below](#inspect)) |
| `baselines review [--serve [--baselines-dir DIR]]` | List failed `assertScreenshot` comparisons, or serve the review page (`--baselines-dir`: its Accept writes into that `atalanta-baselines` folder, for CI artifacts) |
| `baselines accept\|reject <name\|all>` | Make the actual screen the baseline, or keep the old baseline |
| `heal [--accept\|--discard\|--reject N]` | Review selectors that self-healing fixed during passing tests |
| `history merge <files…>` | Merge the history files of several CI jobs |
| `history flaky [--file FILE] [--window N] [--max-rate R] [--json]` | Each test's flake rate over its last N recorded runs; exit 1 when any test with at least 5 runs exceeds `--max-rate` ([below](#history-flaky)) |
| `verify-change "<description>" --app-id <id> [--after login.yaml]` | Check that a change does what its description says. It replays the saved test for the description with no model, or explores and saves a new one that passes its [trust gates](GUIDE.md#glossary). PR evidence (`manifest.json`, `failures.md`, `comment.md`, video) goes to `--bundle` (default `atalanta-results/verify-change`) and saved tests to `--cache` (default `.atalanta/verify-cache`). Exit 0 passed, 1 failed |
| `gate <file> --goal "<text>"` | Run the trust gates on a one-test suite YAML (hand-written or exported): burn-in replays, a wrong-path replay that must fail, and an assertion naming goal text ([below](#gate)). Exit 0 trusted, 1 not trusted |
| `login <suite> [--test NAME] [-d DEVICE] [--env K=V]` | Run a stored login flow once (no retry, no cache) and leave the app logged in. Values come from the environment (`${NAME}`), the keychain (`${keychain:NAME}`) or a TOTP secret (`${totp:NAME}`), and are masked everywhere ([DSL.md](DSL.md#logging-in)) |
| `results check [--expect-shards N] <paths…>` | After a sharded run: fail (exit 1) when a shard reported nothing, ran no tests, left a damaged `results.json` / `junit.xml`, or the shards computed different partitions (their `shard_plan` fingerprints differ) ([CI.md](CI.md#no-silent-passes)) |
| `trace view [PATH] [--no-open]` | Open the trace viewer: a test's `traces/<test>/` folder (or its `steps.json`/`trace.html`), or a run's artifacts folder or `results.json` (an index of every kept trace) |
| `repl -d <device>` | Run steps one at a time (`:tree`, `:find`, `:shot`, `:save`) |
| `explore "<goal>" --app-id <id>` | An AI agent reaches the goal on a device and writes a deterministic YAML test with assertions, then replays it with no model calls ([EXPLORE.md](EXPLORE.md), flags below) |
| `mcp [--device/-d DEV] [--platform/-p ios\|android\|web] [--config/-c SUITE] [--profile core\|full] [--read-only] [--notify …] [--tasks-dir DIR]` | Start the MCP server on stdio ([MCP.md](MCP.md)). `--profile core` (default) serves five tools, `full` all of them |
| `mcp install [all\|claude\|cursor\|codex\|windsurf\|vscode\|cline\|gemini] [--dry-run] [--uninstall]` | Register the MCP server with agent clients and install the Atalanta agent rules |
| `mcp config <client>` / `mcp rules` | Print a client's config snippet / the agent rules |
| `migrate maestro <dir> [--dry-run]` / `migrate waits <suite>` | Convert Maestro flows (`--dry-run`: write nothing, rank the commands that don't convert by the flows they affect); remove redundant `wait:` steps |
| `nl-eval` | Measure planner consistency (pass^k) on saved natural-language traces, or compare models on the built-in task set (`--model provider:model …`, see below) |
| `signing check\|devices\|register-device\|profile` | iOS signing with an App Store Connect API key |
| `daemon start\|stop\|status` | Keep drivers warm between runs (`run --daemon`), and keep the browser of the web agent commands. There is one daemon per socket: `start` holds `daemon.sock.lock` for the daemon's life, and fails (naming the pid) while another daemon holds it, even one busy with a run. A client whose daemon goes away mid-run exits 3 |
| `snapshot`, `tap`, `type`, `press`, `swipe`, `scroll`, `launch`, `stop`, `assert`, `wait`, `screenshot`, `export`, `session` | Drive an Android or iOS device one shell command at a time, by ref, and save the session as a test ([below](#agent-commands)) |
| `skills [list\|get <name>\|stub]` | Print the agent guides built into this binary (`atalanta skills get cli`) |
| `version` | Print the version |

## `history flaky`

Gates a pipeline on flaky tests without a device ([CI.md](CI.md#rolling-flake-rate)).

`atalanta history flaky [--file FILE] [--window N] [--max-rate R] [--json]` prints each test's flake rate over its last `N` recorded runs (default 20), sorted worst first:

| Flag | Meaning |
|---|---|
| `--file FILE` | History file, or a folder holding `atalanta-history.json` (default `./atalanta-results/atalanta-history.json`, where `atalanta run` writes it) |
| `--window N` | Recent runs per test (default 20) |
| `--max-rate R` | Fail when any test with at least 5 runs exceeds this rate (default 0.05) |
| `--json` | Print one JSON object: `{window, maxRate, tests:[{name, runs, flaky, flips, rate}], over:[names]}` |

In the table and the JSON:

- `runs` is the window's run count.
- `flaky` is the runs that passed only after a retry.
- `flips` is the pass/fail changes between consecutive runs in the window.
- `rate` is the measure `atalanta run` reports: `(flaky + flips) / (runs + runs - 1)`.

Infrastructure failures are not in the history, so they count in none of these. The table marks each test above `--max-rate` with `*`.

Exit codes:

- 0: no test with at least 5 runs exceeds `--max-rate`.
- 1: at least one does. The names are listed under `warning:`.
- 2: the history file is missing or unreadable, `--window` is under 5, or `--max-rate` is outside 0..1.

Tests with fewer than 5 runs get a rate but never trip the threshold.

## `init`

`atalanta init` takes you from a running device to a first passing test. Run it once per project. It is interactive on a terminal, and every answer is also a flag.

| Flag | Meaning |
|---|---|
| `-p ios\|android\|web` | Platform. Default: the one with a running device (asked when both) |
| `--app-id ID` | Package / bundle id. Default: the app in front (Android `dumpsys activity`; iOS the one running user app), else asked from `pm list packages -3` / `simctl listapps` |
| `--url URL` | Web: the start page |
| `-d ID` | Device. Default: the only running one, or asked (iOS: simulators; pass a real iPhone's UDID) |
| `--dir PATH` | Suite folder (default `atalanta/`) |
| `-y, --yes` | Ask nothing; take the defaults (CI) |
| `--force` | Overwrite existing files |
| `--run` | Run the new suite at once and print the time from `init` to green |
| `--impact` | Also write an `atalanta-impact.yaml` stub (nothing mapped yet, so every test still runs) |
| `--ci github` | Write the CI workflow itself instead of an example: `.github/workflows/atalanta.yml` (the Atalanta Action, two shards and a `results check` job) |

It writes:

- `<dir>/<app>.yaml` (`com.android.settings` becomes `settings.yaml`).
- `<dir>/ci/github-actions.example.yml`, a sharded workflow with JUnit and GitHub reports. Copy it to `.github/workflows/`.
- Entries in `.gitignore` for `atalanta-results/` and the local heal files.

The test launches the app and asserts one or two elements of its first screen. It prefers an id (`id:`, layout wrappers last), then a short text that matches exactly one element and carries no digits, times or prices. Commented `tap` and `inputText` examples with `${TEST_EMAIL}` placeholders follow. The suite is parsed and validated before it is written. While `init` reads the screen it holds the device's lease, waiting up to a minute for a run that has it.

Exit codes: 0 (or the `--run` result), 2 bad flag or a file that exists, 3 no running device.

## `doctor`

`atalanta doctor [PATH]` checks that the machine and the project are ready to run tests. Run it first when a device or a toolchain misbehaves. It checks each area in parallel, and a run takes about a second. Each command has a deadline (`--timeout DURATION`, default 3000 ms; see [durations](#durations)).

| Area | Checks |
|---|---|
| Host | OS and arch, Atalanta version, free memory and disk against the `devices up` bar (35% / 3 GB, 3 GB), state / lease / artifact dirs writable, lease files left by exited processes |
| Android | adb (PATH, `ATALANTA_ADB_PATH`, or found only in the SDK), server version and response, devices (online / unauthorized / offline), API level, tree server push + handshake (`--deep`), emulator and AVDs; Java is not needed |
| iOS (macOS) | Xcode and `xcode-select -p`, license, simctl, booted simulators, the WebDriverAgent / AtalantaRunner build cache and whether the pinned tag fits this Xcode, devicectl and paired iPhones, `ATALANTA_IOS_TEAM_ID` when one is connected |
| Web | Chrome / Chromium (`CHROME`, PATH, /Applications) |
| AI | Which provider keys are set (names only), the model, the API host's reachability (`--deep`) |
| Project | `atalanta.yaml` or `atalanta/` (or the PATH given): suites parse and validate, their app is installed on a running device, `atalanta-impact.yaml` reads and names real tests |

Marks: `✓` ok, `!` warning, `✗` failure, `-` skipped. A fix prints as `Run: <command>` or `Guidance: <text>`. A missing toolchain fails only for a platform you named with `-p` or that a suite uses. Otherwise the check is skipped.

| Flag | Meaning |
|---|---|
| `--json` | The report as JSON: `verdict`, `checks[]` (`id`, `area`, `status`, `summary`, `detail`, `fix: {kind: run\|guidance, value, safe_auto}`) |
| `--deep` | Also push the tree server to a device and PING it (it is only started when no other process holds the device), and connect to the AI API host |
| `--fix` | Apply the safe fixes, then check again: restart a wedged adb server, `adb reconnect offline`, remove dead-pid lease files, re-push the tree server. Nothing interactive or needing sudo |
| `--strict` | A degraded verdict (warnings) exits non-zero too |
| `-p ios,android,web` | Platforms you test on |

Exit codes:

- 0: the verdict is ready or degraded. With `--strict`, degraded exits 3, or 2 when the warnings are project warnings.
- 2: the verdict is blocked and a project check failed.
- 3: the verdict is blocked for any other reason.

## `run` flags

`atalanta run [PATH]` runs a suite file, or every suite in a folder. Common cases: `--parallel` and `-d` to choose devices, `--shard` to split a run across CI jobs, `--retries` to absorb flakiness, and `--report` to choose output.

### Target

| Flag | Meaning |
|---|---|
| `PATH`, `-c, --config FILE` | The suite file (or folder) to run; the two are the same. Default `atalanta.yaml` |
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
| `--changed-since REF` | Only tests the change can affect, plus risky and unmapped ones (`atalanta-impact.yaml`) |
| `--impact-map FILE` | With `--changed-since`: the map from tests and flows to source patterns (default `atalanta-impact.yaml` next to the suite) |
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
| `--quarantine FILE` | Known failures to forgive (default `atalanta-quarantine.yaml`) |
| `--cache-results` | Skip tests that passed when nothing has changed |
| `--update-baselines[=missing]` | Rewrite missing and changed `assertScreenshot` baselines, or only the missing ones |
| `--verify off\|final\|checkpoints\|strict` | AI verification for every test, over `verify:` in the suite and tests ([DSL.md](DSL.md#ai-verification-verify)). Anything but `off` needs a model key; without one the run exits 2 |
| `--daemon` | Run through a running `atalanta daemon` |

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
| `--junit-classname test\|suite` | JUnit `classname`: `test` (default) is `atalanta` for every test; `suite` is the suite file's stem, so CI groups tests by suite. Changing it resets CI test history once |
| `--clean-allure` | With `--report allure`: remove the results already in `allure-results/` first. By default they are kept, so reruns and shards add up into one report |
| `--artifacts-dir DIR` | Where everything goes (default `./atalanta-results`) |
| `--trace off\|on-failure\|always` | Per-test traces (default `on-failure`), with a `trace.html` viewer next to each |
| `--video off\|on-failure\|always` | Per-test screen video (default `on-failure`, or `artifacts.video`). See [Failure evidence](#failure-evidence) |
| `--logs off\|on-failure\|always` | Per-test device logs (default `on-failure`, or `artifacts.logs`) |
| `--pause-on-failure` | Stop at the first failing step, print where the evidence is, and wait: Enter continues, `r` retries the step, `q` aborts the run (exit 130). Interactive terminals only; ignored with a warning otherwise and with `--daemon` |
| `--profile FILE`, `--profile-steps` | A Chrome/Perfetto trace of the run; where time went per action |

## `nl-eval`

`atalanta nl-eval` measures how consistently the natural-language planner works, using pass^k (the chance that all k attempts at a task pass).

| Flag | Meaning |
|---|---|
| `-k N`, `-n N` | pass^k, and re-plans per task (default k) |
| `--traces DIR` | Saved traces to re-plan (default `.atalanta/nl-traces`) |
| `--model PROVIDER:MODEL[@URL]` | Compare models (repeatable). Any provider in [AI-PROVIDERS.md](AI-PROVIDERS.md); the key comes from that provider's variable |
| `--tasks builtin\|DIR` | The task set: `builtin` (fixed screens and expected steps, the default with `--model`) or a trace directory |
| `--price PROVIDER:MODEL=IN,OUT` | Dollars per million input/output tokens, for the cost column (repeatable) |
| `--recommend` | Name the default: best pass^k, then the cheapest within 2 points of it (by cost, else tokens, else latency) |
| `--json FILE` | The full report (per model: pass@1, pass^k, consistency, errors, p50/p95 latency, tokens, cost) |
| `--min-pass-k F`, `--max-error-rate F` | Exit 1 below the pass^k floor; exit 3 when calls mostly failed |

## `explore`

`atalanta explore "<goal>" --app-id <id> [flags]` lets an AI agent reach a goal on a device and writes a deterministic YAML test for it. It needs a model key unless you pass `--script`. How it works, and what the test it writes contains: [EXPLORE.md](EXPLORE.md).

| Flag | Meaning |
|---|---|
| `--app-id ID` | The app under test (required) |
| `-p, --platform ios\|android\|web`, `-d, --device ID` | Default: the platform from the device id; the first booted device no other Atalanta process holds. A held device is waited for (`--lease-wait DURATION`, default 600 s; a bare number is seconds) |
| `--model PROVIDER:MODEL[@URL]` | The goal model (default: the configured provider's goal model, `ATALANTA_NL_EXPLORE_MODEL`) |
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

Exit codes:

- 0: the goal was reached, the test was written and every replay passed.
- 1: anything else about the goal: the agent gave up, got stuck, could not verify, ran out of budget, or a replay still failed after the repair round.
- 2: configuration.
- 3: device.
- 4: `--timeout` was hit.

## `gate`

`atalanta gate <file> --goal "<text>" [flags]` runs the [trust gates](GUIDE.md#glossary) on a one-test suite YAML, hand-written or exported from an agent session (`atalanta export`, MCP `export_session_flow`), before you rely on it. Nothing is explored and no model is called. It does three things:

1. Replays the test N times, the later ones with timing jitter.
2. Replays it once on the wrong path: a neighbouring tap instead of the last navigational one. This replay must fail.
3. Checks that an assertion names text from the goal.

| Flag | Meaning |
|---|---|
| `<file>` | The suite YAML to gate: exactly one test |
| `--goal <TEXT>` | What the test is meant to show (required); an assertion must name text from it |
| `-p, --platform ios\|android\|web`, `-d, --device ID` | Default: the platform from the device id, and the first booted device no other Atalanta process holds |
| `--lease-wait DURATION` | How long to wait for a device another Atalanta process holds (default 600 s; a bare number is seconds) |
| `--replay N` | Burn-in replays (default 5; 0 is a config error) |
| `--jitter-ms N` | Upper bound of the random pause before each step, on replays after the first (default 150 ms) |
| `--env NAME=VALUE` | A value for a `${NAME}` (repeatable) |
| `--env-file PATH` | KEY=VALUE lines for the `${NAME}`s (repeatable) |
| `--json` | One JSON object on stdout: `{file, goal, trusted, gates, replay}` |

Exit codes: 0 trusted, 1 not trusted, 2 configuration or usage, 3 device.

## AI environment

The model-backed commands (`explore`, `nl-eval`, `--verify`, natural-language steps) read these variables. [AI-PROVIDERS.md](AI-PROVIDERS.md) says what each does.

- `ATALANTA_NL_PROVIDER`
- `ATALANTA_NL_MODEL` (may be `provider:model`)
- `ATALANTA_NL_EXPLORE_MODEL`
- `ATALANTA_NL_API_URL`
- `ATALANTA_NL_API_KEY`
- `ATALANTA_NL_IMAGES=0`
- `ATALANTA_NL_OUTPUT=json_schema|tools|text`
- `ATALANTA_NL_EFFORT` (Anthropic)
- `ATALANTA_NL_CACHE`
- `ATALANTA_NL_TRACES=0`
- Each provider's own key variable.

## Proxies and certificates

Outbound HTTPS covers AI providers, `http` steps, the App Store Connect API and MCP task webhooks. It trusts Mozilla's bundled roots **and** the operating system's trust store: the macOS keychain, Windows' store, or the system bundle on Linux. When `SSL_CERT_FILE` (a PEM bundle) or `SSL_CERT_DIR` is set, those files are read **instead of** the operating system's store. Mozilla's roots stay in both cases. A corporate proxy's or mitmproxy's CA therefore works once the machine trusts it, or once the variable names a bundle that holds it. Atalanta uses rustls and never links OpenSSL.

`HTTPS_PROXY`, `HTTP_PROXY` and `ALL_PROXY` (or their lowercase forms) send those requests through a proxy. `NO_PROXY` lists hosts that skip it.

- Traffic to the iOS runner (WebDriverAgent or AtalantaRunner, on loopback, a usbmuxd forward or the local network) never goes through a proxy, whatever these variables say.
- Android's tree server is a direct socket through `adb forward` and is not HTTP.
- An `http` step to `localhost` does follow the proxy variables, like curl. Add `localhost,127.0.0.1` to `NO_PROXY` when a proxy is set.

## `api`

`atalanta api scaffold <openapi.yaml|json> [-o suite.yaml] [--base-url URL] [--confirm]` writes a starting API test suite from an OpenAPI spec, so you do not write each request by hand. The suite is API-only ([HTTP and API tests](DSL.md#http-and-api-tests)): it has no `appId`, and `config.http.baseUrl` comes from `--base-url` or the spec's first server. Review the output before you rely on it.

- **One test per resource** (`/pets` and `/pets/{petId}` become `pets`). Steps run in order: creates, reads, changes, deletes.
- **Linked values.** A response field is linked to a parameter of a later request by name (`id` of `POST /pets` to `{petId}`, `petId` to `petId`, `id` under `/pets/{id}`). Each candidate is scored and the best one above a threshold is kept. The producer `capture`s the value and the consumer uses `${PET_ID}`. When a parameter can only be filled by another resource's create, that create runs first as a setup step, because captures belong to one test.
- **Bodies**: the media type's `example`/`examples`, else the least valid value (required fields only, an enum's first value, a value of the right `format`; `readOnly` fields left out). A non-JSON body is left for you, with a comment.
- **Secrets** are never written: each security scheme becomes a header (or query parameter) with a variable, `Authorization: Bearer ${API_TOKEN}`, `X-API-Key: ${API_KEY}`. Required parameters nothing links become variables (`${STORE_ID}`); every variable is listed, with what it is for, at the top of the file.
- **Checks** come from each operation's documented success response: `expect.status`, and for the required fields (top level, then one level down) `type`, `exists: true`, `enum` → `oneOf`, `minimum`/`maximum` → `gte`/`lte` (`gt`/`lt` when exclusive), `minLength`/`maxLength`/`minItems` → `length`, and `format` (uuid, date-time, date, email, uri) → a loose `matches`. Optional and nullable fields get no type or format check: a correct response may leave them out or null. At most `--max-checks` (default 12) per request; a comment says how many more were left out.
- **`--confirm`** sends the suite's GET and HEAD requests to the base url, never any other method, and drops each check a response disproves: those are where the server and its spec disagree, and they are printed and left as comments in the suite. `${VARS}` come from the environment (`API_TOKEN`, or `PET_ID` for an existing pet: a GET that needs a value only a POST captures is skipped otherwise). A response that isn't the documented success keeps its checks, unconfirmed.

The suite is parsed and validated before it is written. `-o` refuses an existing file without `--force`. Without `-o` the suite goes to stdout.

Supported input is OpenAPI 3.0 and 3.1, in YAML or JSON, with local `#/…` `$ref`s only. Swagger 2.0 is refused, with instructions to convert it.

Exit codes: 0 written; 2 an unreadable spec, an existing `-o`, or `--confirm` with no server set or reachable.

## `inspect`

`atalanta inspect` starts the console, a local web page for working with devices: a live mirror with input, the element tree, recording, suite runs and explore. It prints its URL with a per-session token (`http://localhost:9876/?token=…`).

| Flag | Meaning |
|---|---|
| `-p ios\|android\|web` | Only this platform's devices. Default: every platform's, in one console |
| `-d` | The device to start on. Default: the first booted phone or simulator |
| `--port N` | Port (default 9876) |
| `--host IP` | Address to listen on (default `127.0.0.1`). Anything else exposes the device to the network, still behind the token, and prints a warning |
| `--open` | Open the console in the default browser |
| `--url URL` | The page the web device opens when it starts |
| `--tests-dir DIR` | Where recorded tests are saved (new files only; default `flows`). `--flows-dir` still works |
| `--results-dir DIR` | What `/review` reads (default `atalanta-results`). Console runs write to `<DIR>/console/<time>-<task>/` |
| `--app-id ID` | App for the performance chart (Android) |

**Mirror.** The page opens a WebSocket to `/api/mirror/<device>`:

| Device | Stream | Measured (2026-10-25) |
|---|---|---|
| Android | H.264 Annex-B from the on-device video server (shared with failure videos), decoded by WebCodecs `VideoDecoder` | Android emulator: ~30 fps while the screen moves, < 2 ms from arrival to paint, a key press visible ~80–120 ms after the click |
| iOS simulator | WebDriverAgent's MJPEG server (30 fps at half scale), on loopback (see Security) | iPhone 17: ~26 fps, ~2 ms from arrival to paint |
| Web | Chrome's screencast (a JPEG per repaint) | 60 fps on an animated page |
| Anything else, or no WebCodecs | Screenshots, sent only when they change: every 80 ms after a change or an input, backing off to 1 s | — |

A new viewer gets the current picture at once, from the last group of pictures in the stream's buffer, so a still screen shows up without waiting for a change. A viewer that falls behind skips to the next key frame. A hidden tab pauses its stream. Several tabs share one stream per device. Android video settings are the `ATALANTA_ANDROID_VIDEO_*` variables under [Failure evidence](#failure-evidence).

**Input and leases.** In *Interact* mode a click taps, a drag swipes, a long press long-presses, the wheel scrolls and typing types. Enter, Backspace, Tab, Escape and the arrows are sent as keys. *Inspect* mode only selects elements.

The first input takes the device's lease, the same lock `atalanta run` takes, and keeps it until *Release control* or the console exits. A device another process holds answers `busy: pid=… cmd=…` and can still be watched. When the console starts a run, it releases its lease for that `atalanta run`.

**Run panel and tasks.** The panel lists the YAML files with a `tests:` key under the folder `atalanta inspect` started in. It skips `node_modules`, `target`, hidden folders and `atalanta-results`. A run is `atalanta run <suite> -d <device> --report json,html --video on-failure --logs on-failure` in that folder. Live events come through `ATALANTA_EVENTS_FILE`, one JSON line per test and step start and end (see `atalanta_common::live_events`). Tasks on one device run one at a time. A finished run lists its artifacts, and *Replay* opens its `trace.html`, which plays the video synced to the steps.

**Explore panel.** The panel takes a goal, the app (default: `--app-id`), and either a plan file from the project (`--script`, no model) or a model (`provider:model`, default the configured one). It runs `atalanta explore <goal> --app-id … -d <device> --json --inject-file <task>/inject.txt --out <task>/<goal>.yaml` in the project folder, and takes the device's lease like a run.

- The agent's turns (observed, planned, acted, asserted, checked, replays) stream in from the same events file.
- The *Inject* box appends a line to the inject file, which the agent takes in at its next turn.
- *Cancel* is a Ctrl-C. The session ends `stopped` and still reports.
- At the end the page shows the outcome, the YAML, each replay (no model calls), and the model's calls and tokens.
- *Save to project* writes the YAML to a new `.yaml` path inside the project folder (`POST /api/tasks/<id>/save`). It refuses `..` and a symlinked folder out of the project, and does not overwrite unless confirmed.

**Diagnostics** (Devices tab) show `atalanta doctor`'s checks without `--deep`, run in the console's process: the verdict, then each check with its fix.

**Security.** Every `/api` route and WebSocket upgrade needs the token and a `Host` naming the server. Requests from another origin (`Origin`, or `Sec-Fetch-Site: cross-site` or `same-site`) are refused, and there is no CORS. Artifacts are served only from the run's own folder, with no `..` and no symlinks out. A Content-Security-Policy lets `trace.html` play its video but not call the API.

The iOS mirror reads WebDriverAgent's MJPEG stream, which has no authentication. Upstream WDA listens for it on every interface whatever `USE_IP` says, and a simulator shares the Mac's network, so an unpatched build exposes the simulator's screen to the LAN. Atalanta patches its WDA clone before building it so the stream listens where `USE_IP` says: loopback for simulators, like WDA's HTTP server. A cached build from before the patch is rebuilt once. AtalantaRunner serves no MJPEG. On a cabled iPhone, WDA and its stream bind the phone's loopback unless `ATALANTA_IOS_DEVICE_LOOPBACK=0` (see [DRIVERS.md](DRIVERS.md)).

## Agent commands

These commands drive a device one step at a time from the shell. They are for coding agents that run shell commands, and for people at a terminal. Each command runs one MCP tool in process, so refs, leases, masking and the recording behave as in the MCP server ([MCP.md](MCP.md#snapshots-and-refs)).

```bash
atalanta snapshot                         # one line per element, each with a ref (@e7)
atalanta tap @e7                          # waits for the screen to settle, prints what changed
atalanta type @e3 "qa@example.com"        # or --secret NAME, or --stdin
atalanta assert "Welcome"                 # --not (must be absent), --timeout 5s (wait for it)
atalanta export --out atalanta/flow.yaml  # the session as a test, with assertions
```

- **Targets:** a ref (`@e7`), `key=value` for one selector key as the DSL writes it (`id=login`, `textContains=net`), a JSON selector (`'{"role":"button","name":"Pay"}'`), else exact text.
- **After an action** (`tap`, `type`, `press`, `swipe`, `scroll`, `launch`, `stop`): the command waits for the screen to settle, then prints a delta snapshot. Settling uses the device's signal on Android, else two equal reads 100 ms apart, for at most 3 s. `--no-settle` skips both the wait and the delta.
- **Snapshot flags:** `--delta`, `--within @eN` (only what lies inside that element), `--find TEXT`, `--limit N` (the header then says `incomplete`), `--bounds`.
- **Sessions:** the device, the refs and the recording are saved under `~/.atalanta/agent/sessions/` (`ATALANTA_AGENT_DIR`). A session is keyed by `--session NAME` (or `ATALANTA_SESSION`), else by the git worktree, else by the directory. The first command picks `--device`, else the first booted Android device, then iOS. Later commands keep that choice. Two commands of one session take turns on a file lock. `atalanta session` shows the session, `session reset` starts a new one and `session list` lists them.
- **Several agents on one device:** each command holds the device's lease from the action to the delta that reports it, so another agent's command waits instead of acting in between. It waits up to 10 s (`ATALANTA_LEASE_WAIT_MS`), then fails with `DEVICE_BUSY` and `retry: safe`, naming the process that holds the lease. Sessions keep their own refs. After another agent navigates, a ref from before fails with `STALE_REF` rather than naming what now sits in its place.
- **Output:** text by default. `--json` prints one object: `{"v":1,"ok":true,"command","session","device","data","settle","screen"}`, or `{"v":1,"ok":false,…,"error":{"code","message","retry","delivered","next"}}` ([MCP.md](MCP.md#failures)).
- **Exit status:**
  - 0: ok.
  - 3: the device is the problem (a `DEVICE_*` code such as `DEVICE_BUSY`, `DEVICE_NOT_FOUND` or `DEVICE_LOST`), or the session is busy (`SESSION_BUSY`: another command of the same session held it past `ATALANTA_LEASE_WAIT_MS`). CI retries these like any infrastructure failure.
  - 1: any other failure the reply describes.
  - 2: a malformed command line.
- **Platforms:** Android, iOS and the web.
  - **iOS:** the commands leave WebDriverAgent running when they exit (`ATALANTA_KEEP_WDA=1` unless you set it yourself), so the next command reattaches instead of relaunching it.
  - **Web** (`-p web`, device `web`, or a session that drives the web): Chrome dies with the process that started it, so the command is sent to `atalanta daemon`, which keeps one browser for the agent commands (apart from the drivers `run --daemon` pools). If no daemon answers, the command starts one in the background (its own process group, output in `~/.atalanta/daemon.log`) and stops early with the log's last line if it exits at start. `atalanta daemon stop` closes the browser. `type --stdin` is read by the client and forwarded. The daemon serves one request at a time: web sessions share one page, and a `run --daemon` in progress makes agent commands wait. `launch URL` opens a page.
- **Measured** (debug build, 2026-11-07), each command in a fresh process. These are single measurements on one machine, not a benchmark:
  - Android 14 emulator: `snapshot` 0.26 s, `tap` with settle and delta 0.56–0.67 s.
  - iOS 26.0.1 simulator (iPhone 17, WebDriverAgent): first `launch` 17 s with a cached runner build; then `snapshot` 0.6 s (3.7 s while each command relaunched WebDriverAgent), `press back` with settle and delta 2.2 s.
  - Chrome, a local page: the first command 2.8 s (daemon and Chrome start), then 0.03–0.36 s a command.
  - An exported session replayed with `atalanta run`: Android passed; iOS Settings and the web page passed 3 of 3.

## Artifacts (`--artifacts-dir`, default `atalanta-results/`)

Everything a run writes goes under one folder. Upload it from CI with `if: always()` so a failed run keeps its evidence.

| File | Contents |
|---|---|
| `junit.xml`, `results.json`, `report.html`, `allure-results/` | Reports. Allure attachments are hard links to the evidence when on the same disk |
| `run-metrics.json` | With `--report json`: per device, the tests run, busy time, utilisation, retries and infrastructure failures; for the run, wall time, retries used, cached and quarantined tests. Derived from the results (lease waits and installs are in the debug log) |
| `failures.md` | Every run: each failed test with its step and `file:line`, the error, the end of its app log (error lines first, with line numbers) and the files that show it, and a one-line *why* that cites one of them or says "insufficient evidence" |
| `atalanta-history.json` | Durations and outcomes per test (scheduling, flaky detection, `--last-failed`) |
| `atalanta-result-cache.json` | `--cache-results` |
| `traces/<test>/` | Step-by-step trace of a failed test (`steps.json`, `trees/`), its viewer `trace.html`, `video.mp4` (web: `screencast.json` + `screencast/`), `app.log`, `device.log` (Android) or `console.log` (web). A retry's files end in `-retry<N>` |
| `trace-index.html` | Written by `atalanta trace view <artifacts dir>`: every kept trace |
| `diffs/<name>-expected/-actual/-diff.png` | Failed `assertScreenshot` comparisons |
| `diffs/<name>-review.json` | What `atalanta baselines` needs to accept a failed comparison |
| `crashlogs/<test>/` | Crash reports copied from the device |

Evidence is never deleted unless you ask: `ATALANTA_ARTIFACTS_MAX_BYTES` (`500M`, `2G`) removes the oldest earlier runs' `traces/<test>/` folders and `screenshots/` files after a run until the folder is under the cap (this run's evidence always stays). A run starts with a warning when the disk holding the artifacts has less free space than `devices up` needs (3 GB, `ATALANTA_MIN_FREE_DISK_GB`).

### `results.json` format

`results.json` has a top-level `format_version`, currently 2. It goes up only when a field changes meaning or is removed. New fields are added without a bump, so readers should ignore fields they do not know. A file without `format_version` is version 1.

Version 2 differs from version 1 in two ways:

- An integer `duration_ms` sits beside every `duration` (the run, each test, each step). The `{"secs", "nanos"}` `duration` object is still written for version-1 readers but is deprecated: read `duration_ms`.
- Resource keys are snake_case like the rest of the file (`resource_peak.total_pss_kb`, was `totalPssKb`).

 `atalanta results check` and `trace view` read both versions. A sharded run also records `shard_plan` (`fingerprint`, this shard's planned `tests`, `planned_total`), which `atalanta results check` compares across shards.

### `junit.xml` failures and errors

A test that failed because the device, the driver or Atalanta failed is an `<error type="InfrastructureError">` or `<error type="InternalError">`, and is counted in `errors`. `<failure type="AssertionError">` and `failures` are for the test's own checks. `timestamp` is when the run started.

## Failure evidence

Each test's video and device logs are captured while it runs. With `on-failure` (the default) they are written next to its trace only when the test fails. With `always` they are written for every test, and with `off` they are not captured. Reports link them: `report.html` (a "Traces of failed tests" index, links and an inline video per test), JUnit (`[[ATTACHMENT|path]]` lines in `<system-out>`), `results.json` (`attachments`) and Allure (attachments).

| Platform | Video | Logs |
|---|---|---|
| Android | H.264 from Atalanta's on-device video server (scrcpy-style: a virtual display into MediaCodec, streamed over `adb forward`), muxed to MP4 in Atalanta. One stream per device for the whole run; each test's clip comes from an in-memory ring buffer of the last `artifacts.videoSeconds` (default 30), cut at key frames. Falls back to `adb shell screenrecord` (one recording per test in 180 s segments) when the device's encoder can't be used | `logcat -v threadtime` from the test's start (`-T` at the device's time): `device.log` (main, system, crash) and `app.log` (the app's pids plus the crash buffer). The app's crashes, native crashes and ANRs are added to the failure message |
| iOS simulator | `simctl io recordVideo --codec h264`, stopped with SIGINT at the test's end; deleted when not kept | `log stream --style compact` for the app's process (`app.log`); crash reports are copied to `crashlogs/<test>/` |
| iOS device | Not supported | Not supported |
| Web | The CDP screencast (JPEG frames of the last `videoSeconds`), as `screencast.json` + `screencast/*.jpg`, played by `trace.html` | Console calls, uncaught page errors, browser log entries, failed and 4xx/5xx requests (`console.log`) |

Log files and the lines embedded in `trace.html` go through the same secret masking as reports. Android video settings: `ATALANTA_ANDROID_VIDEO_SIZE` (long edge, default 1280), `ATALANTA_ANDROID_VIDEO_BITRATE` (default 2000000), `ATALANTA_ANDROID_VIDEO_FPS` (default 30); `ATALANTA_ANDROID_VIDEO=screenrecord` skips the video server and records with `adb shell screenrecord`.

`trace.html` is one self-contained file (no network, works from `file://`): the steps with their timing, each step's screenshot with a marker where it tapped and the matched element boxed, the selector and the element it resolved to (or the closest ones), heals, warnings and errors, the video seeked to the selected step, the log lines from 1 s before to 1 s after it, and a search box over steps and logs (`/` focuses it, arrow keys or `j`/`k` move between steps).

## Durations

Flags that take a time accept a unit, and older scripts that pass a bare number keep working. `--timeout` (on `run`, `explore`, `doctor` and the agent commands) and `explore --lease-wait` take a duration with a unit: `500ms`, `30s`, `10m`, `1h` (`1.5s` works; `2m30s` doesn't). A bare number keeps the unit the flag always had, so existing scripts keep working: milliseconds on `run`, `doctor` and the agent commands, seconds on `explore`. A bare number that looks like the other unit (under 1000 on a milliseconds flag, a day or more on a seconds flag) prints a warning naming the suffixed form. Write the unit in new scripts.

## Exit codes

A pipeline reads these to decide whether to fail, retry or page someone. [CI.md](CI.md#exit-codes-in-a-pipeline) says how to use them. A run's exit code comes from its results (`atalanta-common/src/result.rs`). Any other error is mapped by its kind (`AtalantaError::exit_code` in `atalanta-common/src/error.rs`).

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
