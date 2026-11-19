#!/usr/bin/env python3
"""iOSWorld tasks through Velocity's agent, written in iOSWorld's run format.

For each task in iOSWorld's `tasks.json`:
- reset the task's apps as iOSWorld does between tasks: terminate them, wipe
  their data containers and the workspace suite's shared app-group
  container, so each app re-seeds the persona's data on launch (the same
  directories as iOSWorld's `reset_app_data`, reimplemented here);
- run `velocity explore --benchmark -p ios` with the task's goal, a
  screenshot every turn (`--screenshots always`), and each turn's
  screenshot saved (`VELOCITY_EXPLORE_SHOTS_DIR`);
- write `<run>/<task>/task.json` (goal, status, the agent's answer) and
  `trajectory.json` (one step per turn: the screenshot before it, the
  action, the next screenshot after) for iOSWorld's own judge,
  `scripts/judge_trajectories.py`, which this harness does not run.

    python3 run.py --udid <sim> --out out/run1 [--category single_app] [--tasks a,b] [--limit N]

The simulator must have been bootstrapped by iOSWorld
(`iphone/bootstrap/bootstrap_ios_apps.sh`), which writes the app manifest.
"""

import argparse
import json
import os
import plistlib
import shutil
import subprocess
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
IOSW = ROOT / "ext" / "iOSWorld"
V = os.environ.get("VELOCITY_BIN", str(ROOT / "target" / "debug" / "velocity"))
MANIFEST = IOSW / "iphone" / "bootstrap" / ".app_manifest.json"
ALIASES = {"letterboxd": "cinephile", "whatsapp": "quickchat", "linkedin": "lockedin"}
DATA_DIRS = ["Library/Preferences", "Library/Application Support", "Library/Caches",
             "Library/Cookies", "Library/SplashBoard", "Library/Saved Application State",
             "Library/WebKit", "Documents", "tmp"]
SHARED_GROUPS = {"group.com.iosworld.benchmark.workspacesuite"}


def sh(*cmd, check=False):
    return subprocess.run(list(cmd), capture_output=True, text=True, check=check)


def bundle_of(app, manifest):
    e = manifest.get(ALIASES.get(app, app)) or manifest.get(app) or {}
    return e.get("bundle_id")


def wipe(path):
    if not os.path.isdir(path):
        return
    for entry in os.listdir(path):
        full = os.path.join(path, entry)
        try:
            shutil.rmtree(full) if os.path.isdir(full) else os.remove(full)
        except OSError:
            pass


def reset_apps(udid, apps, manifest):
    bundles = [b for b in (bundle_of(a, manifest) for a in apps) if b]
    for b in bundles:
        sh("xcrun", "simctl", "terminate", udid, b)
    # The shared app-group container of the workspace suite.
    data = Path.home() / "Library/Developer/CoreSimulator/Devices" / udid / "data"
    groups = data / "Containers" / "Shared" / "AppGroup"
    if groups.is_dir():
        for d in groups.iterdir():
            meta = d / ".com.apple.mobile_container_manager.metadata.plist"
            try:
                gid = plistlib.loads(meta.read_bytes()).get("MCMMetadataIdentifier", "")
            except (OSError, plistlib.InvalidFileException, ValueError):
                continue
            if gid in SHARED_GROUPS:
                for entry in d.iterdir():
                    if not entry.name.startswith("."):
                        shutil.rmtree(entry, ignore_errors=True) if entry.is_dir() else entry.unlink(missing_ok=True)
    for b in bundles:
        c = sh("xcrun", "simctl", "get_app_container", udid, b, "data").stdout.strip()
        for rel in DATA_DIRS:
            wipe(os.path.join(c, rel))
    sh("xcrun", "simctl", "launch", udid, "com.apple.springboard")  # home screen
    sh("xcrun", "simctl", "ui", udid, "appearance", "light")


