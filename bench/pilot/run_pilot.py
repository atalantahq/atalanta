#!/usr/bin/env python3
"""Pilot of the trust benchmark: one lane (web, android or ios) of goals.json.

For each goal: `velocity explore` with a real model writes a test (5 replays,
the wrong-path gate, the goal-text gate), and its result is appended to
out/runs.jsonl in score.py's format plus pilot fields. For web goals the
written test is then replayed 3 times against the app with that goal's
feature broken (app/bug.js): a pass there is a false pass.

    python3 run_pilot.py web --device web
    python3 run_pilot.py android --device emulator-5554
    python3 run_pilot.py ios --device <udid>

Needs the app server (python3 -m http.server 8790 --directory app) and the
model bridge (claude_bridge.py) or any provider in the environment.
"""

import argparse
import json
import os
import subprocess
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
VELOCITY = os.environ.get("VELOCITY_BIN", str(HERE.parents[1] / "target" / "debug" / "velocity"))
OUT = HERE / "out"
LOCK = threading.Lock()
MUTANT_REPLAYS = 3


def set_bug(name):
    (HERE / "app" / "bug.js").write_text(f'window.BUG = "{name}";\n')


def append(row):
    with LOCK, open(OUT / "runs.jsonl", "a") as f:
        f.write(json.dumps(row) + "\n")


def explore(lane, app_id, device, g):
    d = OUT / g["id"]
    d.mkdir(parents=True, exist_ok=True)
    test = d / "test.yaml"
    env = dict(os.environ, VELOCITY_EVENTS_FILE=str(d / "events.jsonl"))
    cmd = [VELOCITY, "explore", g["goal"], "--app-id", app_id, "-p", lane, "-d", device,
           "--out", str(test), "--force", "--json", "--no-demos",
           "--price", os.environ.get("PILOT_PRICE", "2,10"), "--max-cost", "1.5"]
    for n in g.get("data", []):
        cmd += ["--data", f"{n}={os.environ[n]}"]
    for n in g.get("secrets", []):
        cmd += ["--secret", n]
    started = time.time()
    # Argument list, no shell; the values come from this script's own flags and the developer's env.
    # nosemgrep: python.lang.security.audit.dangerous-subprocess-use-tainted-env-args.dangerous-subprocess-use-tainted-env-args
    p = subprocess.run(cmd, capture_output=True, text=True, env=env, timeout=1200)
    (d / "live.log").write_text(p.stderr)
    (d / "report.json").write_text(p.stdout)
    try:
        r = json.loads(p.stdout)
    except json.JSONDecodeError:
        r = {}
    rep, gates = r.get("report") or {}, r.get("gates") or {}
    return {
        "goal": g["id"], "lane": lane, "login": bool(g.get("login")), "phase": "day0",
        "exit": p.returncode, "outcome": rep.get("outcome"),
        "trusted": bool(gates.get("trusted")),
        "replays": gates.get("replays", 0), "passed": gates.get("replaysPassed", 0),
        "wrong_path": (gates.get("wrongPath") or {}).get("result"),
        "gate_reasons": gates.get("reasons"),
        "cost_usd": rep.get("costUsd"), "turns": rep.get("modelCalls"),
        "seconds": round(time.time() - started, 1),
        "test_written": test.exists(),
        "transcripts": [str((d / "events.jsonl").relative_to(HERE))],
    }


def mutant(g, test):
    """Replays of the test against the app with g's feature broken."""
    set_bug(g["bug"])
    try:
        passes = 0
        for _ in range(MUTANT_REPLAYS):
            # Argument list, no shell; the values come from this script's own flags and the developer's env.
            # nosemgrep: python.lang.security.audit.dangerous-subprocess-use-tainted-env-args.dangerous-subprocess-use-tainted-env-args
            p = subprocess.run([VELOCITY, "run", str(test), "-p", "web"],
                               capture_output=True, text=True, timeout=300, cwd=test.parent)
            passes += int(p.returncode == 0)
        return passes
    finally:
        set_bug("none")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("lane", choices=["web", "android", "ios"])
    ap.add_argument("--device", required=True)
    ap.add_argument("--only")
    a = ap.parse_args()
    spec = json.loads((HERE / "goals.json").read_text())[a.lane]
    OUT.mkdir(exist_ok=True)
    for g in spec["goals"]:
        if a.only and g["id"] != a.only:
            continue
        row = explore(a.lane, spec["app_id"], a.device, g)
        if a.lane == "web" and row["test_written"]:
            row["mutant"] = g["bug"]
            row["mutant_replays"] = MUTANT_REPLAYS
            row["mutant_passes"] = mutant(g, OUT / g["id"] / "test.yaml")
        append(row)
        print(json.dumps({k: row.get(k) for k in
                          ["goal", "outcome", "trusted", "passed", "wrong_path", "mutant_passes",
                           "cost_usd", "seconds"]}), flush=True)


if __name__ == "__main__":
    main()
