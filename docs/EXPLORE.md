# `velocity explore`: AI explores once, replay runs CI

```bash
velocity explore "open Network & internet" --app-id com.android.settings   # an agent reaches the goal and writes open-network-internet.yaml
velocity run open-network-internet.yaml                                     # plain YAML: native speed, no model
# commit it; CI runs it like any other suite
```

An agent works through a goal on a real device, one operation per turn. The session is written out as a deterministic Velocity test with real assertions, replayed straight away with **no model calls** to prove it holds, and repaired once if the replay fails. The model is paid for while exploring; CI never calls it.

This is the opposite trade-off to agents that call a model on every step of every run (Google's ARTEMIS: Android only, 3 to 40 s per step, and its maintainer declined record-and-replay). Here the model runs once per test and the replay runs at the deterministic runner's speed.

## How it works

Code: `crates/velocity-explore` (the loop, the YAML writer, the replay) and `crates/velocity-nl/src/goal.rs` (the goal prompt and its wire formats). The loop drives the same executor as `velocity run`, so it gets the same sync, settle, dialog handling and actionability checks.

Each turn has five steps:

1. **Observe.** The element tree is read and listed as numbered lines, as for natural-language steps (`ScreenIndex`), with the states the driver reports (`checked`/`unchecked`, `selected`, `focused`, `disabled`) and an empty field's `hint`. A screenshot is attached when the provider takes images and `--screenshots` asks for one: `poor` (the default: only when fewer than 4 elements are listed), `always`, or `never`.
2. **Plan.** The goal model answers one JSON object: `op` (`tap`, `longPress`, `inputText`, `clearText`, `scroll`, `swipe`, `back`, `home`, `enter`, `openApp`, `launch`, `wait`, `assert`, `answer` or `done`), an `element` number, a `value` *name*, a `direction`, `ms`, an `outcome` (for `done`: `success` or `giveUp`), a `reason`, an `expect` (see below), and a `memo`: a short note the model keeps for itself (facts gathered on earlier screens, what is left), shown back to it next turn and never acted on by code. `scroll` with an element number scrolls inside that list or picker. The prompt's version (`GOAL_PROMPT_VERSION`, now `goal-4`) is in the JSON report.
3. **Ground.** Code builds everything that has to be right:
   - the selector, from the element number (`robust_selector`: id, then accessibility label, then unique text, with `index:` only for twins);
   - the typed value, looked up by name (see Values);
   - the app to open (`openApp`), by name from `--open-app NAME=ID` / `--apps-file`;
   - refusals for a disabled target, a number not in the list, a made-up value, an unlisted app, an ungrounded answer, or a wait over 10 s. A refusal goes back to the model as a note, and three in a row end the run.
4. **Act.** The action runs through the executor, which settles the screen afterwards.
5. **Check the expectation, record.** Code judges the model's `expect` against the screen the action led to (below), and the action is kept with the screen before and after it.

"The model picks; code decides": model text never becomes a selector or a typed value. The `reason` field only reaches logs and history.

### Expected post-conditions (`expect`)

Before acting, the model says what should hold afterwards (Q-Reflector, arXiv 2608.05587: an external reflector judging each transition against a predicted post-condition added 14.5 to 17.1 points of task success on MobileWorld). `expect` is `{"kind", "text", "element"}` with one of these kinds:

| kind | means | checked by code against the next screen |
|---|---|---|
| `appears` + `text` | the next screen shows this text | a shown element whose text or label equals it (case, spaces and typographic quotes folded) or contains it as whole words (3+ characters) |
| `gone` + `element` | element *n* of this list disappears | its selector (content first, else the robust one) matches nothing |
| `checked` / `unchecked` + `element` | switch or checkbox *n* ends in that state | the same element found again by its selector, and its reported state |
| `none` | nothing named (always for `inputText`, `assert`, `answer`, `done`) | — |

The short forms `{"appears": "Saved"}`, `{"gone": 3}`, `{"checked": 3, "value": true}` are read too. Without the field (older models, `goal-3` replies with free text) the turn works as before. A malformed `expect` never fails the reply. Each expectation ends as:

- **met**: a stable assertion is added after the step in the test: `assertVisible` on the matched element, `assertNotVisible` on the gone one, or `assertVisible { …, checked: true }`. It is built from the real element with the same selector builder as the other assertions, never from the model's text. It is written only when it would fail without the step (it didn't hold before), never on volatile text or user data, not for scrolls and swipes, and is deduped with the screen assertion. A replay that fails on it drops it, as for screen assertions.
- **not met**: the model is told at once with a `note` event (`You expected "Saved" to appear after tap { text: "Save" }, but no element on the screen now shows it: the action did not do what you meant. Read this screen and re-plan from it.`), the history line says `(expected "Saved" to appear: not met)`, the action counts toward the stuck limit like an unchanged screen, and nothing is asserted.
- **not checkable**: an element number not in the list, an element that isn't a switch, a value's name instead of a text, a failed action, an unreadable form. Counted only.

