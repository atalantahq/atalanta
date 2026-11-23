# Trust benchmark pilot (2026-11-07)

A small run of the trust benchmark with a real model. It checks the harness end to end and finds what breaks before the frozen kill test ([../killtest/PREREG.md](../killtest/PREREG.md)).

**This is not the kill test.** The kill test needs at least 3 real apps (1 on iOS) with real logins, 20 pre-registered goals and published release pairs, all frozen before the first run. This pilot uses:
- a 6-feature web app written for it;
- the Android and iOS Settings apps;
- 11 goals chosen on the day;
- no day-7 or release-N rerun.

Its numbers measure the harness and the gates. They don't measure Atalanta on real apps.

## Setup

| | |
|---|---|
| Atalanta | this tree, debug build, 2026-11-07 (rounds 1–4 differ only by the fixes listed below) |
| Model | `claude-sonnet-5-5` (goal), `claude-haiku-5-5` (steps), through `claude_bridge.py`: an OpenAI-compatible endpoint over the Claude Code CLI (`claude -p`, no tools, no project settings). Text only (`ATALANTA_NL_IMAGES=0`). |
| Devices | Android emulator (API 34, arm64), iOS 26.0.1 simulator (iPhone 17), headless Chrome |
| Apps | `app/` (Pilot Shop: sign-in, plans, cart, search, settings), `com.android.settings`, `com.apple.Preferences` |
| Goals | `goals.json`: 6 web (1 behind a login), 3 Android, 2 iOS |
| Seeded regressions | each web goal has one: `app/bug.js` set to `plan`, `cart`, `search`, `dark`, `signin` or `price` breaks that feature only |
| Per goal | `atalanta explore` with 5 replays, the wrong-path gate and the goal-text gate (explore's defaults). For web goals, the written test then gets 3 replays against the broken app: any pass is a false pass. |
| Cost | the CLI's list-price figure per call. Explore's own estimate used `--price 3,15`. |

The three lanes (web, Android, iOS) ran in parallel, one device each.

## Results

| Round | S0: trusted, 5/5 | Wrong-path gate ran | Seeded regressions caught | False passes (trusted, passed on a broken app) | Login secret in model traffic | Median cost per goal |
|---|---|---|---|---|---|---|
| 1 | 7/11 | 6/11 | 4/5 (one test wasn't written) | **1** | 0 of 67 calls | $0.086 |
| 2 | 6/11 | 8/11 | 6/6 | 0 | 0 of 51 calls | $0.081 |
| 3 | 9/11 | 5/11 | 6/6 | 0 | 0 of 51 calls | $0.081 |
| 4 (final) | **10/11** | **10/11** | **6/6** | **0** | 0 of 49 calls | $0.078 |

Round 4, per goal (`results/round4.jsonl`):

| goal | trusted | replays | wrong path | broken app passed | cost $ |
|---|---|---|---|---|---|
| a-apps | yes | 5/5 | rejected | — | 0.075 |
| a-display | yes | 5/5 | rejected | — | 0.077 |
| a-internet | yes | 5/5 | rejected | — | 0.078 |
| i-about | yes | 5/5 | rejected | — | 0.082 |
| i-camera | yes | 5/5 | rejected | — | 0.086 |
| w-cart | yes | 5/5 | rejected | 0/3 | 0.068 |
| w-dark | yes | 5/5 | skipped (a checkbox with no comparable neighbour) | 0/3 | 0.067 |
| w-login | **no** (goal-text gate) | 5/5 | rejected | 0/3 | 0.112 |
| w-plan | yes | 5/5 | rejected | 0/3 | 0.109 |
| w-price | yes | 5/5 | rejected | 0/3 | 0.069 |
| w-search | yes | 5/5 | rejected | 0/3 | 0.085 |

Each round's numbers are after that round's fixes only. The table is not one build measured four times.

## What it found (and what was fixed)

Each item was found by a pilot round and fixed before the next. The CHANGELOG has them under "Fixed".

1. **Round 1: a trusted false pass.** w-price ("check that the yearly plan costs $99") passed 5/5 replays and the wrong-path gate, then passed 3/3 replays against the app showing $109.
   - Cause: the model's assertion fell back to `assertVisible: { id: price-yearly }`, which only checks that the element exists, because `$99` was refused as "volatile".
   - Fix: a value the goal names may be asserted. The test is now `assertVisible: { text: $99 }` and fails on the broken app (`results/tests/w-price.round1.yaml` vs `w-price.yaml`).
2. **Round 1: multi-step goals were never verified.** The final AI check saw only the last screen, so "open Network & internet and then Internet" was `unverified`: that screen can't show the first step.
   - Fix: it now judges the end state with the runner's log of what was done (benchmark runs already did this).
3. **Round 1: the wrong-path gate never ran on iOS.** Cells have a label and no text, and the neighbour filter compared text only (`None == None`).
4. **Rounds 1–2: the wrong-path gate made false accusations.** On Android it picked the tapped row's own summary line ("34 apps installed" under "All apps"), which opens the same screen. It then called sound tests weak: a-apps, a-display and a-internet.
   - Round 3's fix excluded a row's stacked lines but then found no neighbour at all (skipped on 5 goals).
   - Round 4 also searches up to five levels, because an Android title sits two levels inside its row.
   - **The gate never caught a genuinely weak test in this pilot.** Every "wrong path passed" in rounds 1–2 was this bug.
5. **Web element ids were treated as labels** (from the smoke run before round 1). `<button id="plan-go">Continue</button>` was written as `tap: { text: plan-go }`, which the page never matches. `Element.dom_id` now carries the id, and the test writes `tap: { id: plan-go }`.
6. **Web `aria-label`s were written as `text:`**, which the page matches against visible text only. Such taps waited out a 10 s timeout. Then the gate's own lookup failed and crashed explore with no report (w-cart, round 1).
   - Now written as `accessibilityId:`. A failed gate lookup is reported as "not checked".
7. **A web page's `<body>` was asserted.** Its text joins the whole page, so the assertion failed every replay. Three pickers (the MCP recorder, explore's screen assertions, its expectation assertions) now skip containers whose text joins their children's.

## Open

- **w-login stays untrusted.**
  - Its assertions ("Welcome back", `id: who`) name nothing from "sign in with the test account", so the goal-text gate refuses it.
  - The test is sound (it failed every replay on the broken sign-in). The heuristic is strict here, and was left strict.
- **The gate is skipped for an element with no comparable neighbour** (w-dark's checkbox). A skipped gate still counts as trusted, as `gates.rs` decides; the kill test should report how many trusted tests had the gate skipped.
- **The wrong-path gate was never shown catching a weak test here.** The seeded regressions are what showed the tests catch breakage. A pilot set with deliberately weak goals would measure the gate itself.
- **Radio buttons on the web** (`<input type="radio">` inside a `<label>`, no id) are written as `className: input, index: 0`, which the browser doesn't resolve. The model recovered by tapping the label text the next turn.
- **Not covered:** real devices, day-7 and release-N stability, apps with real backends, other models, screenshots to the checker.

## Reproduce

```bash
cd bench/pilot
python3 -m http.server 8790 --bind 127.0.0.1 --directory app &
python3 claude_bridge.py --port 8787 --log out/calls.jsonl &      # or any provider: docs/AI-PROVIDERS.md
export ATALANTA_NL_PROVIDER=openai-compatible ATALANTA_NL_API_URL=http://127.0.0.1:8787/v1 \
  ATALANTA_NL_MODEL=claude-haiku-5-5 ATALANTA_NL_EXPLORE_MODEL=claude-sonnet-5-5 ATALANTA_NL_IMAGES=0 \
  PILOT_EMAIL=pilot@example.com PILOT_PASSWORD=correct-horse-7 ATALANTA_SECRETS=PILOT_PASSWORD ATALANTA_KEEP_WDA=1
python3 run_pilot.py web --device web &
python3 run_pilot.py android --device emulator-5554 &
python3 run_pilot.py ios --device <simulator udid> &
wait; python3 summarize.py out/runs.jsonl out/calls.jsonl
```

The test password is the pilot app's own fixture, not a real credential.
