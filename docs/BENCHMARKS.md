# Benchmarks

Atalanta's agent, `atalanta explore`, has one published benchmark result: AndroidWorld.

## AndroidWorld

[AndroidWorld](https://github.com/google-research/android_world), from Google Research, is 116 tasks across 20 Android apps on an API 33 emulator. Each task sets up the app, gives the agent a goal in plain English, then checks the device (databases, files, settings, the screen) to decide whether the agent succeeded.

### Result

**59.9%** (69.5 of 116 tasks), run on 2026-10-25 with the agent frozen beforehand.

| | |
|---|---|
| Model | Claude Sonnet 5.5 through the Anthropic API (`anthropic:claude-sonnet-5-5`), effort `low`, a screenshot every turn |
| Atalanta | 0.1.0, release binary sha256 `351f37817604c7a8a7eebd53391133b36bb4832c64f64c461e5c1a364d60c051` |
| Harness | android_world `55ddaf5`, API 33 Google APIs arm64-v8a emulator, Apple Silicon Mac |
| Tasks | seed 30, one combination per task (AndroidWorld's defaults) |
| Time | 57 s per task, setup and check included; 11.8 agent turns per task |
| Model cost | $25.63 in total, about $0.22 per task, at list prices with cache reads counted as full input |

The run was first reported as 60.8%. One task that ran out of turns had been credited as a success; AndroidWorld only credits a task when the agent says it is done. The adapter was fixed and the score corrected the same day.

| Difficulty | Score | Tasks |
|---|---|---|
| Easy | 72.1% | 61 |
| Medium | 52.8% | 36 |
| Hard | 34.2% | 19 |

<details>
<summary>Score by app</summary>

A task is counted under its first app; `multi-app` uses several, `system` none.

| App | Score | Tasks |
|---|---|---|
| audio recorder | 50.0% | 2 |
| broccoli app | 80.0% | 10 |
| camera | 100.0% | 2 |
| chrome | 0.0% | 3 |
| clock | 100.0% | 3 |
| contacts | 50.0% | 2 |
| files | 0.0% | 2 |
| joplin | 75.0% | 4 |
| markor | 54.5% | 11 |
| multi-app | 22.2% | 9 |
| open tracks sports tracker | 50.0% | 6 |
| osmand | 33.3% | 3 |
| pro expense | 100.0% | 7 |
| retro music | 37.5% | 4 |
| settings | 71.4% | 14 |
| simple calendar pro | 70.6% | 17 |
| simple draw pro | 0.0% | 1 |
| simple gallery pro | 100.0% | 1 |
| simple sms messenger | 83.3% | 6 |
| system | 0.0% | 1 |
| tasks | 33.3% | 6 |
| vlc | 50.0% | 2 |

</details>

### Compared with other agents

| Agent | Score | Source |
|---|---|---|
| Human | 80.0% | AndroidWorld paper (arXiv:2405.14573v5), Table 3 |
| Atalanta with Claude Sonnet 5.5 | 59.9% | this run |
| M3A with GPT-4 Turbo, the paper's best agent | 30.6% | AndroidWorld paper, Table 3 |
| Top community leaderboard entries | 99–100% | self-reported |

The leaderboard entries have not been re-run by the AndroidWorld maintainers, and their setups (seeds, retries, emulator image) are not all stated.

### Later runs

The agent was then improved and run again on the same 116 tasks. Complete reruns scored between 72.4% and 81.9%. Because the agent was changed on these tasks, those scores are not held-out results, and no run on a new seed has been done yet. These reruns also reached Claude through the Claude Code CLI rather than the Anthropic API.

### Where it failed

| Cause | Tasks | Examples |
|---|---|---|
| A tap had no visible effect three times in a row, so the run stopped | 13 | the file manager's "Open with" chooser (all Browser and Files tasks), a brightness slider, a drawing canvas |
| Ran out of turns on long, multi-item tasks | ~14 | building playlists, stepping through a calendar day by day |
| Answered from on-screen text, but wrongly | 6 | sports-tracker totals, next week's events |
| The value was only in an image, which the agent cannot read as text | 4 | a receipt, video frames, a recipe photo |
| Said it was done, but the final state was wrong | 3 | a Markor edit, an OsmAnd track |
| Tried to tap an element that was not on screen | 2 | clipboard paste, a calendar header |

### How it runs

A small Python adapter in [`bench/androidworld/`](../bench/androidworld/) runs each task through AndroidWorld's own setup, scoring and teardown. For each task it starts one `atalanta explore --benchmark` process, the same agent that writes tests, with the turn limit set to AndroidWorld's step budget. No YAML is written, nothing is replayed, and every action is the agent's. How the agent works: [EXPLORE.md](EXPLORE.md).

Scoring follows AndroidWorld: a task counts only when the agent says it is done and AndroidWorld's check passes. Each run records its model, seed and binary hash, and per-task tokens, cost and time.

Commands to reproduce the run: [REPRODUCE.md](../bench/androidworld/REPRODUCE.md).

### Limitations

- One run on one seed. Each task scores 0 or 1, so a difference of a few points is within run-to-run variation.
- The arm64 emulator image was used, not the x86_64 image AndroidWorld was developed on.
- Text that exists only in an image can be seen but not typed or given as an answer, by design. Tasks that read values from images lose points.
- Costs are estimates from token counts and list prices.

## Other runs (unofficial)

These runs were done, but their setup differs from the official one, so their numbers can't be compared with published scores. Each one says why.

### iOSWorld (iOS)

[iOSWorld](https://github.com/ljang0/iOSWorld), from CMU, is 133 tasks across 26 iOS apps on an iOS 26 simulator.

| Tasks | Atalanta | Paper's best agent |
|---|---|---|
| Single-app (27) | 19 of 27 (70.4%) | not reported separately |
| Multi-app (60) | 13 of 60 (21.7%) | 36% |
| Overall | not run | 51% |

**Why it's unofficial:** iOSWorld grades each run with a GPT model. No OpenAI key was available, so Claude graded these runs instead, the same model family as the agent. The 46 memory tasks have not been run. Details: [bench/iosworld/README.md](../bench/iosworld/README.md).

### MiniWoB++ (web)

MiniWoB++ is a set of small web tasks: filling forms, clicking buttons, picking dates. Atalanta completed **70.0–71.5% of 130 tasks** across its last three runs.

**Why it's unofficial:** it ran in our own harness ([`bench/miniwob/`](../bench/miniwob/)), with the benchmark's 10-second limit per task removed, and the agent was changed between runs. The figures may not reproduce on the current build.

### Speed against Maestro (Android)

The same 8-step test on the Android Settings app, written once for each tool and run on the same emulator, timed from the moment the command starts until it exits:

| Tool | Time |
|---|---|
| Atalanta | 3.7 s |
| Maestro | 26–27 s |

**Why it's unofficial:** this was one manual run. The script to repeat it, [`bench/head-to-head/`](../bench/head-to-head/), has not been run and recorded yet.
