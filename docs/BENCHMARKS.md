# Benchmarks

## AndroidWorld

[AndroidWorld](https://github.com/google-research/android_world) (Google
Research) is 116 parameterised tasks across 20 real Android apps on an API 33
emulator. Each task sets up app state, gives the agent a goal in English, and
then checks the device itself (databases, files, settings, the screen) to
decide success; question tasks check the agent's answer.

Atalanta runs it with `atalanta explore --benchmark`, the same agent loop that
writes tests, driven by a small Python adapter in
[`bench/androidworld/`](../bench/androidworld/). Exact commands:
[REPRODUCE.md](../bench/androidworld/REPRODUCE.md).

### How to read these numbers (read first)

**Every AndroidWorld number here comes from the same 116 task instances (task seed 30, one combination each).** The 59.9% run below was the first full run, with the agent frozen before it started. After it, the agent was changed and run again on the same instances seven more times (2026-11-07 and 2026-11-08), five of them in full. The five complete reruns each used a different Atalanta binary: between runs the goal prompt went from `goal-2` to `goal-6`, the Android tree-server protocol changed several times (17 by the last run), and fixes were made for failures seen in the previous run. Changing the agent between runs on the same tasks is tuning on the test set. **The later scores are therefore not held-out results, and no run on another seed has been done.** A fair headline is: "59.9% on the first frozen run; 78.4% after eight runs of iteration on the same seed, not held out."

| Run | Date | Tasks run | Score | Model route |
|---|---|---|---|---|
| first frozen run (below) | 2026-10-25 | 116 | **59.9%** | Anthropic API, `anthropic:claude-sonnet-5-5` |
| `full-v2` | 2026-11-07 | 116 | 77.6% | Claude Code CLI bridge |
| `full-v3` | 2026-11-08 | 116 | 81.9% | bridge |
| `full-v4` | 2026-11-08 | 116 | 81.9% | bridge |
| `full-v5` | 2026-11-08 | 28 (stopped) | 71.4% of 28 | bridge |
| `full-v6` | 2026-11-08 | 27 (stopped) | no summary | bridge |
| `full-v7` | 2026-11-08 | 116 | 72.4% | bridge |
| `full-v8` | 2026-11-08 | 116 | 78.4% | bridge |

