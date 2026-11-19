#!/usr/bin/env python3
"""Summarise a pilot round: python3 summarize.py out/runs.jsonl [out/calls.jsonl]

Prints one row per goal and the totals the kill test scores (S0: trusted
and 5/5), plus what the pilot adds: wrong-path results and, for web goals,
false passes on the app with the goal's feature broken. With a calls file
from claude_bridge.py it also checks the login secret never reached the
model (SECRET_VALUES, comma-separated, default the pilot's test password).
"""

import json
import os
import statistics
import sys


def main():
    runs = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
    runs.sort(key=lambda r: r["goal"])
    print("| goal | outcome | trusted | replays | wrong path | broken app passed | cost $ | s |")
    print("|---|---|---|---|---|---|---|---|")
    for r in runs:
        mp = r.get("mutant_passes")
        broken = "—" if mp is None else f"{mp}/{r.get('mutant_replays', 3)}"
        print(f"| {r['goal']} | {r.get('outcome') or 'no report'} | {'yes' if r['trusted'] else 'no'} | "
              f"{r.get('passed', 0)}/{r.get('replays', 0)} | {r.get('wrong_path') or '—'} | {broken} | "
              f"{r.get('cost_usd') or 0:.3f} | {r.get('seconds')} |")
    n = len(runs)
    s0 = [r for r in runs if r["trusted"] and r.get("replays") and r.get("passed") == r.get("replays")]
    web = [r for r in runs if r.get("mutant_passes") is not None]
    caught = [r for r in web if r["mutant_passes"] == 0]
    trusted_false = [r for r in web if r["trusted"] and r["mutant_passes"] > 0]
    checked = [r for r in runs if r.get("wrong_path") in ("rejected", "passed")]
    costs = [r["cost_usd"] for r in runs if r.get("cost_usd") is not None]
    print()
    print(f"- S0 (trusted, 5/5): {len(s0)}/{n}")
    print(f"- wrong-path gate ran: {len(checked)}/{n}; caught a weak test: "
          f"{sum(r['wrong_path'] == 'passed' for r in checked)}")
    print(f"- seeded regressions caught (web, test failed every replay): {len(caught)}/{len(web)}")
    print(f"- trusted tests that passed on a broken app (false passes): {len(trusted_false)}")
    if costs:
        print(f"- cost per goal: median ${statistics.median(costs):.3f}, total ${sum(costs):.2f} (list price)")
    if len(sys.argv) > 2:
        secrets = os.environ.get("SECRET_VALUES", "correct-horse-7").split(",")
        text = open(sys.argv[2]).read()
        calls = text.count("\n")
        leaks = sum(text.count(s) for s in secrets if s)
        print(f"- login secrecy: {calls} model calls, secret occurrences: {leaks}")


if __name__ == "__main__":
    main()
