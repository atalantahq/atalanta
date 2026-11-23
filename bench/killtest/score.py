#!/usr/bin/env python3
"""Score the kill test (row 1.1) against its frozen pre-registration.

Input: runs.jsonl, one JSON object per line:

    {"goal": "shop-01", "login": true, "phase": "day0",
     "trusted": true, "replays": 5, "passed": 5,
     "cost_usd": 0.042, "turns": 9,
     "transcripts": ["runs/shop-01/day0/events.jsonl"]}

- phase "day0": the explore run. `trusted` is its JSON report's
  `gates.trusted`; `replays`/`passed` its replays; cost and turns from its
  report; `transcripts` every file holding what the model was sent or said.
- phase "day7" and "release": `atalanta run` of the day-0 test, 5 times:
  `replays` and `passed` only.

Secrets: SECRET_NAMES (comma-separated) names environment variables whose
values must appear in no login goal's transcripts.

Exit code: 0 GO, 1 NO-GO, 2 the input or the pre-registration is unusable.
"""

import json
import os
import statistics
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent


def load_runs(path):
    runs = []
    for n, line in enumerate(Path(path).read_text().splitlines(), 1):
        line = line.strip()
        if not line:
            continue
        try:
            runs.append(json.loads(line))
        except json.JSONDecodeError as e:
            raise SystemExit(f"{path}:{n}: not JSON: {e}")
    return runs


def secrets_from_env(names):
    values = []
    for name in [n.strip() for n in names.split(",") if n.strip()]:
        v = os.environ.get(name)
        if not v:
            raise SystemExit(f"SECRET_NAMES lists {name}, which is not set")
        values.append(v)
    return values


def leaked(transcripts, secrets, base):
    for t in transcripts or []:
        p = Path(t)
        if not p.is_absolute():
            p = base / p
        try:
            text = p.read_text(errors="replace")
        except OSError:
            # A missing transcript can't be shown clean.
            return True
        if any(s in text for s in secrets):
            return True
    return False


def score(runs, prereg, secrets, base):
    """The metrics and the verdict, as a dict."""
    goals = sorted({r["goal"] for r in runs})
    by = {(r["goal"], r["phase"]): r for r in runs}
    burned = lambda r: r is not None and r.get("replays", 0) > 0 and r.get("passed") == r.get("replays")  # noqa: E731
    s0_goals = [g for g in goals if burned(by.get((g, "day0"))) and by[(g, "day0")].get("trusted")]
    s7_goals = [g for g in goals if burned(by.get((g, "day7")))]
    rel_goals = [g for g in goals if burned(by.get((g, "release")))]
    day0 = [by[(g, "day0")] for g in goals if (g, "day0") in by]
    costs = [r.get("cost_usd") for r in day0 if r.get("cost_usd") is not None]
    turns = [r.get("turns") for r in day0 if r.get("turns") is not None]
    login = [r for r in day0 if r.get("login")]
    clean = [r for r in login if not leaked(r.get("transcripts"), secrets, base)]
    n = len(goals)
    m = {
        "goals": n,
        "login_goals": len(login),
        "s0": len(s0_goals) / n if n else 0.0,
        "s7": len(s7_goals) / n if n else 0.0,
        "s_rel": len(rel_goals) / n if n else 0.0,
        "median_cost_usd": statistics.median(costs) if costs else None,
        "median_turns": statistics.median(turns) if turns else None,
        "login_secrecy": len(clean) / len(login) if login else 0.0,
    }
    t = prereg["thresholds"]
    cap = prereg.get("cost_cap_usd_per_goal")
    checks = {
        "s0": m["s0"] >= t["s0_min"],
        "s7": m["s7"] >= t["s7_min"],
        "s_rel": m["s_rel"] >= t["s_rel_min"],
        "cost": cap is not None and m["median_cost_usd"] is not None and m["median_cost_usd"] <= cap,
        "login_secrecy": login != [] and m["login_secrecy"] >= t["login_secrecy"],
    }
    problems = []
    if not prereg.get("frozen"):
        problems.append("prereg.json is not frozen: this is not a kill-test result")
    if n < prereg.get("goals_min", 20):
        problems.append(f"{n} goals, fewer than the {prereg.get('goals_min')} pre-registered")
    if len(login) < prereg.get("login_goals_min", 5):
        problems.append(f"{len(login)} login goals, fewer than {prereg.get('login_goals_min')}")
    verdict = "GO" if all(checks.values()) and not problems else "NO-GO"
    return {"metrics": m, "checks": checks, "problems": problems, "verdict": verdict}


def main(argv):
    if len(argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    prereg = json.loads((HERE / "prereg.json").read_text())
    runs = load_runs(argv[1])
    secrets = secrets_from_env(os.environ.get("SECRET_NAMES", ""))
    out = score(runs, prereg, secrets, Path(argv[1]).resolve().parent)
    print(json.dumps(out, indent=2))
    return 0 if out["verdict"] == "GO" else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