Each run directory (written under `bench/androidworld/runs/`, which is not kept in the repository) holds `run.json` (the run's model, seed and binary SHA-256) and `summary.md` (the per-app tables). Two more things the later runs carry with them:

- **Model route.** From `full-v2` on, the model was `openai-compatible:claude-sonnet-5-5`: an OpenAI-compatible endpoint backed by the Claude Code CLI (`bench/pilot/claude_bridge.py`), not the Anthropic API. Effort, caching and token accounting differ from a direct API call.
- **The three Browser tasks** scored 0 in every full run but `full-v2` (1 of 3). Browser-only reruns (`browser-v8` to `browser-v10`) looked into why; the tree-server change made for it (protocol 16) works on API 34 and later only, so it does nothing on this API 33 emulator ([STATUS.md](STATUS.md), the protocol 16 row).

### Result

**59.9% (69.5 / 116)** with `anthropic:claude-sonnet-5-5`, 2026-10-25: the first full run, with the agent frozen beforehand. One run of all 116 tasks, task seed 30, one combination per task, no exceptions left out (AndroidWorld's convention and the strict rate agree). One task scored 0.5.

**Correction (2026-10-25):** this run was first reported as 60.8% (70.5 / 116). The adapter answered `done=True` to AndroidWorld's second `step()` call even when the agent had not said done, so `TurnOffWifiAndTurnOnBluetooth#0`, which ran out of turns (`agent_done: false`), was credited 1.0. AndroidWorld credits a task only when the agent says done (`suite_utils._run_task`). The adapter now repeats the agent's verdict, and `run.py summarize` applies the rule to existing rows (the raw `results.jsonl` is unchanged). No other row was affected. This is our own run: results, trajectories and the exact binary hash are in the run directory and can be re-run with [REPRODUCE.md](../bench/androidworld/REPRODUCE.md).

| | |
|---|---|
| Model | `anthropic:claude-sonnet-5-5`, effort `low`, screenshot every turn, `--verify final` |
| Atalanta | 0.1.0, release binary sha256 `351f37817604c7a8a7eebd53391133b36bb4832c64f64c461e5c1a364d60c051`, goal prompt `goal-2` (frozen before the run) |
| Harness | google-research/android_world `55ddaf5edc6f6c25673aed343907d5b9dca66f84`, AndroidWorldAvd (API 33 Google APIs arm64-v8a), Apple Silicon Mac |
| Outcomes (Atalanta's) | success 63, gave up 31, stuck 13, unverified 6, out of turns 3 (one of them wrongly credited at first; see the correction) |
| Per task | 11.8 planning turns, 4.2 s per turn, 57 s wall clock (setup and check included), 12.5 model calls |
| Tokens and cost | 11.86 M input, 0.19 M output, est. **$25.63** for the scored rows (≈ $0.22 per task; list prices $2 / $10 per M, cache reads counted at the full input price, so an upper bound) |

By difficulty (AndroidWorld's tags):

| difficulty | official | strict | n |
|---|---|---|---|
| easy | 72.1% | 72.1% | 61 |
| hard | 34.2% | 34.2% | 19 |
| medium | 52.8% | 52.8% | 36 |

By app (a task's first app; `multi-app` when it uses several, `system` for none):

| app | official | strict | n |
|---|---|---|---|
| audio recorder | 50.0% | 50.0% | 2 |
| broccoli app | 80.0% | 80.0% | 10 |
| camera | 100.0% | 100.0% | 2 |
| chrome | 0.0% | 0.0% | 3 |
| clock | 100.0% | 100.0% | 3 |
| contacts | 50.0% | 50.0% | 2 |
| files | 0.0% | 0.0% | 2 |
| joplin | 75.0% | 75.0% | 4 |
| markor | 54.5% | 54.5% | 11 |
| multi-app | 22.2% | 22.2% | 9 |
| open tracks sports tracker | 50.0% | 50.0% | 6 |
| osmand | 33.3% | 33.3% | 3 |
| pro expense | 100.0% | 100.0% | 7 |
| retro music | 37.5% | 37.5% | 4 |
| settings | 71.4% | 71.4% | 14 |
| simple calendar pro | 70.6% | 70.6% | 17 |
| simple draw pro | 0.0% | 0.0% | 1 |
| simple gallery pro | 100.0% | 100.0% | 1 |
| simple sms messenger | 83.3% | 83.3% | 6 |
| system | 0.0% | 0.0% | 1 |
| tasks | 33.3% | 33.3% | 6 |
| vlc | 50.0% | 50.0% | 2 |

Against published numbers:

| Agent | AndroidWorld success | Source |
|---|---|---|
| Atalanta `explore --benchmark` + Claude Sonnet 5.5 (this run, first frozen run) | 59.9% | our run; trajectories and per-task results published with the run |
| Atalanta, after eight runs of iteration on the same seed | 78.4% | our runs; **not held out** (see [How to read these numbers](#how-to-read-these-numbers-read-first)) |
| Google ARTEMIS | 99.1% | **self-reported** (community leaderboard sheet) |
| Top community leaderboard entries | 99-100% | **self-reported** |
| M3A, accessibility tree, GPT-4 Turbo (the paper's best agent) | 30.6% | AndroidWorld paper, Rawles et al., arXiv:2405.14573v5, Table 3 |
| Human | 80.0% | the same paper, Table 3 |

The self-reported entries were not re-run by the AndroidWorld maintainers or by us, and their protocols (seeds, combinations, retries, emulator image) are not all stated; read the comparison with that in mind.

#### Where it failed (45.5 of 116)

| Category | Instances | Examples |
|---|---|---|
| A tap whose effect Atalanta couldn't see, 3 times in a row: the run stops as `stuck` | 13 | the file manager's "Open with" chooser (all 3 Browser tasks, both Files tasks), brightness slider, clipboard toolbar, Simple Draw canvas |
| Out of turns on long multi-item tasks | ~14 | playlists (Retro, VLC), renaming a Markor note, stepping a calendar day by day, recipes from a Markor file |
| An answer grounded in shown text but wrong | 6 | sports-tracker totals and counts, task lists, next week's events |
| Values only in pixels (refused by grounding, by design) | 4 | receipt image, video frames, recipe photo, expenses in a gallery image |
| `done`, but the end state was wrong | 3 | Markor edit and merge, OsmAnd track |
| The model wanted to tap something not in the list (refused 3 times) | 2 | clipboard paste, calendar header |

What would move the number most (not applied to this run, whose agent was frozen; later runs on the same tasks did change the agent, see above): a tap at a point inside a listed element for custom views and system choosers, a stuck check that compares screenshots as well as the tree, and date navigation helpers.

#### Incidents during the run, and the integrity audit

- **Harness hang (11:48).** After FilesDeleteFile's episode ended, AndroidWorld's own check/teardown hung: an adb child android_env forked deadlocked in gRPC's fork handler before exec (that child, with the runner's command line, is what looked like a second runner). The runner was stopped; the run directory was audited (23 rows, no duplicate instances, no overlapping episode time spans, every events file with turns 1..n and exactly one `finished`, every task started once in the log): **no task was affected**. FilesDeleteFile and MarkorAddNoteHeader (stopped mid-episode), which had no result, ran again from their own initialisation, as AndroidWorld's checkpointer does for unfinished instances. Added afterwards (harness only; agent, prompt and binary unchanged): a per-instance watchdog that turns a hang outside the agent's episode into an exception, `run.lock` so a second runner refuses to start, `GRPC_ENABLE_FORK_SUPPORT=0`, a stop after 3 exceptions in a row, and exception instances run again on resume.
- **Disk disconnect and host reboot.** The 44 rows written were intact; the emulator was cold-booted with the same flags; OsmAndTrack, in flight, had no row and ran again.
- **One exception.** RecipeDeleteDuplicateRecipes3 hit "model API unreachable" during the host incident; it ran again on resume (a second disk disconnect interrupted the first re-run, which left no row) and scored 0.
- Every resume ran with the same binary (checked by its SHA-256, which `run.py` refuses to change within a run directory), model, seed and prompt. Each instance initialises its own task state, so results recorded before a pause are independent of it. Spend outside the scored rows (validation ≈ $2.2, attempts cut short by the incidents ≈ $1.5) brings the total to about $29.

### How the adapter works

- `atalanta_agent.py` is an AndroidWorld agent. For each episode it runs one
  `atalanta explore --benchmark --json` child process with Atalanta's turn
  budget set to AndroidWorld's step budget, and the events (the trajectory)
  written to `episodes/<task>/events.jsonl`. A grounded answer is passed to
  AndroidWorld's own answer channel (`env.execute_action(answer)`).
  A process per episode rather than a long-lived `atalanta mcp` session: the
  agent loop already lives in `explore`, and MCP would only wrap the same
  child; a process per episode isolates tasks and gives a hard kill.
- `run.py` builds the suite with AndroidWorld's `create_suite`, runs each
  instance through AndroidWorld's own `suite_utils._run_task` (initialise,
  episode, success check, teardown), appends a line to `results.jsonl` at
  once (resumable), and writes `summary.json` / `summary.md`.
- No YAML is written and nothing is replayed. Dialogs are not auto-dismissed:
  every action is the agent's.

### What is measured, and how it matches AndroidWorld's scoring

| | AndroidWorld (`run.py`, `suite_utils`) | Here |
|---|---|---|
| Tasks | `android_world` family, 116 templates | the same |
| Parameters | `create_suite(seed=30, n_task_combinations=1)` (its defaults) | the same |
| Step budget | `int(10 * complexity)` agent steps | the same number of Atalanta planning turns (a turn that code refused, an answer and the final `done` each count) |
| Success | the agent said done **and** the task's check passes | the same: done = Atalanta ended by its own decision (`success`, `unverified`, `gaveUp`); budget, time, stuck and errors are not done |
| Headline | mean over templates of each template's mean, instances that raised left out (`process_episodes`) | the same, plus a strict rate where exceptions count as failures |

The model, provider, list prices, Atalanta version and binary SHA-256, the
harness commit, seed, combinations, effort and prompt version are in each
run's `run.json`; tokens, cost (list prices, cache reads counted at the full
input price, so an upper bound), steps and time per task are in
`results.jsonl`.

### The agent

- **The model picks; code decides.** Each turn the model returns one
  operation and an element number from the numbered screen list (plus a
  screenshot every turn); code builds the selector, runs the action through
  the executor, and refuses anything that doesn't check out.
- **Ops**: tap, longPress, inputText, clearText, scroll and swipe (whole
  screen or inside a named list, chip row or picker), back, home, enter,
  openApp (by name, from AndroidWorld's app names), wait, answer, done.
- **Values from the goal and screens** (`--values-from-goal --values-from-screen`): typed text is accepted only
  when it occurs in the goal or on a screen seen in the run, is a number, or
  joins such pieces ([EXPLORE.md](EXPLORE.md#--values-from-goal-and---values-from-screen-off-by-default)).
- **Answers**: accepted only when grounded in text shown during the run, a
  number, a yes/no, or words seen plus calendar/unit words
  ([EXPLORE.md](EXPLORE.md#answers-answer)).
- **Screen list**: element states (checked/unchecked, selected, focused,
  disabled), empty fields' hints, and `cell=row,col` for unlabeled grid cells
  (calendar days); a `memo` the model keeps across turns; a one-line history.
- **Self-check before done**: the checker model must cite real elements
  showing the end state, given the run's own action log; two failed checks
  end the run as `unverified` (still "done" for AndroidWorld).

### Caveats

- **Leaderboard numbers are self-reported.** AndroidWorld's community
  leaderboard is a spreadsheet of submitted results; protocols differ
  (seeds, combinations, retries, which emulator image, human help in
  setup), and none of the top entries were re-run by the maintainers.
  Google ARTEMIS reports 99.1%; the top entries report 99-100%. The
  AndroidWorld paper's best agent scored 30.6% and humans 80.0%.
- One run, one seed, one combination per task: per-task results are noisy
  (a task is 0 or 1), so differences of a few points are within run-to-run
  variation.
- Apple Silicon: the arm64-v8a API 33 Google APIs image, not the x86_64
  image AndroidWorld was developed on; apps come from AndroidWorld's own
  setup (VLC needed its arm64 APK, which the setup picks itself).
- Text that exists only as pixels (an image, a custom-drawn calendar
  grid, a video frame) can be seen in the screenshot but can't be typed or
  answered: grounding refuses it. Tasks that read values from images
  (receipts, recipe photos, video frames) lose points by design.
- Costs are estimates from token counts and list prices.

### Harness notes (this machine)

- Another emulator held gRPC 8554, so AndroidWorld's AVD ran on console 5556
  / gRPC 8556; the scripts pass both ports to AndroidWorld's `env_launcher`.
- `GRPC_VERBOSITY=ERROR` must be set before gRPC loads (AndroidWorld's
  `run.py` does it; `aw_env.py` does too): otherwise gRPC's fork-time log
  lines end up in the adb output android_env parses, and app-snapshot
  restores fail at random.
- On a failed adb call android_env runs `adb kill-server`; `aw_env.py`
  disables only that restart (the retries stay) so other devices on the
  shared adb server keep their connections (`AW_ALLOW_ADB_RESTART=1` restores
  it).
- Atalanta's tree server must not suppress accessibility services, or
  AndroidWorld's accessibility forwarder (which many success checks read)
  loses its tree for the rest of the run. The adapter sets
  `ATALANTA_ANDROID_KEEP_A11Y=1`; Atalanta's `--keep-a11y` connection was
  fixed for this (it had silently fallen back to the suppressing connect on
  API 33 and 34).
- android_env's accessibility forwarder sometimes crashes when the env
  connects, leaving a "keeps stopping" system dialog; the adapter closes that
  one dialog (only that one) before an episode.

## Other benchmark runs in the tree (not published results)

- **MiniWoB++ (web)**, `bench/miniwob/`: 70.0–71.5% of 130 tasks across the last three runs (`out/v3`, `v4`, `v5`), with earlier runs lower. This is our own harness (`bench/miniwob/run.py`): the 10 s episode limit is lifted, `--values-from-screen` and `--values-from-screenshots` are on, and the agent changed between runs. It is not comparable to published BrowserGym or MiniWoB++ numbers. These runs predate the screenshot gate: text typed under `--values-from-screenshots` was then accepted unverified, after any screenshot. It is now accepted only after a screenshot of a screen with pixel-only content, so the figures may not reproduce on the current build.
- **iOSWorld**, `bench/iosworld/`: judged by Claude Sonnet through the bridge, not by the paper's GPT judge, so the agent and the judge are the same model family. The deviations are listed in [bench/iosworld/README.md](../bench/iosworld/README.md); the numbers are not comparable to the paper's.
