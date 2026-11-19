---
name: velocity
description: Write, run and fix Velocity mobile UI tests (Android, iOS, web YAML suites) with the Velocity MCP server. Use when asked to test an app's UI, drive an emulator, simulator or browser, write or repair a velocity.yaml suite, or explain a failing Velocity run. Not for unit tests or non-UI code.
---

# Velocity: mobile UI tests from an agent

Velocity runs YAML test suites on Android emulators/devices, iOS simulators/devices and Chrome. Its MCP server (`velocity mcp`) lets you drive the device, record what you did as a suite, run it and fix it.

## Tools

`velocity mcp` serves five tools (`--profile full` lists the ones they stand for; their names are in brackets below): `observe` reads the screen, `act {action: …}` does one thing and returns what changed, `run` starts and polls background runs, `artifacts {kind: …}` explains failures and exports the session, and `verify_change` checks a change and leaves PR evidence.

`velocity skills get mcp-tools` says what each changes, returns and whether a call is safe to repeat.

## Workflow

Two ways to get a test: let `velocity explore` write one for a goal (below), or drive the app yourself and export the session (steps 2-5).

1. **Check the machine**: `velocity doctor` in a shell. If `blocked`, do its `Run:` / `Guidance:` steps first.
2. **Look at the screen**: `observe` [`snapshot`] (one line per element, each with a ref like `@e12`). `mode: "delta"` shows only what changed. Use `observe {what: screenshot}` only for what text can't show (images, colours).
3. **Drive the app**: `act` with `launch`, `tap`, `type`, `scroll`, `swipe`, `press_key` (`back`, `enter`, …). Target elements with `ref: "@e12"` from the latest observe, or a `selector` (`{"id": "login"}`, `{"text": "Sign in"}`). A stale ref is an error that says so: observe again.
4. **Check as you go**: `act {action: assert_visible}` / `assert_not_visible`.
5. **Save it**: `artifacts {kind: flow}` [`export_session_flow`] turns the session's actions into a suite, with an `assertVisible` after each action that changed the screen. Review those assertions, then write the YAML to a file (for example `tests/login.yaml`).
6. **Run it**: `run {suite: <path>}` [`run_task`] returns at once with a `task_id`; poll `run {task_id}` [`task_status`] until it is `passed`, `failed`, `stopped` or `error`. Or in a shell: `velocity run tests/login.yaml`.
7. **Fix failures**: `artifacts` [`describe_failure`] gives the failing step, its `file:line`, the error and the closest elements on screen; `artifacts {kind: trace}` [`inspect_trace`] shows each step, and `step_screenshot` the screen with a red ring where it tapped. Change the selector or add a wait, then run again.

## Explore a goal, then run the YAML

`run {goal: "open Network & internet", app_id: …}` [`run_task` with a `goal`] starts `velocity explore`: an agent reaches the goal on the device one operation per turn, writes the session as a deterministic YAML test with assertions, and replays it with no model calls to prove it holds. It needs a model key (or `script`: a fixed plan, docs/EXPLORE.md); values it may type go in `data` (`{"EMAIL": "qa@example.com"}`) and secrets by name in `secret` (`["LOGIN_PASSWORD"]`, read from the server's environment).

1. Poll `run {task_id}` (`wait_ms: 10000` waits for the next turn): `progress` shows the turn, op, target and phase.
2. If it heads the wrong way, steer it with `run {task_id, instruction: "use the search box"}`; `run {task_id, stop: true}` ends it.
3. When it is done, `summary.explore` has the `outcome`, the `yaml` path, the replays (all should pass, with 0 model calls), and the model calls and tokens spent.
4. **Review the YAML** like code: selectors, and that the `assertVisible` steps check what the goal means. Move it into the project's tests.
5. **Run it** with `run {suite: <that path>}` (or `velocity run <path>`). It runs at native speed with no model; commit it and CI replays it the same way.

## Rules

- **Never put secrets in YAML.** Use `${ENV_VAR}` (for example `text: ${LOGIN_PASSWORD}`) and set the variable in the environment. With MCP, type them with `act {action: type, secret: "LOGIN_PASSWORD"}` [`type_text`]: the value is typed, never shown, and the export writes `${LOGIN_PASSWORD}`. The variable must be listed in `VELOCITY_SECRETS`.
- Prefer stable selectors: `id` (resource-id / accessibility identifier), then `accessibilityId`, then exact `text`. Avoid text that changes (times, counts, prices) and `index` unless elements are true twins.
- Don't add sleeps: Velocity waits for the screen to settle after every step. Use `assertVisible` to wait for something to appear.
- One device runs one thing at a time: while a `run` task runs on a device, `act` and `observe` on it report it busy.

## YAML cheat sheet

```yaml
appId: com.example.app            # package / bundle id (required)
config:
  platform: android               # android | ios | web
  retry: { count: 1 }
tests:
  - name: login works
    tags: [smoke]
    steps:
      - launchApp: { clearState: true }
      - tap: { id: login_button }
      - inputText: { selector: { id: email }, text: qa@example.com }
      - inputText: { selector: { id: password }, text: "${LOGIN_PASSWORD}" }
      - pressKey: { key: enter }
      - assertVisible: { text: Welcome }
      - scrollUntilVisible: { selector: { text: Settings }, direction: down }
      - assertNotVisible: { id: spinner }
      - runFlow: logout
flows:
  - id: logout
    steps:
      - tap: { accessibilityId: Profile }
      - tap: { text: Log out }
```

- Selectors: `id`, `text`, `textContains`, `textMatches` (regex), `accessibilityId`, `className`, `role` + `name`, state filters `checked` / `enabled` / `selected` / `focused`, `index`, and relations `below` / `above` / `leftOf` / `rightOf` / `childOf` / `containsChild` (each takes a selector).
- Other actions: `doubleTap`, `longPress`, `clearText`, `assertText`, `swipe: { direction: up }`, `screenshot`, `assertScreenshot`, `stopApp`, `wait`, `openLink`. Step modifiers: `optional: true`, `timeoutMs`.
- Commands: `velocity validate <suite>` checks a suite; `velocity run <suite> [--filter 'name*'] [--device <id>]`; `velocity schema` prints the JSON Schema; `velocity mcp config <client>` prints the MCP config snippet.
