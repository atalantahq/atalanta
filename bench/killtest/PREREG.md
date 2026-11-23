# Kill test pre-registration (row 1.1, AI-100)

Does `atalanta explore` write tests that hold? The kill test answers this before any launch copy says so. Everything on this page is fixed **before** the first run. `prereg.json` holds the same numbers for `score.py`, and the results are published whether the answer is GO or NO-GO.

**Status:** draft. Set `"frozen": true` in `prereg.json` and commit both files before running. A result scored against an unfrozen pre-registration is not a kill-test result.

## Build

- Atalanta commit: `________` (it must include rows 1.3 gates and 1.5 login; also in `prereg.json` as `build_sha`).
- Model: `________` (`provider:model`). `--price IN,OUT` in USD per million tokens: `____,____`.
- Cost cap per goal: `$____` (`--max-cost`). A goal over the cap fails.

## Apps (owner's choice; at least 3, at least 1 on iOS)

Each app has a real login and a test bypass: a test account with no one-time password, or a demo mode on a backend you can host yourself. AndroidWorld apps don't qualify.

| # | App | Platform | Login and bypass | Release N−1 | Release N |
|---|---|---|---|---|---|
| 1 | | | | | |
| 2 | | | | | |
| 3 | | iOS | | | |

Release pairs are builds that are already published.

## Goals (20, at least 5 behind login)

Write them in `goals.yaml` (see `goals.example.yaml`), one per goal, with its app, whether it needs the login, and its plain-English text. Don't change them after freezing.

## Start state of every goal

A fresh install, then `clearState`, then **`atalanta explore --after <app>/login.yaml`** for login goals (`${NAME}`s from the environment or the keychain), or an `http` step against the app's documented backend. No emulator snapshots.

## What is measured (`score.py`)

| Metric | Definition |
|---|---|
| **S0, burned in** | Share of goals whose test, on day 0, passes 5 of 5 replays, fails the wrong-path replay, and has an assertion that names goal text: explore's `gates.trusted` |
| **S7** | Share of goals whose day-0 test passes 5 of 5 replays on day 7, same build of the app |
| **S_rel** | Share of goals whose day-0 test passes 5 of 5 replays on release N, with no edits |
| **Cost** | Median USD and median planning turns per goal |
| **Login secrecy** | Share of login goals whose model transcript holds none of the secret values |

## GO requires all of

- S0 ≥ 50% (10 of 20);
- S7 ≥ 40%;
- S_rel ≥ 30%;
- median cost ≤ the cap;
- login secrecy = 100%.

These thresholds are frozen with this page. Five of five is "burned in", not "proven stable": 95% confidence needs about 170 reruns (ICST 2021).

**On NO-GO** (decided by 30 July 2027): the fallback headline applies. Atalanta is then one suite for mobile, web and API, a CI Action that absorbs emulator flakiness, and failure evidence agents and reviewers can act on.

## Running and scoring

Each run appends one JSON line to `runs.jsonl` (format in `score.py`). Then:

```bash
SECRET_NAMES=LOGIN_PASSWORD,LOGIN_TOTP python3 score.py runs.jsonl
```

`score.py` reads the values of `SECRET_NAMES` from the environment, looks for them in each login goal's transcript files, and prints the metrics and GO or NO-GO against `prereg.json`.
