# SPDX-FileCopyrightText: the Atalanta authors
# SPDX-License-Identifier: AGPL-3.0-only

"""Summarise atalanta results.json files written by scripts/measure/*.sh.

Usage: summarize.py DIR LABEL [LABEL...] [--steps PATTERN] [--by test|action]

For each LABEL, reads DIR/LABEL/*/results.json and prints, per step
(test name + step index + action), the median and p90 duration and
settle time in ms over the rounds, then the per-label difference against
the first label. Only the standard library is used.
"""
import glob
import json
import os
import re
import statistics
import sys


def ms(d):
    if isinstance(d, dict):
        return d.get("secs", 0) * 1000 + d.get("nanos", 0) / 1e6
    if isinstance(d, (int, float)):
        return float(d)
    return 0.0


def pct(xs, p):
    if not xs:
        return float("nan")
    xs = sorted(xs)
    k = min(len(xs) - 1, max(0, round(p * (len(xs) - 1))))
    return xs[k]


def load(dir_, label):
    steps = {}
    runs = 0
    for path in sorted(glob.glob(os.path.join(dir_, label, "*", "results.json"))):
        try:
            with open(path) as f:
                doc = json.load(f)
        except (OSError, ValueError):
            continue
        runs += 1
        for test in doc.get("tests", []):
            name = test.get("test_name") or test.get("testName") or "?"
            for st in test.get("steps", []):
                status = str(st.get("status", "")).lower()
                if status not in ("passed", "pass"):
                    continue
                idx = st.get("step_index", st.get("stepIndex", 0))
                action = st.get("action_name") or st.get("actionName") or "?"
                key = (name, idx, action)
                entry = steps.setdefault(key, {"dur": [], "settle": []})
                entry["dur"].append(ms(st.get("duration")))
                s = st.get("settle_ms", st.get("settleMs"))
                if s is not None:
                    entry["settle"].append(float(s))
    return runs, steps


def main(argv):
    pattern = None
    if "--steps" in argv:
        i = argv.index("--steps")
        pattern = re.compile(argv[i + 1])
        argv = argv[:i] + argv[i + 2:]
    dir_, labels = argv[0], argv[1:]
    data = {label: load(dir_, label) for label in labels}
    keys = []
    for _, (_, steps) in data.items():
        for k in steps:
            if k not in keys:
                keys.append(k)
    keys.sort(key=lambda k: (k[0], k[1]))
    print(f"results: {dir_}")
    for label in labels:
        print(f"  {label}: {data[label][0]} run(s)")
    header = f"{'test / step':<48}" + "".join(f"{l[:22]:>24}" for l in labels)
    print()
    print("median duration ms (p90) [median settle ms]")
    print(header)
    totals = {l: [] for l in labels}
    for k in keys:
        title = f"{k[0][:28]} #{k[1]} {k[2]}"
        if pattern and not pattern.search(title):
            continue
        row = f"{title[:47]:<48}"
        for label in labels:
            e = data[label][1].get(k)
            if not e or not e["dur"]:
                row += f"{'-':>24}"
                continue
            med = statistics.median(e["dur"])
            totals[label].append(med)
            settle = statistics.median(e["settle"]) if e["settle"] else float("nan")
            row += f"{med:>9.0f} ({pct(e['dur'], 0.9):>5.0f}) [{settle:>5.0f}]"
        print(row)
    print()
    base = labels[0]
    for label in labels:
        n = len(totals[label])
        tot = sum(totals[label])
        line = f"{label}: {n} step(s), sum of medians {tot:.0f} ms, {tot / n if n else 0:.1f} ms/step"
        if label != base and totals[base] and n:
            diff = tot - sum(totals[base])
            line += f"; vs {base}: {diff:+.0f} ms total, {diff / n:+.1f} ms/step"
        print(line)


if __name__ == "__main__":
    if len(sys.argv) < 3:
        print(__doc__)
        sys.exit(2)
    main(sys.argv[1:])
