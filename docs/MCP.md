# MCP server

`velocity mcp` runs a Model Context Protocol server. AI agents use it to drive a device, an emulator or a browser, to turn what they did into a suite, to run suites in the background and to debug failures. The code is in `crates/velocity-mcp`.

```bash
velocity mcp [-d <device>] [-p ios|android|web] [-c suite.yaml] [--profile core|full] [--read-only]
             [--notify webhook=<url>|script=<path>|jsonl=<path>|desktop]… [--tasks-dir DIR]
```

- **Transport:** newline-delimited JSON-RPC 2.0 over stdio. stdout carries only the protocol; logs go to stderr.
- **Methods:** `initialize`, `server/discover`, `tools/list`, `tools/call`, `ping` and `notifications/*`.
- **Protocol versions:** 2024-11-05, 2025-03-26, 2025-06-18 and 2025-11-25. The stateless 2026-07-28 revision is also accepted; its version comes from `_meta` (`io.modelcontextprotocol/protocolVersion`). Any other version gets error -32022.
- **Device:** without `-d`, the server picks the first booted device.
- **Device leases:** each tool call that drives the device holds the device's cross-process lease (`~/.velocity/leases`) for the call, so the server and a `velocity run` never drive one device at once. A device leased elsewhere is waited for up to 5 s, then the tool says who holds it. While a `run_task` runs on the device, the other device tools answer that it is busy.
- **Suite:** `-c` loads a suite, which the flow and test tools need, and which gives `launch_app` and the exports their default app id.
- **Profiles:** by default the server lists five tools, the **core** profile: `observe`, `act`, `run`, `artifacts` and `verify_change` ([Core tools](#core-tools)). The first four each stand for several full tools and run exactly what that tool would. `--profile full` lists the full tools instead (with `verify_change`). Every listed tool's definition is in the agent's context on every turn: the core profile's are about 3.4 KB (about 850 tokens), the full profile's about 20 KB (about 5,000). CI prints both (`core_profile_schema_budget`) and fails if the core profile passes 3.6 KB or 5 tools. Under the core profile a full tool's name gets an error naming the core call that does it.
- **Read-only:** `--read-only` lists only the tools that don't change the device. Those are the ones marked read-only below; in the core profile, `observe` and `artifacts`.
- **Tool names:** each tool carries MCP annotations (`readOnlyHint`, `destructiveHint`, `idempotentHint`: true for the read-only tools and for `open_inspector`, `close_inspector`, `stop_app` and `task_stop`). Common other names are accepted as aliases, for example `click` for `tap`, `type` for `type_text`, `open_app` for `launch_app`, `doctor` for `diagnose`, and `export_session_test` for `export_session_flow` (which exports a test, not a `flows:` entry).
- **Deadlines and dead sessions:** every device call has a deadline, 55 s by default (`VELOCITY_MCP_CALL_TIMEOUT_MS`). A call past it is cut off, the driver session is rebuilt, and the agent is told the call's effect is unknown, so it observes before repeating it. A call that finds the session or the connection gone (a lost WebDriverAgent session, a dropped adb connection, a closed browser) rebuilds the session too. A read-only call is then retried once; an action is not, because it may already have reached the device.
- **Shutdown:** when the client closes stdin, running tasks are stopped (Ctrl-C to the run, which writes its reports) and waited for, up to 40 s.

## Installing into agent clients

`velocity mcp install [all|claude|cursor|codex|windsurf|vscode|cline|gemini]` registers the server (`<this velocity binary> mcp`) in each client's global MCP config and installs the agent rules (`crates/velocity-mcp/assets/velocity-agent-rules.md`: the workflow, a YAML cheat sheet, "never put secrets in YAML, use `${ENV}`"). `all` (the default) means every client found on the machine. Other servers and settings in those files are kept; a file is copied to `<file>.bak` before it changes; a second run changes nothing. `--dry-run` prints what would change; `--uninstall` removes both. `velocity mcp config <client>` prints the snippet to add by hand, `velocity mcp rules` the rules.

| Client | MCP config | Rules |
|---|---|---|
| Claude Code | `claude mcp add --scope user velocity -- <velocity> mcp` when the `claude` CLI is on PATH, else `~/.claude.json` `mcpServers.velocity` (`type: stdio`) | skill `~/.claude/skills/velocity/SKILL.md` |
| Cursor | `~/.cursor/mcp.json` `mcpServers.velocity` | none: Cursor keeps global User Rules in its settings, so the command says to paste `velocity mcp rules` there (or into a project's `.cursor/rules/velocity.mdc`) |
| Codex | `~/.codex/config.toml` `[mcp_servers.velocity]` (edited with `toml_edit`, comments kept) | skill `~/.agents/skills/velocity/SKILL.md` (Codex's user skills folder) |
| Windsurf | `~/.codeium/windsurf/mcp_config.json` `mcpServers.velocity` | a marked block in `~/.codeium/windsurf/memories/global_rules.md` (replaced on reinstall, removed on uninstall; the rest of the file is kept) |
| VS Code | user `mcp.json` `servers.velocity` (`type: stdio`): `~/Library/Application Support/Code/User/` (macOS), `~/.config/Code/User/` (Linux), `%APPDATA%\Code\User\` (Windows) | none global: guidance to use a project's `.github/copilot-instructions.md` |
| Cline | `<VS Code user dir>/globalStorage/saoudrizwan.claude-dev/settings/cline_mcp_settings.json` `mcpServers.velocity`, and `~/.cline/data/settings/cline_mcp_settings.json` when the Cline CLI is installed | `~/Documents/Cline/Rules/velocity.md` |
| Gemini CLI | `~/.gemini/settings.json` `mcpServers.velocity` | skill `~/.gemini/skills/velocity/SKILL.md` |

A config file that isn't plain JSON (a `mcp.json` with comments) is left untouched, with the entry to add printed instead.

## Snapshots and refs

`snapshot` is the cheap way to read the screen: one line per element that says something or can be acted on (text, label, hint, checked state or an actionable role; bare layout containers are left out), with a ref:

```text
# 1080x2340, 18 elements
@e2 TextView "Settings" #homepage_title
@e3 button
@e5 TextView "Network & internet" #title
@e7 switch "Wi-Fi" #switch_widget [checked]
```

- A ref stays the same across snapshots while the element does: same kind and id (or, without an id, the same text and label), and the same place among identical twins.
- `mode: "delta"` returns only what changed since the last snapshot: `+` added, `-` removed, `~` changed (text, state). Unchanged lines are left out; an unchanged screen is one line.
- An id that repeats on screen (every list row's `#title`) doesn't name one element, so such elements are also identified by their text and label: after a navigation the old row's ref is gone (`STALE_REF`), never moved onto whatever row now sits in its place. An element with a unique id keeps its ref while its text changes (typing into a field).
- `within: "@e12"` prints only the elements inside that one (a dialog, a list), `find` only lines containing the text (any case), and `limit` at most that many lines (the header then says `incomplete` and how many more match; JSON adds `shown` and `complete`). They narrow what is printed, never what is remembered: the next delta and every ref still see the whole screen.
- Tools that take a `selector` also take `ref: "@e12"` (or `selector: "@e12"`). The server turns the ref into a selector built from the live tree in code, and checks it finds exactly that element. A ref that is not on the last snapshot, whose element is gone, or (for a twin) has moved is an error asking for a new snapshot. An exported flow records the selector, never the ref.
- `list_elements` still hands out its own refs (`e3`, without `@`), which expire at the next `list_elements` and after any tool that changes the screen.

Token cost on the Android Settings home screen (emulator-5554, API 34; characters / 4):

| Read | Characters | ~Tokens |
|---|---:|---:|
| `list_elements` (JSON) | 20,492 | 5,123 |
| `list_elements` (`format: text`) | 3,239 | 810 |
| `get_screen_summary` | 752 | 188 |
| `snapshot` (JSON) | 2,140 | 535 |
| **`snapshot` (text)** | **811** | **203** |
| `snapshot` delta, nothing changed | 59 | 15 |
| `snapshot` delta after tapping into Network & internet (a new screen) | 1,003 | 251 |

A text snapshot is 25x smaller than `list_elements` JSON and 4x smaller than its text form; a delta after an action that changed little costs a few lines.

## Failures

A failed tool result has `isError: true`, the message as text, and `structuredContent.error` for clients and the [agent commands](CLI.md#agent-commands):

```json
{ "code": "STALE_REF", "message": "ref @e9 is stale: …", "retry": "no", "delivered": "no", "next": "take a new snapshot and use a current ref" }
```

- **`retry`**: `safe` (nothing happened that repeating could double: a read, or an action refused before it reached the device), `observe_first` (the effect is unknown: look before repeating), `no` (it fails the same way again; follow `next`). A read-only tool's `observe_first` is reported as `safe`.
- **`delivered`**: `no` (refused before it reached the device, or only read), `unknown`, or `yes` (it changed the screen before failing: a `scroll` that never found its target).
- A failed step keeps the kind of its error: a `scroll` or `tap` whose element never appears is `NOT_FOUND`, a crash during a step is `APP_CRASHED` (until 2026-11-07 both read `INVALID_ARGS`).
- **Codes** (`crates/velocity-mcp/src/agent_error.rs`; new ones are added, none renamed): `STALE_REF`, `NOT_FOUND`, `NOT_ACTIONABLE`, `ASSERTION_FAILED`, `APP_CRASHED`, `INVALID_ARGS`, `DEVICE_NOT_FOUND`, `DEVICE_LOCKED`, `DEVICE_BUSY`, `DEVICE_LOST`, `DEVICE_NOT_READY`, `DEVICE_UNRESPONSIVE`, `DEVICE_ERROR`, `TIMEOUT`, `COMMAND_FAILED`, `HTTP_UNREACHABLE`, `UNSUPPORTED`, `READ_ONLY`, `NOT_IN_PROFILE`, `UNKNOWN_TOOL`, `INTERNAL`.

## Selectors

Tools that take a `selector` accept the same map as a suite step, for example `{"text": "Log in"}`, `{"id": "submit"}` or `{"role": "button", "name": "Pay"}`, checked by the same rules: a state filter alone (`{"enabled": true}`) is refused, as in a suite. No other selector form is accepted.

## Secrets

`type_text` takes `secret` instead of `text`. Its value is the name of an environment variable.

- **Allowed variables:** only names listed in `VELOCITY_SECRETS`, or names a suite in this process already used as a secret.
- **Masking:** the value is typed and masked in all output. It is never returned.
- **Export:** `export_session_flow` writes the step as `${NAME}`, and never uses typed text as an assertion. Literal typed text is escaped (`$${…}`), so it reads no variable when the flow runs.
- **Explore tasks:** `run_task`'s `secret` names follow the same allow-list.

## Background tasks

`run_task` starts a child process and answers at once with a `task_id`, its folder and log. Tasks live under `--tasks-dir` (default `./velocity-results/tasks`, or `~/.velocity/mcp-tasks` when the server runs in `/` or `$HOME`). Both kinds write live events (`VELOCITY_EVENTS_FILE`, `velocity_common::live_events`) to `<task>/events.jsonl`, which `task_status` reads.

- **A suite run** (`suite`, a path, or inline `yaml`): an ordinary `velocity run`. It takes the device lease, its output goes to `<task>/run.log` (never to the protocol), and its reports and traces (`--trace always`, a screenshot per step for failing tests) go to `<task>/artifacts/`. A `suite` path must be inside the working directory or the tasks folder.
- **A suite the agent wrote** (`yaml`), outside `--profile full`: it may not have `exec` steps; its `${…}` may name only what its `http` steps capture and, in typed text only (`inputText`, `inputRaw`, a natural step's `data`), secrets the user allowed (`VELOCITY_SECRETS`, also as `keychain:` or `totp:`); `config.ai` may set only `model`, `exploreModel` and `images`; and `mockRequest` may not serve a `file`. Its child gets only the environment a run needs (system, toolchain, proxy, `VELOCITY_*` and AI-provider variables, and the allowed secrets). A suite saved as a file the user reviews and passed as `suite` runs as written. See [SECURITY.md §9](SECURITY.md#9-agents-the-rule-of-two).
- **A trust gate** (`gate: true`, with `goal` and `suite` or `yaml` holding one test): `velocity gate <file> --goal=<goal> --json` ([CLI.md](CLI.md#gate)). The test is replayed (`replay`, 1 to 20, default 5; the later replays with timing jitter), then replayed once on the wrong path, which must fail, and an assertion must name the goal. It is how an agent checks a test it exported before committing it. The suite gets the same checks as a suite run's (an agent-written suite may not run `exec` steps outside `--profile full`, and so on). `summary.gates` holds `trusted`, the gates' report with its `reasons`, the file and the goal; the counts are the replays. The task ends `passed` when the test is trusted and `failed` when it is not, with the reasons as its error. On the core profile it is `run {yaml, goal, gate: true}`.
- **An exploration** (`goal` with `app_id`): `velocity explore <goal> --app-id … --json --inject-file <task>/inject.txt --out <out>` ([EXPLORE.md](EXPLORE.md)). An agent reaches the goal on the device, the session is written as a deterministic YAML test (default `<task>/explore.yaml`), and the test is replayed with no model calls. `script` plays a fixed plan instead of a model (no key needed); `model`, `data` (`{NAME: value}`), `secret` (names read from the server's environment, never sent to a model or written), `max_steps`, `verify` (`off|final|checkpoints|strict`) and `replay` pass through. Its live lines go to `run.log`, its JSON result to `<task>/result.json`.

Then:

- `task_status` shows the state (`queued`, `running`, `passed`, `failed`, `stopped`, `error`; a task that crashed ends as `error`), the progress and, when done, a summary. Progress is the test, step i/n and what it does; for an exploration also its `phase` (`exploring`, `explored`, `replaying`, `repairing`), `turn`, and the last `op` and `target`. The summary is `{total, passed, failed, flaky, skipped, failures: [{test, step, error, screenshot, trace}]}`; for an exploration the counts are its replays, and `summary.explore` has the `outcome`, `reason`, the `yaml` path written, `replay` (`runs`, `passed`, `ms`, `model_calls`), `repair`, `turns`, `actions`, `model_calls`, `input_tokens`, `output_tokens`, `cost_usd` and a `next` hint. `wait_ms` waits for the next change instead of polling. Each task also writes `<task>/task.json`.
- `task_stop` sends the child Ctrl-C. A run stops its test, writes its reports and ends as `stopped`; an exploration ends its session at its next turn and still prints its result. Either is killed after 30 s.
- `task_inject` appends an instruction to a running exploration's inject file, one line, which the agent takes in at its next planning turn as an operator's instruction (text to plan from, never a selector or a value). A suite run says it takes none.
- `--notify` (repeatable) tells someone when a task ends, with the task JSON: `webhook=<url>` (POST), `script=<path>` (run with the `task.json` path as its argument and `VELOCITY_TASK_ID`, `VELOCITY_TASK_STATE` set; its output goes to stderr), `jsonl=<path>` (one line per task) or `desktop` (`osascript` on macOS, `notify-send` on Linux; text is passed as arguments, never spliced into a script). Only the person starting the server picks notifiers; a tool call can't.

The workflow the agent rules teach: explore a goal with `run_task`, review the YAML in `summary.explore.yaml`, then run it with `run_task suite=<that path>` (or `velocity run`) and commit it; CI replays it with no model.

## Exported flows

`export_session_flow` turns the session's successful `tap`, `type_text`, `scroll`, `swipe`, `press_key`, `launch_app`, `stop_app`, `assert_visible` and `assert_not_visible` calls into a suite. After each action that may have changed the screen (tap, key, swipe, scroll, launch), the next screen the server reads decides an `assertVisible`: an element the previous screen didn't have, by a selector that finds only it there and nothing on the previous screen, preferring an id, then an accessibility label, then exact text; bare layout containers, text with digits (times, counts, prices) and typed text are never used. The YAML is built with `serde_yaml`, so no value can inject YAML. The reply's `gates` says the test is not gated yet (`gated: false`, `trusted: false`) and `gates.next` is the call that gates it; with a `goal` argument it is the exact `run_task` call.

## Core tools

The default profile. Each call expands into the full tool named in brackets and runs it the usual way: the device lease, refs, the session recording, the read-only check and redaction all apply.

### `observe`
Read the device. `what`: `screen` (the default) [`snapshot`, with `mode: "delta"` for only the changes], `screenshot` [`screenshot`: a file path, or with `inline: true` the image, below 2,000 px], `element` [`get_element`, with `ref` or `selector`] or `devices` [`list_devices`]. Read-only.

### `act`
Do one thing and get back what changed on screen. `action`: `tap` [`tap`], `type` [`type_text`, with `text` or `secret`], `swipe`, `scroll`, `press_key` (`key`), `launch` [`launch_app`, `app_id`], `stop` [`stop_app`], `wait` [`wait_for_element`], `assert_visible` or `assert_not_visible`; the target is a `ref` from `observe` or a `selector`. After an action that may change the screen, the server waits for the screen to settle (the device's signal on Android, else two equal reads 100 ms apart, at most 3 s), and the result has a second text block, `Screen now:` and a delta snapshot. The full tool's other arguments pass through: `clear_state` (launch), `until_visible` and `max_scrolls` (scroll), `timeout_ms` and `poll_interval_ms` (wait). A failure carries `structuredContent.error` with `retry` (`safe`, `observe_first` or `no`) and `delivered` ([Failures](#failures)).

### `run`
Background runs. With `suite` (a path in the project), `yaml` (a suite you write, run on the device) or `goal` (and `app_id`; an AI explores, spending model calls) it starts one [`run_task`] and returns its `task_id` at once; poll it. With `task_id` it returns the status [`task_status`, `wait_ms`], stops it with `stop: true` [`task_stop`] or steers an explore run with `instruction` [`task_inject`].

### `artifacts`
What runs left. `kind`: `failure` (the default) [`describe_failure`], `trace` [`inspect_trace`, `task_id` or `test`, `step`], `flow` [`export_session_flow`: this session as a suite], `tests` [`list_tests`] or `flows` [`list_flows`]. Read-only.

### `verify_change`
Check on the device that a change does what its description says, with evidence for the PR: `velocity verify-change` as a background task (poll it with `run {task_id}`). It replays the saved flow for the description with no model, or explores a new one and saves it only when it passes its trust gates. `description` and `app_id` are required; `after` names a login suite to run first. The task's summary is the bundle's `manifest.json` (verdict, cache hit, model calls, evidence), and the bundle folder (`manifest.json`, `failures.md`, `comment.md`, the run's reports and video) is in the task folder. Listed in both profiles. See [CLI.md](CLI.md).

## Full tools

`--profile full`.

### `list_devices`
List devices (simulators, emulators, real devices, browsers). Read-only.

### `screenshot`
Take a screenshot. It is saved as a PNG, and the reply gives its `path`, width and height, so the image stays out of the agent's context. `inline: true` returns the image too, downscaled with a box filter so its longest edge is below 2,000 px. Read-only.

### `tap`
Tap the element matching `selector`, or `ref` (`@e12` from `snapshot`).

### `type_text`
Type `text` (or the value of the environment variable named by `secret`) into the element matching `selector`.

### `swipe`
Swipe `direction`: `up`, `down`, `left` or `right` (the finger's direction). `scroll` takes the content's direction instead.

### `press_key`
Press `key`, one of `back`, `home`, `enter`, `volumeUp`, `volumeDown`, `backspace`, `tab`, `escape`, `delete` and the arrows.

Not every platform has every key:

- iOS has no `escape`, `delete` or arrows. There, `escape` presses back and the result says so.
- The web has no volume keys.

### `list_elements`
List the elements on screen, optionally narrowed by `filter`, as `json` or `text` (`format`). It hands out refs. Read-only.

### `get_element`
Find one element by `selector`. Read-only.

### `assert_visible`
Check that an element matching `selector` (or `ref`) is visible. Read-only.

### `assert_not_visible`
Check that no element matching `selector` (or `ref`) is visible, waiting up to `timeout_ms` (default 5000) for it to go, as a suite's `assertNotVisible` does. Read-only.

### `snapshot`
The screen as compact text, one line per element with a session-stable ref (`@e12`); `mode: "delta"` returns only the changes since the last snapshot; `format: "json"`; `bounds: true` adds each element's box. See [Snapshots and refs](#snapshots-and-refs). Read-only.

### `launch_app`
Launch `app_id` (default: the loaded suite's) as a suite's `launchApp` does; `clear_state: true` wipes its data first.

### `stop_app`
Stop `app_id` (default: the loaded suite's).

### `scroll`
Scroll the content `direction` (`down` shows what is below; default `down`). With `until_visible` (a selector), keep scrolling until that element is on screen, up to `max_scrolls` (default 10), as `scrollUntilVisible` does.

### `get_screen_summary`
A short summary of the screen: its title, visible text, interactive elements and navigation state. It uses fewer tokens than `list_elements`. Read-only.

### `wait_for_element`
Wait until an element matching `selector` is visible, up to `timeout_ms` (checked every `poll_interval_ms`). Read-only.

### `list_flows`
The flows in the loaded suite. Read-only.

### `run_flow`
Run a flow from the loaded suite by `id`.

### `list_tests`
The tests in the loaded suite. Read-only.

### `run_test`
Run a test from the loaded suite by `name`.

### `describe_failure`
Explain the last failed test (or `test`) from its trace in `artifacts_dir`. It gives:

- the failing step and its `file:line`;
- the selector and the error;
- the on-screen elements closest to what the selector asked for, up to `limit`.

Read-only.

### `generate_test_skeleton`
A test from this session: its recorded actions with their assertions; with nothing recorded, a launch and an assertion on the current screen, with the screen's elements listed as comments to build steps from; with no screen either, a placeholder. `description` names the test and goes into a comment. Read-only.

### `export_session_flow`
Turn this session's successful actions into a Velocity test named `name`, with an `assertVisible` after each action that changed the screen (see [Exported flows](#exported-flows)). `clear` starts the recording afresh. Read-only.

### `open_inspector`
Start the inspector web UI on `port`, sharing the server's device connection, and return its URL (with the session token). Calling it again returns the same URL.

### `close_inspector`
Stop the inspector.

### `run_task`
Run a suite in the background: `suite` (a path; default the loaded suite) or `yaml` (the suite itself), with `filter`, `tags`, `device`, `retries`, `verify` (`true`: retry a failing test once, so a flaky one is reported as flaky) and `step_screenshots` (default true). Answers at once with `task_id`, `dir`, `log` and `trace_dir`. Or explore: `goal` with `app_id` (default the loaded suite's), `platform`, `device`, `model` or `script`, `data`, `secret`, `max_steps`, `verify` (a level), `replay` and `out`; answers with `task_id`, `kind: explore`, `dir`, `log` and `out`. See [Background tasks](#background-tasks).

### `task_status`
A task's state, progress and, once done, its summary. Without `task_id`, the session's tasks. `wait_ms` (up to 30000) waits for the step to change or the task to end. Read-only.

### `task_stop`
Stop a task (Ctrl-C to its run, which writes its reports) and wait up to `wait_ms` (default 15000) for it to end.

### `task_inject`
Give a running exploration (`run_task` with a `goal`) an `instruction`, taken in at its next planning turn. Suite runs take none and say so.

### `diagnose`
Check the machine and device: adb and its server, Android devices (booted, authorized) and the on-device tree server (by reading the screen), Xcode, booted simulators, the iOS runner build, device leases, AI keys (names only), disk and memory. Returns `verdict` (`ready`, `degraded` or `blocked`), each check, and ordered `next_steps`, each `Run: <command>` or `Guidance: <what to do>`. `attempt_fix: true` makes only safe fixes and lists them in `fixes_applied`: restart an adb server that doesn't answer, clear lease records left by exited processes, and remove the tree server's jar from a device whose screen can't be read (only when its lease is free) so it is pushed again. `probe_device: false` skips reading the screen; `platform` narrows the checks.

### `inspect_trace`
Read a run's trace, from a task (`task_id`) or an `artifacts_dir`, for `test` (default: the newest trace). `view`: `summary` (one line per step: status, action, time, error), `search` (steps whose label, selector, error, matched element or screen text contain `query`), `step_details` (one `step`, 1-based: selector, matched element and tap point, nearest elements, settle timing, heals, warnings, error) or `step_screenshot` (the screen the step acted on, as an image with a red ring at the tap point; screenshots are kept for failing tests). Read-only.
