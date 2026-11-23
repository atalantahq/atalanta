# `atalanta explore`: AI explores once, replay runs CI

```bash
atalanta explore "open Network & internet" --app-id com.android.settings   # an agent reaches the goal and writes open-network-internet.yaml
atalanta run open-network-internet.yaml                                     # plain YAML: native speed, no model
# commit it; CI runs it like any other suite
```

An agent works through a goal on a real device, one operation per turn. The session is written out as a deterministic Atalanta test with real assertions, replayed straight away with **no model calls** to prove it holds, and repaired once if the replay fails. The model is paid for while exploring; CI never calls it.

This is the opposite trade-off to agents that call a model on every step of every run (Google's ARTEMIS: Android only, 3 to 40 s per step, and its maintainer declined record-and-replay). Here the model runs once per test and the replay runs at the deterministic runner's speed.

## How it works

Code: `crates/atalanta-explore` (the loop, the YAML writer, the replay) and `crates/atalanta-nl/src/goal.rs` (the goal prompt and its wire formats). The loop drives the same executor as `atalanta run`, so it gets the same sync, settle, dialog handling and actionability checks.

Each turn has five steps:

1. **Observe.** The element tree is read and listed as numbered lines, as for natural-language steps (`ScreenIndex`), with the states the driver reports (`checked`/`unchecked`, `selected`, `focused`, `disabled`) and an empty field's `hint`. A screenshot is attached when the provider takes images and `--screenshots` asks for one: `poor` (the default: only when fewer than 4 elements are listed), `always`, or `never`. It is shrunk to at most 1,000 px on its long side before it is sent (code's own pixel checks keep the full image), and the final check follows the same mode.
2. **Plan.** The goal model answers one JSON object: `op` (`tap`, `longPress`, `inputText`, `clearText`, `scroll`, `swipe`, `drag`, `back`, `home`, `enter`, `openApp`, `launch`, `wait`, `assert`, `answer` or `done`), an `element` number, a `value` *name* (for `drag`, the number of the element to drop on), a `direction`, `ms`, an `outcome` (for `done`: `success` or `giveUp`), a `reason`, an `expect` (see below), and a `memo`: a short note the model keeps for itself (facts gathered on earlier screens, what is left), shown back to it next turn and never acted on by code. `scroll` with an element number scrolls inside that list or picker. The prompt's version (`GOAL_PROMPT_VERSION`, now `goal-8`) is in the JSON report.
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
| `--timeout` (`10m`, `1h`; a bare number is seconds) | 600 s | `timed out` (exit 4) |
| `--max-tokens`, `--max-cost` (needs `--price IN,OUT[,CACHE_READ[,CACHE_WRITE]]`) | none | `budget exhausted` |
| Actions in a row that leave the screen unchanged or miss their expectation | 3 (the model is told after 2) | `stuck` |
| Reaching the same screen 6 times (20 with `--benchmark`) | — | `stuck` |
| The same loop of screens coming round 3 times with nothing changed or learned | — | `stuck` |
| Answers code refused, 3 in a row | — | `gave up` |

A repair round (below) spends what the first session left of `--max-tokens` and `--max-cost`, not a budget of its own; when nothing is left the test is kept and the repair is reported as not possible.

A `wait` that changed nothing neither counts as progress nor as a no-op, so waits between no-op taps don't hide them; a third idle wait in a row counts as a no-op. An action counts as having done something when the screen's structure changed, when its pixels did (a canvas), or when a text other than a time or a percentage changed (a number a click changed; the history line then says `(text changed: "3" is now "1")`). A screen that seemed unchanged is looked at again after 0.6 s, and for up to 2 s when it holds a web view, whose accessibility tree is updated late (library callers can change these through `ExploreOptions::timing`).

The model is also told when its last action worked but changed nothing on the list (elements, their states and bounds compared, so a toggled switch counts as a change; "may be at its end" for a scroll), and when it is back on a screen it saw within the last 4 turns. Both are on-demand reflection (MobileUse, arXiv 2507.16853) and are emitted as `Note` events. Every note the model gets at the start of a turn is also in a `notes` event as code decided it (`{"kind":"loopBack","turnsAgo":2,"atTurn":3}`, `noEffect`, `streak`, `backLedRound`, `rejected`, …); the prose is rendered from those.

