#!/usr/bin/env python3
"""Print a markdown table of wall-clock median and p90 per tool.

Usage: summarize.py results.csv

Reads the CSV run.sh writes. Warm-up rows are ignored. Statistics cover the
passing measured runs only; failures are counted in the table, not timed.
p90 uses linear interpolation between closest ranks (numpy's default), so with
N=5 it lies between the two slowest runs.
"""

import csv
import statistics
import sys


def percentile(values, p):
    xs = sorted(values)
    if len(xs) == 1:
        return xs[0]
    k = (len(xs) - 1) * p / 100
    lo = int(k)
    hi = min(lo + 1, len(xs) - 1)
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)


def main(path):
    runs = {}
    with open(path, newline="") as f:
        for row in csv.DictReader(f):
            if row["phase"] != "measured":
                continue
            r = runs.setdefault(row["tool"], {"ok": [], "total": 0})
            r["total"] += 1
            if row["exit_code"] == "0":
                r["ok"].append(int(row["wall_ms"]) / 1000)

    if not runs:
        sys.exit(f"no measured runs in {path}")

    print("| Tool | Passed | Median (s) | p90 (s) | Min (s) | Max (s) |")
    print("| --- | --- | --- | --- | --- | --- |")
    medians = {}
    for tool in sorted(runs):
        r = runs[tool]
        ok = r["ok"]
        if ok:
            medians[tool] = statistics.median(ok)
            cells = [medians[tool], percentile(ok, 90), min(ok), max(ok)]
            stats = " | ".join(f"{v:.2f}" for v in cells)
        else:
            stats = "– | – | – | –"
        print(f"| {tool} | {len(ok)}/{r['total']} | {stats} |")

    if "velocity" in medians and "maestro" in medians and medians["velocity"] > 0:
        ratio = medians["maestro"] / medians["velocity"]
        print(f"\nMedian wall clock, Maestro / Velocity: {ratio:.1f}x")
    if any(len(r["ok"]) < r["total"] for r in runs.values()):
        print("\nSome runs failed; see logs/ next to the CSV. Failed runs are not timed.")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(sys.argv[1])