def run_task(task, udid, manifest, out, a):
    d = out / task["name"]
    if d.exists():
        shutil.rmtree(d)
    (d / "steps").mkdir(parents=True)
    reset_apps(udid, task.get("apps", []), manifest)
    first_app = bundle_of((task.get("apps") or [""])[0], manifest) or "com.apple.springboard"
    apps_file = d / "apps.json"
    apps_file.write_text(json.dumps({
        a_: bundle_of(a_, manifest) for a_ in manifest if bundle_of(a_, manifest)}))
    env = dict(os.environ, VELOCITY_EVENTS_FILE=str(d / "events.jsonl"),
               VELOCITY_EXPLORE_SHOTS_DIR=str(d / "steps"), VELOCITY_KEEP_WDA="1")
    cmd = [V, "explore", task["goal"], "--app-id", first_app, "-p", "ios", "-d", udid, "--benchmark",
           "--values-from-goal", "--values-from-screen", "--values-from-screenshots",
           "--values-from-model",
           "--screenshots", "always", "--apps-file", str(apps_file),
           "--max-steps", str(a.max_steps), "--timeout", str(a.timeout), "--json",
           "--price", os.environ.get("PRICE", "2,10")]
    t = time.time()
    # Argument list, no shell; the values come from this script's own flags and the developer's env.
    # nosemgrep: python.lang.security.audit.dangerous-subprocess-use-tainted-env-args.dangerous-subprocess-use-tainted-env-args
    p = subprocess.run(cmd, capture_output=True, text=True, env=env, timeout=a.timeout + 300)
    secs = round(time.time() - t, 1)
    (d / "stderr.log").write_text(p.stderr)
    final = d / "final_state"
    final.mkdir()
    sh("xcrun", "simctl", "io", udid, "screenshot", str(final / "screenshot.png"))
    try:
        rep = json.loads(p.stdout).get("report", {})
    except json.JSONDecodeError:
        rep = {}
    # One trajectory step per acted turn: its screenshot, its action, and the
    # next turn's screenshot (the final one after the last).
    acted = {}
    for line in (d / "events.jsonl").read_text().splitlines() if (d / "events.jsonl").exists() else []:
        try:
            e = json.loads(line)
        except json.JSONDecodeError:
            continue
        if e.get("event") == "acted":
            acted.setdefault(e["turn"], []).append({"action": e.get("action"), "ok": e.get("ok"),
                                                   "error": e.get("error")})
    traj = []
    turns = sorted(acted)
    for i, n in enumerate(turns):
        shot = d / "steps" / f"turn-{n:02d}.png"
        nxt = d / "steps" / f"turn-{turns[i + 1]:02d}.png" if i + 1 < len(turns) else final / "screenshot.png"
        traj.append({"step": i + 1,
                     # Absolute: the judge opens them from its own directory.
                     "screenshot": str(shot.resolve()) if shot.exists() else None,
                     "actions": acted[n],
                     "post_action_screenshot": str(nxt.resolve()) if nxt.exists() else None})
    (d / "trajectory.json").write_text(json.dumps(traj, indent=2))
    answer = rep.get("answer") or rep.get("reason")
    (d / "task.json").write_text(json.dumps({
        "task": task["name"], "goal": task["goal"], "apps": task.get("apps"),
        "category": task.get("category"), "status": "ok" if rep else "error",
        "agent_answer": answer, "velocity_outcome": rep.get("outcome"),
        "steps": len(traj), "seconds": secs, "cost_usd": rep.get("costUsd"),
    }, indent=2))
    return {"task": task["name"], "outcome": rep.get("outcome"), "steps": len(traj), "seconds": secs,
            "cost_usd": rep.get("costUsd"), "exit": p.returncode}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--udid", required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--category")
    ap.add_argument("--tasks")
    ap.add_argument("--limit", type=int)
    ap.add_argument("--max-steps", type=int, default=50)  # iOSWorld's own default
    ap.add_argument("--timeout", type=int, default=900)
    a = ap.parse_args()
    manifest = json.loads(MANIFEST.read_text())
    tasks = json.loads((IOSW / "tasks.json").read_text())
    if a.category:
        tasks = [t for t in tasks if t.get("category") == a.category]
    if a.tasks:
        want = set(a.tasks.split(","))
        tasks = [t for t in tasks if t["name"] in want]
    if a.limit:
        tasks = tasks[: a.limit]
    a.out.mkdir(parents=True, exist_ok=True)
    for t in tasks:
        row = run_task(t, a.udid, manifest, a.out, a)
        with open(a.out / "velocity-results.jsonl", "a") as f:
            f.write(json.dumps(row) + "\n")
        print(json.dumps(row), flush=True)


if __name__ == "__main__":
    main()