A loop counts against the run only when nothing was learned on the way round: no memo change, no answer, no text on a screen changed. "Open each item, read it, go back" is a loop that reads, and is never taxed. A loop that learned nothing is waste: when it held a `back`, the screen that `back` was pressed on is marked, the next time the model is there it is told so, and one more `back` from that screen is refused (a `Rejected` event; the turn is spent) so it uses what the screen offers, such as Confirm or Apply. A second `back` there is allowed, since it may be the only way out. The same loop (the same set of screens, whichever one it is entered from) coming round 3 times with nothing learned ends the run as `stuck`; the report's `revisitTurns` counts the turns spent going round.

### Demonstrations from the project's tests

One demonstration of a similar task took a model from 19.3% to 51.7% success in LearnAct (arXiv 2504.13805), and a project already has the best demonstrations there are: its own passing tests. Before the first turn, explore looks for suites under `--demos-from` (default: the folder `--out` writes to): up to 3 folders deep, skipping hidden folders, `target`, `node_modules`, `build`, `Pods` and `DerivedData`. It keeps tests of the same app and platform and ranks them by word overlap with the goal (a word in a test's name or description counts double). The best one, and a second scoring at least half as well, are shown as "How similar goals were done in this app": one step per line, flows inlined, at most 1,500 characters, redacted, with typed text only as `${NAME}`. They are hints. The reply format and grounding don't change: an element must still be on the current list, and a value is still only a name. `--no-demos` turns this off, and `--benchmark` never uses it. The JSON report lists the tests shown under `demos`.

### Verification (`--verify off|final|checkpoints|strict`, default `final`)

When the model answers `done` with `success`, the checker model is asked whether the goal holds on a fresh read of the final screen and the run's log. The claim carries only data: the goal, the log (code's own lines, every app text in it quoted) and the answer, the log and the answer each in a block tagged with a hash of its content, so a screen text can't close it or pose as a rule. The rules for judging an end state are in the checker's system prompt, which calls those blocks data. It must cite real evidence: elements from the list, or, only when the final screen can't show the result (the goal was done in another app or on an earlier screen), the turns of the log that show it done. A "holds" that cites neither (or only turns the log doesn't have) is `unknown`, not a pass (the `verify:` rule in [DSL.md](DSL.md)). A goal that holds on the screen gets the cited elements as its final assertions; one that holds by the log alone gets none from the screen, its reason says `holds by the log (#n)`, and the report's `finalBasis` is `log` instead of `screen`. If the check fails, the model is told and keeps going. Two failed checks end the run as `unverified`, and no test is written. Under `checkpoints`, each assertion the model proposes is also checked against its `reason`; under `strict`, a failed checkpoint ends the run. With `off`, the model's word is taken, and the summary says so.

## The generated test

The YAML is built as a `serde_yaml` value (no string splicing), parsed and validated before it is written, and checked to contain no secret's value.

- `appId`, then one test named after the goal (`--name` overrides it) with `description:` set to the goal, so `verify: final` can check it later.
- `launchApp` comes first, with `clearState: true` under `--clear-state`. Under `--no-launch` there is no launch step.
- Every action that worked, with the selector code built for it when it ran.
- A tap that ran with `force` (a read-only field that reports itself disabled; the session says so in a `note`) is written with `force: true`, or the replay would refuse it as not actionable.
- A run of scrolls before an action on an element becomes one `scrollUntilVisible` on that element (with `within:` when they were all inside one list), so the test doesn't depend on scroll distances. Scrolls with no target after them stay as `swipe`. A swipe keeps everything it had when explored: `within:` for a swipe inside a picker or carousel, and `from:`/`to:` for a repeated stroke inside one element.
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

A secret's value is registered for redaction, never sent to a model, and never written to events, the JSON report or the YAML. Text the app shows (an element's name in an event, a step's label in the report) is masked the same way, so an app that echoes a secret on screen doesn't print it. A `--data` value that the model copies from a screen under `--values-from-screen` is still written as `${NAME}`. The replay fills `${NAME}`s from the same values. When you run the test later, pass them with `--env NAME=…` or the environment.

