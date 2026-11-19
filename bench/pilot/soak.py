#!/usr/bin/env python3
"""Soak: every agent path and the pilot's saved tests, in parallel, for N minutes.

Lanes (one thread each, its own device):
- android: agent commands (launch, snapshot, tap, find, assert, back, a
  stale ref) and a `velocity run` of one saved Android test per cycle;
  one real-model `explore` on the first cycle.
- android-b: a second agent (its own session) snapshotting the same
  emulator throughout: each reply must be ok or DEVICE_BUSY.
- ios: the same on the simulator (Settings), `velocity run` of a saved
  iOS test, one `explore`.
- web: agent commands through `velocity daemon` on the pilot app, a
  `velocity run` of a saved web test, and every 3rd cycle that test against
  the app with its feature broken (it must fail); one `explore`.

Every operation is one JSON line in out/soak/ops.jsonl:
{lane, op, ok, expected, ms, code, detail}. `expected` marks results that
are right although not "ok" (DEVICE_BUSY for the second agent, a failure
on the broken app). Summary: python3 soak.py --summary.
"""

import argparse
import itertools
import json
import os
import subprocess
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
V = os.environ.get("VELOCITY_BIN", str(HERE.parents[1] / "target" / "debug" / "velocity"))
OUT = HERE / "out" / "soak"
TESTS = HERE / "out"
LOCK = threading.Lock()
STOP = threading.Event()
ENV = dict(os.environ, VELOCITY_AGENT_DIR=str(OUT / "agent"), VELOCITY_DAEMON_SOCKET="/tmp/vel-soak.sock",
           VELOCITY_KEEP_WDA="1")


def log(lane, op, ok, ms, code=None, detail=None, expected=False):
    with LOCK, open(OUT / "ops.jsonl", "a") as f:
        f.write(json.dumps({"t": round(time.time(), 1), "lane": lane, "op": op, "ok": ok,
                            "expected": expected, "ms": ms, "code": code,
                            "detail": (detail or "")[-400:]}) + "\n")


def agent(lane, session, *args, expect_codes=()):
    """One agent command with --json; logged; returns the envelope."""
    t = time.time()
    # Argument list, no shell; the values come from this script's own flags and the developer's env.
    # nosemgrep: python.lang.security.audit.dangerous-subprocess-use-tainted-env-args.dangerous-subprocess-use-tainted-env-args
    p = subprocess.run([V, *args, "--session", session, "--json"], capture_output=True, text=True,
                       env=ENV, timeout=180)
    ms = int((time.time() - t) * 1000)
    try:
        r = json.loads(p.stdout.strip().splitlines()[-1])
    except (json.JSONDecodeError, IndexError):
        log(lane, args[0], False, ms, "NO_JSON", p.stdout[-300:] + p.stderr[-300:])
        return {}
    code = (r.get("error") or {}).get("code")
    expected = code in expect_codes
    log(lane, args[0], bool(r.get("ok")), ms, code,
        None if r.get("ok") else (r.get("error") or {}).get("message"), expected)
    return r


def run_test(lane, platform, test, device, must_fail=False):
    t = time.time()
    # Argument list, no shell; the values come from this script's own flags and the developer's env.
    # nosemgrep: python.lang.security.audit.dangerous-subprocess-use-tainted-env-args.dangerous-subprocess-use-tainted-env-args
    p = subprocess.run([V, "run", str(test), "-p", platform, "-d", device], capture_output=True,
                       text=True, env=ENV, timeout=600, cwd=OUT)
    ms = int((time.time() - t) * 1000)
    ok = p.returncode == 0
    op = f"run {test.parent.name}" + (" (broken app)" if must_fail else "")
    tail = "" if ok else (p.stdout + p.stderr)[-400:]
    log(lane, op, ok if not must_fail else not ok, ms, None if ok else f"exit {p.returncode}", tail,
        expected=must_fail and not ok)


def explore(lane, platform, device, app_id, goal, gid):
    t = time.time()
    d = OUT / "explore" / gid
    d.mkdir(parents=True, exist_ok=True)
    # Argument list, no shell; the values come from this script's own flags and the developer's env.
    # nosemgrep: python.lang.security.audit.dangerous-subprocess-use-tainted-env-args.dangerous-subprocess-use-tainted-env-args
    p = subprocess.run([V, "explore", goal, "--app-id", app_id, "-p", platform, "-d", device,
                        "--out", str(d / "test.yaml"), "--force", "--json", "--no-demos",
                        "--price", "2,10", "--max-cost", "1.5"],
                       capture_output=True, text=True, env=ENV, timeout=1200)
    ms = int((time.time() - t) * 1000)
    try:
        g = json.loads(p.stdout).get("gates", {})
    except json.JSONDecodeError:
        g = {}
    log(lane, f"explore {gid}", bool(g.get("trusted")), ms, None if g.get("trusted") else "UNTRUSTED",
        json.dumps({"reasons": g.get("reasons"), "wrongPath": g.get("wrongPath")}))


def ref_of(snapshot_text, needle):
    for line in (snapshot_text or "").splitlines():
        if needle in line and line.startswith("@e"):
            return line.split()[0]
    return None


