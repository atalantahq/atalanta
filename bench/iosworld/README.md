# iOSWorld: runs and results (updated 2026-11-10)

[iOSWorld](https://github.com/ljang0/iOSWorld) (CMU, arXiv 2606.09764, Apache-2.0) is the first iOS GUI-agent benchmark with a runnable simulator harness:
- 26 SwiftUI apps sharing one persona's data;
- 133 tasks (`tasks.json`): 27 single-app, 60 multi-app, 46 memory/personalisation;
- macOS, Xcode 26 and an iOS 26 simulator.

It is cloned at `ext/iOSWorld` (commit `c59287b6`). `run.py` here is the adapter that runs each task through `velocity explore --benchmark -p ios` and writes iOSWorld's `task.json`/`trajectory.json`; iOSWorld's own `scripts/judge_trajectories.py` then scores them.

## Results so far

| Run | Tasks | Judge pass rate | Rubric score | Judge that answered |
|---|---|---|---|---|
| `out/single-app-v3` (2026-11-08, 50 steps) | 27 single-app | 19/27 = 70.4% | 0.879 | claude-sonnet-5-5 via the bridge (see below) |
| `out/multi-app` (2026-11-08, 50 steps) | 60 multi-app | 13/60 = 21.7% | 0.640 | claude-sonnet-5-5 via the bridge |
| `out/single-app-v6`, `out/multi-app-v4` (2026-11-08, later build) | 27 + 60 | not yet judged | | |

The paper's best "vision + XML" configuration reports 51% overall and 36% on multi-app tasks.

**Deviations from the paper's setup, which these numbers must carry with them:**
- **The judge was not GPT.** This machine has no OpenAI key. The judge ran through `bench/pilot/claude_bridge.py --map gpt-5.4-mini=claude-sonnet-5-5`, so every rubric call was answered by Claude Sonnet, the same model family as the agent. The `summary.json` files originally named the configured model (`openai/gpt-5.4-mini`); they were corrected on 2026-11-10 and carry a `note`. A score judged by the agent's own model family is not comparable to the paper's.
- The agent model was Claude Sonnet 5.5 (planner) and Claude Haiku 5.5 (checks), also through the bridge, with a 50-step budget and `--values-from-goal/-screen/-screenshots/-model` enabled (see `run.py`).
- The memory/personalisation category (46 tasks) has not been run.

## How it scores

- `scripts/judge_trajectories.py` calls `llm_action_generator.evaluate_trajectory(goal, trajectory, agent_answer, rubric)` for each task directory. Each directory holds `task.json` (goal, status, the agent's final answer) and `trajectory.json` (a list of steps: `screenshot`, `action`, `post_action_screenshot`).
- The judge is a vision model. The paper's setup sends every step's screenshot plus the final one, and scores each rubric criterion. A task passes only when every criterion is met; the reported score is also the fraction met.
- Default judge: OpenAI `gpt-5.4-mini` (`EVAL_PROVIDER`, `EVAL_MODEL`, or `--judge-provider`/`--judge-model`).

## What an official run needs

- An OpenAI key for the judge, as in the paper. Any other judge must be labelled as a deviation.
- The adapter in (2). Run the task's goal with `velocity explore --benchmark -p ios -d <udid> --screenshots always`, against a provider that takes images.
- Report against the paper's "vision + XML" setting, which is how Velocity sees the screen: its best configuration scored 51% overall and 36% on multi-app tasks.