The JSON report has `expectations: {met, notMet, notCheckable}` (also on the `finished` event); each step has its `expectation` and the `acted` event carries it. A met `checked` expectation on a screen whose states changed counts as progress even when its structure (the hash) didn't.

### Context for long runs

Only the current screen is listed in full. Earlier turns are one line each, for example `#3 tap { text: "Wi-Fi" } -> "Internet"`, with `(screen unchanged)` or `(seen after turn 1)` added where they apply. The model sees the newest 12 lines, capped at 2,400 characters, and older turns are counted. Screens are compared by a structural hash: the listed elements' type, id, label and text, without positions, with digits folded so a clock doesn't make every screen new. That hash drives the "you have seen this screen before" note.

### Budgets and stuck detection

| Limit | Default | Ends the run as |
|---|---|---|
| `--max-steps` (planning turns) | 40 | `budget exhausted` |
| `--timeout` (seconds) | 600 | `timed out` (exit 4) |
| `--max-tokens`, `--max-cost` (needs `--price IN,OUT`) | none | `budget exhausted` |
| Actions in a row that leave the screen unchanged or miss their expectation | 3 (the model is told after 2) | `stuck` |
| Reaching the same screen 6 times (20 with `--benchmark`) | — | `stuck` |
| Answers code refused, 3 in a row | — | `gave up` |

The model is also told when its last action worked but changed nothing on the list (elements, their states and bounds compared, so a toggled switch counts as a change; "may be at its end" for a scroll), and when it is back on a screen it saw within the last 4 turns. Both are on-demand reflection (MobileUse, arXiv 2507.16853) and are emitted as `Note` events.

### Demonstrations from the project's tests