def android(dev):
    lane, s = "android", "soak-a"
    tests = itertools.cycle(sorted(TESTS.glob("a-*/test.yaml")))
    for cycle in itertools.count():
        if STOP.is_set():
            return
        if cycle == 0:
            explore(lane, "android", dev, "com.android.settings", "open Battery settings", "a-battery")
        agent(lane, s, "launch", "com.android.settings", "-d", dev)
        snap = agent(lane, s, "snapshot", "--find", "network").get("data")
        ref = ref_of(snap, "Network")
        if ref:
            agent(lane, s, "tap", ref)
            agent(lane, s, "assert", "Internet")
            # The ref named a row of the screen we left: it must be stale.
            agent(lane, s, "tap", ref, expect_codes=("STALE_REF",))
        agent(lane, s, "press", "back")
        agent(lane, s, "assert", "Apps")
        agent(lane, s, "snapshot", "--limit", "10")
        run_test(lane, "android", next(tests), dev)


def android_b(dev):
    lane, s = "android-b", "soak-b"
    while not STOP.is_set():
        agent(lane, s, "snapshot", "-d", dev, "--limit", "5", expect_codes=("DEVICE_BUSY",))
        time.sleep(2)


def ios(dev):
    lane, s = "ios", "soak-i"
    tests = itertools.cycle(sorted(TESTS.glob("i-*/test.yaml")))
    for cycle in itertools.count():
        if STOP.is_set():
            return
        if cycle == 0:
            explore(lane, "ios", dev, "com.apple.Preferences", "open General and then Software Update",
                    "i-update")
        agent(lane, s, "launch", "com.apple.Preferences", "-d", dev, "-p", "ios")
        snap = agent(lane, s, "snapshot", "--find", "general").get("data")
        ref = ref_of(snap, "General")
        if ref:
            agent(lane, s, "tap", ref)
            agent(lane, s, "assert", "About")
            agent(lane, s, "tap", ref, expect_codes=("STALE_REF",))
        agent(lane, s, "snapshot", "--limit", "10")
        run_test(lane, "ios", next(tests), dev)


def web():
    lane, s = "web", "soak-w"
    app = "http://127.0.0.1:8790/"
    tests = itertools.cycle(sorted(TESTS.glob("w-*/test.yaml")))
    bugs = json.loads((HERE / "goals.json").read_text())["web"]["goals"]
    bug_of = {g["id"]: g["bug"] for g in bugs}
    for cycle in itertools.count():
        if STOP.is_set():
            return
        if cycle == 0:
            explore(lane, "web", "web", app, "turn on the email newsletter in settings", "w-news")
        agent(lane, s, "launch", app, "-p", "web")
        agent(lane, s, "tap", "Search")
        agent(lane, s, "type", "id=q", "mug")
        agent(lane, s, "tap", "id=search-go")
        agent(lane, s, "assert", "Mug warmer")
        agent(lane, s, "tap", "@e999", expect_codes=("STALE_REF",))
        agent(lane, s, "snapshot", "--find", "mug")
        test = next(tests)
        # The agent's browser lives in the daemon; `run` launches its own.
        run_test(lane, "web", test, "web")
        if cycle % 3 == 2:
            (HERE / "app" / "bug.js").write_text(f'window.BUG = "{bug_of[test.parent.name]}";\n')
            try:
                run_test(lane, "web", test, "web", must_fail=True)
            finally:
                (HERE / "app" / "bug.js").write_text('window.BUG = "none";\n')


def summary():
    ops = [json.loads(l) for l in open(OUT / "ops.jsonl")]
    by = {}
    for o in ops:
        key = (o["lane"], o["op"].split(" (")[0] if not o["op"].startswith("run") else o["op"])
        by.setdefault(key, []).append(o)
    print("| lane | operation | count | ok | expected | failed | median ms | p95 ms |")
    print("|---|---|---|---|---|---|---|---|")
    for (lane, op), rows in sorted(by.items()):
        ms = sorted(r["ms"] for r in rows)
        ok = sum(r["ok"] for r in rows)
        exp = sum((not r["ok"]) and r["expected"] for r in rows)
        bad = len(rows) - ok - exp
        print(f"| {lane} | {op} | {len(rows)} | {ok} | {exp} | {bad} | {ms[len(ms)//2]} | "
              f"{ms[min(len(ms)-1, int(len(ms)*0.95))]} |")
    fails = [o for o in ops if not o["ok"] and not o["expected"]]
    span = (ops[-1]["t"] - ops[0]["t"]) / 60 if ops else 0
    print(f"\n{len(ops)} operations in {span:.1f} min; unexpected failures: {len(fails)}")
    for o in fails[:40]:
        print(f"- {o['lane']} {o['op']} {o['code']}: {o['detail'][:240]!r}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--minutes", type=float, default=30)
    ap.add_argument("--android", default="emulator-5554")
    ap.add_argument("--ios")
    ap.add_argument("--summary", action="store_true")
    a = ap.parse_args()
    if a.summary:
        summary()
        return
    OUT.mkdir(parents=True, exist_ok=True)
    lanes = [threading.Thread(target=android, args=(a.android,)),
             threading.Thread(target=android_b, args=(a.android,)),
             threading.Thread(target=web)]
    if a.ios:
        lanes.append(threading.Thread(target=ios, args=(a.ios,)))
    for t in lanes:
        t.start()
    time.sleep(a.minutes * 60)
    STOP.set()
    for t in lanes:
        t.join()
    # Argument list, no shell; the values come from this script's own flags and the developer's env.
    # nosemgrep: python.lang.security.audit.dangerous-subprocess-use-tainted-env-args.dangerous-subprocess-use-tainted-env-args
    subprocess.run([V, "daemon", "stop"], env=ENV, capture_output=True)
    summary()


if __name__ == "__main__":
    main()