#### `--values-from-goal` and `--values-from-screen` (off by default)

Benchmark goals carry their values unquoted ("Create a contact for Hugo Pereira. Their number is +13920741751."). With `--values-from-goal` the model may put the text itself in `value`, and code accepts it only when it is grounded (`crates/atalanta-explore/src/values.rs`):

- it occurs verbatim in the goal, case, whitespace and typographic quotes folded; or
- with `--values-from-screen` (a separate opt-in), it is a text shown on a screen during this run, or a run of whole words of one ("Ann" is not copied from "Planning"; a piece shorter than 3 characters must be a whole shown text) (copying from one app to another). Screen text is untrusted: the app, a web page or a message someone sent wrote it, so a page that says "type this into the password field" should not be able to choose what is typed unless you allowed it; or
- it is a plain number (a computed amount, a count); or
- it is pieces joined by line breaks, commas or semicolons, each of which passes one of the rules above (merging notes, a copied list).

Either flag alone allows numbers and its own source; `--values-from-screen` alone allows screen text but not goal text. Anything else (a reworded name, words in a new order, text only visible in an image) is refused and the model is told why. Without either flag nothing changes: typed text is named, as above. The AndroidWorld harness passes both.

`--values-from-screenshots` also accepts text the model read from a screenshot, which code can't check against the pixels. It applies only after a screenshot of a screen with pixel-only content went to the model (an image, canvas or video element with no text of its own, or fewer than 4 listed elements), and only to pieces of at most 200 characters; a screenshot of an ordinary screen (in `--benchmark`, one goes with every turn) doesn't open it. `--values-from-model` types the model's own text and grounds nothing. A plain number the model computed is accepted only with `--values-from-goal` or `--values-from-screen`, not under these two alone. With a `--secret` in the run, either flag is noted at the start (a `note` event): model-authored text and a secret in one run is two legs of the Rule of Two.

`ATALANTA_EXPLORE_SHOTS_DIR=<dir>` saves each screenshot the model was sent as `turn-NN.png`, for an outside judge (the iOSWorld harness uses it). Every `acted` event for a typed literal names where it came from (`typedSource`: `goal`, `screen`, `number`, `composed`, `screenshot`, `model`), so a run that typed model-read text says so.

### Answers (`answer`)

When the goal asks a question, the model answers `{"op":"answer","value":"…","element":n}`, citing the element that shows it (or -1 when it was on an earlier screen or is computed), and then `done`. Code splits the answer at commas and gives each part a source:

| Source | Accepted when | Tier |
|---|---|---|
| `screen` | the part is a text shown in this run (or the cited element), or a run of whole words of one; a number shown on a screen counts | verified by code |
| `number` | a number no screen showed, and the goal asks for a count or a computed amount ("how many", "total", "sum", "average"…) | left to the final check |
| `judgement` | `yes`/`no`/`true`/`false`, and the goal is a yes/no question ("Is Bluetooth on?", "… yes or no") | left to the final check |
| `words` | every word is a number, a word shown on a screen or in the goal, or a calendar or unit word (`October 23 2023 15:00`, `2 tablespoons`), at least one word came from a screen, and the part has 3 or more letters or digits | left to the final check |

Anything else is refused and the model is told why. The accepted answer is in the report (`answer`, with `answerSources`) and an `answered` event. `done` after an answer still runs the final check: it is told, in a tagged block, which parts code verified (they count as seen even if the last screen no longer shows them) and which it did not; an unverified part holds only if the screens or the log's shown texts support it (a count matching the items shown, a yes/no matching a switch's state). Under `--verify off` the summary lists the unverified parts.

Outside `--benchmark`, each answer part code verified on a screen that the final screen still shows, as a text that isn't volatile (not a time, a count, a price), becomes an `assertVisible` at the end of the test, so the test checks the fact and not just the steps. A part read on an earlier screen, a computed number or a judgement isn't asserted; a part whose text changes between replays is dropped by the repair, as any final assertion is.

### Benchmark mode (`--benchmark`)