One demonstration of a similar task took a model from 19.3% to 51.7% success in LearnAct (arXiv 2504.13805), and a project already has the best demonstrations there are: its own passing tests. Before the first turn, explore looks for suites under `--demos-from` (default: the folder `--out` writes to): up to 3 folders deep, skipping hidden folders, `target`, `node_modules`, `build`, `Pods` and `DerivedData`. It keeps tests of the same app and platform and ranks them by word overlap with the goal (a word in a test's name or description counts double). The best one, and a second scoring at least half as well, are shown as "How similar goals were done in this app": one step per line, flows inlined, at most 1,500 characters, redacted, with typed text only as `${NAME}`. They are hints. The reply format and grounding don't change: an element must still be on the current list, and a value is still only a name. `--no-demos` turns this off, and `--benchmark` never uses it. The JSON report lists the tests shown under `demos`.

### Verification (`--verify off|final|checkpoints|strict`, default `final`)

When the model answers `done` with `success`, the checker model is asked whether the goal holds on a fresh read of the final screen. It must cite real elements from the list: a "holds" with no real citation is `unknown`, not a pass (the `verify:` rule in [DSL.md](DSL.md)). If the check fails, the model is told and keeps going. Two failed checks end the run as `unverified`, and no test is written. Under `checkpoints`, each assertion the model proposes is also checked against its `reason`; under `strict`, a failed checkpoint ends the run. With `off`, the model's word is taken, and the summary says so.

## The generated test

The YAML is built as a `serde_yaml` value (no string splicing), parsed and validated before it is written, and checked to contain no secret's value.

- `appId`, then one test named after the goal (`--name` overrides it) with `description:` set to the goal, so `verify: final` can check it later.
- `launchApp` comes first, with `clearState: true` under `--clear-state`. Under `--no-launch` there is no launch step.
- Every action that worked, with the selector code built for it when it ran.
- A run of scrolls before an action on an element becomes one `scrollUntilVisible` on that element, so the test doesn't depend on scroll distances. Scrolls with no target after them stay as `swipe`.
- After each step that changed the screen, an `assertVisible` on an element that is new to that screen. Its selector must match only that element on the new screen and nothing on the screen before; a selector that also matched before would pass without the step doing anything. The preference order:
  1. the screen's title;
  2. an accessibility label;
  3. a unique text;
  4. an id.

  Content comes before ids because an id like `collapsing_toolbar` is on every page and doesn't show which page this is. Short names are preferred over long ones.
- After a step whose `expect` was met, the assertion built from the matched element (`assertVisible`, `assertNotVisible`, or a `checked:` state).
- The model's `assert` proposals, then, at the end, the elements the checker cited for the goal.
- **Never asserted:** volatile text (times, dates, prices, percentages, counters, numbers of 3 or more digits, two or more separate numbers) and text that contains a `--data` or `--secret` value. A small number in a name (`Android 15`, `Wi-Fi 6`) is fine.

### Values

The model never writes text to type. It picks a name from a list:

| Source | Model sees | YAML types |
|---|---|---|
| A quoted literal in the goal (`search for "blue shoes"`) | `TEXT1 = "blue shoes"` | `blue shoes` |
| `--data EMAIL=alice@example.com` | `EMAIL (test data, 17 characters)` | `${EMAIL}` |
| `--secret PASSWORD` (value read from `$PASSWORD`) | `PASSWORD (secret)` | `${PASSWORD}` |

A secret's value is registered for redaction, never sent to a model, and never written to events, the JSON report or the YAML. The replay fills `${NAME}`s from the same values. When you run the test later, pass them with `--env NAME=…` or the environment.

#### `--values-from-goal` (off by default)

Benchmark goals carry their values unquoted ("Create a contact for Hugo Pereira. Their number is +13920741751."). With `--values-from-goal` the model may put the text itself in `value`, and code accepts it only when it is grounded (`crates/velocity-explore/src/values.rs`):

- it occurs verbatim in the goal, case, whitespace and typographic quotes folded; or
- it occurs verbatim in a text shown on a screen during this run (copying from one app to another); or
- it is a plain number (a computed amount, a count); or
- it is pieces joined by line breaks, commas or semicolons, each of which passes one of the rules above (merging notes, a copied list).

Anything else (a reworded name, words in a new order, text only visible in an image) is refused and the model is told why. Without the flag nothing changes: typed text is named, as above.

### Answers (`answer`)

When the goal asks a question, the model answers `{"op":"answer","value":"…","element":n}`, citing the element that shows it (or -1 when it was on an earlier screen or is computed), and then `done`. Code splits the answer at commas and accepts each part only if it is text shown in this run (or the cited element), a number, a yes/no (`True`/`False`) judgement, or a phrase whose every word is a number, a word shown on a screen or in the goal, or a calendar or unit word (`October 23 2023 15:00`, `2 tablespoons`). The accepted answer is in the report (`answer`) and an `answered` event; `done` after an accepted answer ends the run as `success` without the screen check (the answer was already checked against what the run saw). What code can't check: whether a computed number is right.

### Benchmark mode (`--benchmark`)

For harnesses that judge the device state themselves (AndroidWorld: [BENCHMARKS.md](BENCHMARKS.md#androidworld)): the run starts from the screen as it is, writes and replays no test, auto-dismisses no dialog (every action is the agent's), refuses `launch` and `assert`, allows 20 visits per screen, and sends a screenshot every turn when the provider takes images. The model is told each turn that the run is judged on the final device state. The trajectory goes to `$VELOCITY_EVENTS_FILE`.

### `--keep-nl`

Writes `<out>.nl.yaml` beside the deterministic test: the same session as plain-English `do:` and `assert:` steps (`Tap "Network & internet"`), with data as `data: ${NAME}`, ending in an `assert:` of the goal. Use it if your team wants steps that the planner re-resolves and heals when the UI changes. It needs a model whenever a step isn't cached yet. The deterministic YAML is always written, and it is the file that gets replayed.

## Replay and repair

`--replay N` (default 1; 0 skips it) runs the written test N times right away. Each run uses a fresh executor with no planner attached, so a model call is impossible. The job also compares the planner's and checker's call counters before and after each run, and treats any call as an internal error. The summary reports the pass rate and the time per run and per step.

If a replay fails, one repair round runs. What it does depends on the failing step:

| The failing step was | The repair |
|---|---|
| A screen assertion or a final assertion | Drop that assertion. It didn't hold on a second run, so it wasn't stable |
| An action | Explore again: relaunch, replay the recorded steps before the failing one without the model, and let the agent continue from there with the turns it has left (at least 5) |
| The launch | Not repairable |

After a repair, the test is written again, validated and replayed again. The report has both replays and says what the repair did. If it still fails, the exit code is 1.

## Steering a run: CLI, MCP, console and library

- **CLI.** `--inject-file PATH` feeds the agent lines appended to that file while it runs. `--json` prints the result (`report`, `yaml`, `replay`, `repair`, `files`) on stdout and the live lines on stderr. With `VELOCITY_EVENTS_FILE` set, every event is also appended to that file as a JSON line (the same file and mechanism as `velocity run`'s live events, `velocity_common::live_events`), and the replays add their test and step events. Ctrl-C ends the session at its next turn as `stopped`, with a report; a second one exits at once.
- **MCP.** `run_task` with a `goal` runs `velocity explore … --json --inject-file <task>/inject.txt` as a child: `task_status` shows the turn, op and target from the events file, `task_inject` appends a line to the inject file, `task_stop` is a Ctrl-C, and the summary has the YAML path, the replays and the model's calls and tokens ([MCP.md](MCP.md#background-tasks)).
- **Console.** The Explore panel of `velocity inspect` starts the same child, streams the events, has an *Inject* box, and shows the YAML with *Save to project* ([CLI.md](CLI.md#inspect)).
- **Library.** `velocity_explore::ExploreJob` (the whole pipeline) and `ExploreSession` (the loop alone) are the API the CLI uses.
  - **Events.** `with_events(Box<dyn FnMut(&ExploreEvent)>)` receives serde-serializable events tagged `event`: `prefix`, `observed`, `planned`, `rejected`, `acted`, `asserted`, `checked`, `injected`, `note`, `replayRun`, `repair`, `finished`. Each carries `atMs`. The library never prints, so `velocity mcp`'s stdout stays clean.
  - **Instructions.** `handle().inject(String)`, or `with_instructions(Box<dyn InstructionSource>)`. `InstructionSource` is one method, `fn drain(&mut self) -> Vec<String>`, and is implemented for tokio and std receivers. Instructions are taken in at the next planning turn and shown to the model as an operator's instruction.
  - **Stopping.** `handle().stop()`, or `with_cancel(Arc<AtomicBool>)`. The run stops at the next turn, and the outcome is `stopped`.

## Without a model: `--script`

`--script plan.json` plays a fixed plan instead of a model. Targets are named by text, and each turn the planner reads the numbered list as a model would and answers with a number. Observing, grounding, acting, settling, the assertions and the replay all run for real. This is how the device checks in [STATUS.md](STATUS.md) were run without API keys.

```json
{
  "steps": [
    { "op": "tap", "target": "Network & internet", "scroll": "down", "expect": { "appears": "Internet" } },
    { "op": "assert", "target": "Internet" },
    { "op": "done", "outcome": "success", "reason": "the page is open" }
  ],
  "check": { "holds": true, "evidence": ["Internet", "Airplane mode"], "reason": "listed" }
}
```

A step's `expect` names elements by text as `target` does: `{"appears": "Saved"}`, `{"gone": "Battery"}`, `{"checked": "Wi-Fi"}`, `{"unchecked": "Wi-Fi"}`.

## Cost

Exploration costs one goal-model call per turn, plus one checker call per `done(success)` (and per assertion under `checkpoints`). A turn sends the system prompt (cached), the current screen list (typically 1 to 3k tokens for a phone screen), and at most 12 history lines, and gets about 60 output tokens back. A 10-turn exploration is therefore roughly 20 to 40k input tokens. Pass `--price IN,OUT` to see the estimate, and `--max-cost` to cap it. Replays and CI cost nothing.

## Limits

- **Real-model accuracy is unmeasured.** No API keys were available during development. The prompt, the schema and the three wire formats (Anthropic, OpenAI-compatible, Bedrock) are tested against mock servers, and the loop against scripted plans on a fake app and on real devices. How often a real goal model reaches real goals is not yet known.
- Text input goes only through named values. A goal that needs text you didn't provide makes the model give up and name the missing value.
- One test per goal. Branches, loops and conditionals aren't generated.
- Web works through the same loop (`--platform web`), but it hasn't been checked live.
- The volatile-text rules are heuristics. A false negative, such as an unusual date format, shows up as a failed replay, and the repair drops that assertion.