For harnesses that judge the device state themselves (AndroidWorld: [BENCHMARKS.md](BENCHMARKS.md#androidworld)): the run starts from the screen as it is, writes and replays no test, auto-dismisses no dialog (every action is the agent's), refuses `launch` and `assert`, allows 20 visits per screen, and sends a screenshot every turn when the provider takes images. The model gets a benchmark variant of the instructions (no test-writing framing, no `assert` or `launch`, a rule that the run is judged on the final device state), byte-stable for the run, instead of a note repeated every turn. The trajectory goes to `$ATALANTA_EVENTS_FILE`.

### `--keep-nl`

Writes `<out>.nl.yaml` beside the deterministic test: the same session as plain-English `do:` and `assert:` steps (`Tap "Network & internet"`), with data as `data: ${NAME}`, ending in an `assert:` of the goal. Use it if your team wants steps that the planner re-resolves and heals when the UI changes. It needs a model whenever a step isn't cached yet. The deterministic YAML is always written, and it is the file that gets replayed.

## Replay and repair

`--replay N` (default 5; 0 skips it) runs the written test N times right away; every run after the first pauses up to 150 ms at random before each step, so a test that only passes at one speed shows it. Each run uses a fresh executor with no planner attached, so a model call is impossible. The job also compares the planner's and checker's call counters before and after each run, and treats any call as an internal error. The summary reports the pass rate and the time per run and per step.

If a replay fails, one repair round runs. What it does depends on the failing step:

| The failing step was | The repair |
|---|---|
| A screen assertion or a final assertion | Drop that assertion. It didn't hold on a second run, so it wasn't stable |
| An action | Explore again: relaunch, replay the recorded steps before the failing one without the model, and let the agent continue from there with the turns it has left (at least 5) |
| The launch | Not repairable |

After a repair, the test is written again, validated and replayed again. The report has both replays and says what the repair did. If it still fails, the exit code is 1.

### Trust gates

A test that replays is a cache of the run, and the cache must not lie. After the replays pass, two more checks decide whether the test is **trusted** (row 1.3 of the v4 plan; `--no-gates` skips them, and benchmarks never run them):

- **Wrong path.** On the device, the test runs up to its last tap. It then taps one of the two nearest neighbours of that element (the same kind of element, with real text of its own) and runs to the end. It must fail. If it still passes, its assertions can't tell the screen it should reach from a neighbouring one, for example two plans whose checkouts look the same. Each neighbour gets a fresh run from the launch. A tap with no neighbour is reported as not checked.
- **Goal text.** At least one assertion must name a word of the goal (stop words like "open" and "page" don't count), so the test checks what the goal asked for and not only that some screen showed.

Hand-written tests and tests exported from an agent session (`atalanta export`, the MCP server's `export_session_flow`) were never explored, so explore's gates never ran on them. They can be gated the same way with `atalanta gate FILE --goal "..."`: it replays the file's one test, runs the wrong-path replay and the goal-text rule, and prints `trusted` or `not trusted` with the reasons ([CLI.md](CLI.md#gate)). A test is worth committing as a regression test only once it is trusted.

Each assertion explore writes has also had to fail on the screen before its step (the writer's own rule). The summary prints a `gates:` line: `trusted`, or `not trusted` and why. An untrusted test is still written, so it can be reviewed and fixed, but the exit code is 1 and the JSON report's `gates.trusted` is false. Five passing replays mean "burned in", not "proven stable": 95% confidence needs about 170 reruns (ICST 2021).

## Steering a run: CLI, MCP, console and library

- **CLI.** `--inject-file PATH` feeds the agent lines appended to that file while it runs. `--json` prints the result (`report`, `yaml`, `replay`, `repair`, `files`) on stdout and the live lines on stderr. With `ATALANTA_EVENTS_FILE` set, every event is also appended to that file as a JSON line (the same file and mechanism as `atalanta run`'s live events, `atalanta_common::live_events`), and the replays add their test and step events. Ctrl-C ends the session at its next turn as `stopped`, with a report; a second one exits at once.
- **MCP.** `run_task` with a `goal` runs `atalanta explore … --json --inject-file <task>/inject.txt` as a child: `task_status` shows the turn, op and target from the events file, `task_inject` appends a line to the inject file, `task_stop` is a Ctrl-C, and the summary has the YAML path, the replays and the model's calls and tokens ([MCP.md](MCP.md#background-tasks)).
- **Console.** The Explore panel of `atalanta inspect` starts the same child, streams the events, has an *Inject* box, and shows the YAML with *Save to project* ([CLI.md](CLI.md#inspect)).
- **Library.** `atalanta_explore::ExploreJob` (the whole pipeline) and `ExploreSession` (the loop alone) are the API the CLI uses.
  - **Events.** `with_events(Box<dyn FnMut(&ExploreEvent)>)` receives serde-serializable events tagged `event`: `prefix`, `observed`, `planned`, `rejected`, `acted`, `asserted`, `checked`, `injected`, `note`, `replayRun`, `repair`, `finished`. Each carries `atMs`. The library never prints, so `atalanta mcp`'s stdout stays clean.
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

## Other flags

| Flag | What it does |
|---|---|
| `--device`, `-d` | the device to use (default: the first booted one no other run holds) |
| `--lease-wait` | how long to wait for a device another Atalanta process holds (default 600 s; takes a unit) |
| `--model` | the goal model as `provider:model` (default: the configured provider's goal model, [AI-PROVIDERS.md](AI-PROVIDERS.md)) |
| `--force` | overwrite `--out` if it exists |
| `--after <suite>`, `--after-test <name>` | replay a stored flow (a login) with no model before the first turn; the written test starts with it by name |
| `--no-repair` | don't try a repair round when a replay fails |
| `--dry-run` | explore and report only: no test is written or replayed |

`crates/atalanta-explore/tests/docs.rs` fails when a flag of `atalanta explore` is missing from this page.

## Cost

Exploration costs one goal-model call per turn, plus one checker call per `done(success)` (and per assertion under `checkpoints`). A turn sends the system prompt and the run's stable part (goal, values, apps, demonstrations), both cache-marked on the Anthropic wire and first on every wire so a prefix cache can reuse them; then the current screen list (typically 1 to 3k tokens for a phone screen), the screenshot if any (at most 1,000 px), and the turn's own part (at most 12 history lines, the memo, the notes). It gets about 60 output tokens back. A 10-turn exploration is therefore roughly 20 to 40k input tokens. Pass `--price IN,OUT` to see the estimate (or `IN,OUT,CACHE_READ[,CACHE_WRITE]` for a provider whose cache rates differ), and `--max-cost` to cap it. Replays and CI cost nothing.

`inputTokens` in the report and the `finished` event counts every prompt token; `cacheReadTokens` and `cacheWriteTokens` say how many of them were read from and written to the provider's prompt cache (Anthropic; OpenAI-compatible endpoints report reads only). The estimate bills cache reads at a tenth of the input price and cache writes at 1.25 times it (Anthropic's ratios) unless `--price` names the cache rates.

On an OpenAI-compatible endpoint each request also asks for `max_completion_tokens` 2048, `reasoning_effort: "low"`, `temperature: 0` with a fixed `seed`, and `detail: "low"` on a goal turn's screenshot. An endpoint that rejects one of these by name stops getting it for the rest of the run (`max_completion_tokens` falls back to `max_tokens`), with a warning. A key sent over plain `http://` to a host that isn't this machine is warned about. The budgets are checked before each turn, so a run can end one turn and one final check past `--max-tokens` or `--max-cost`.

## Limits

- **Real-model accuracy is unmeasured.** No API keys were available during development. The prompt, the schema and the three wire formats (Anthropic, OpenAI-compatible, Bedrock) are tested against mock servers, and the loop against scripted plans on a fake app and on real devices. How often a real goal model reaches real goals is not yet known.
- Text input goes only through named values. A goal that needs text you didn't provide makes the model give up and name the missing value.
- One test per goal. Branches, loops and conditionals aren't generated.
- Web works through the same loop (`--platform web`). It was first run live with a real model in the trust-benchmark pilot (2026-11-07, [bench/pilot/](../bench/pilot/README.md)): 6 goals on a small web app, 5 trusted in the final round, and every test failed against the app with its feature broken.
- The volatile-text rules are heuristics. A false negative, such as an unusual date format, shows up as a failed replay, and the repair drops that assertion.
