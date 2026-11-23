#!/usr/bin/env python3
"""Tally `cargo clippy --message-format=json` output by lint.

    clippy_tally.py CLIPPY_JSON [--show LINT[,LINT...]] [--files LIST] [--top N]

--show   list every hit (file:line and the source line) for these lints,
         e.g. --show unwrap_used,expect_used  (the clippy:: prefix is optional)
--files  only count hits in the files named in LIST, one path per line
         ("-" reads stdin), e.g. from `git diff --name-only main...`
--top    how many lints the summary lists (default 25)

A lint hit that cargo reports once per target (lib, then lib-as-test) is
counted once.
"""
import argparse
import collections
import json
import sys

# The lints that point at unfinished or lazy code. Everything else pedantic
# and nursery report is style.
SLOP = [
    "clippy::todo",
    "clippy::unimplemented",
    "clippy::dbg_macro",
    "clippy::panic",
    "clippy::unwrap_used",
    "clippy::expect_used",
    "clippy::allow_attributes",
    "clippy::redundant_clone",
    "clippy::print_stdout",
]


def hits(path):
    seen = set()
    with open(path, encoding="utf-8") as f:
        for line in f:
            try:
                msg = json.loads(line)
            except ValueError:
                continue
            if msg.get("reason") != "compiler-message":
                continue
            diag = msg["message"]
            code = (diag.get("code") or {}).get("code")
            if not code or diag.get("level") not in ("warning", "error"):
                continue
            span = next((s for s in diag["spans"] if s.get("is_primary")), None)
            if span is None:
                continue
            key = (code, span["file_name"], span["line_start"], span["column_start"])
            if key in seen:
                continue
            seen.add(key)
            text = span["text"][0]["text"].strip() if span.get("text") else ""
            yield code, span["file_name"], span["line_start"], text


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("clippy_json")
    ap.add_argument("--show", default="")
    ap.add_argument("--files")
    ap.add_argument("--top", type=int, default=25)
    args = ap.parse_args()

    only = None
    if args.files:
        src = sys.stdin if args.files == "-" else open(args.files, encoding="utf-8")
        only = {line.strip() for line in src if line.strip()}

    show = {s if "::" in s else f"clippy::{s}" for s in args.show.split(",") if s}
    by_lint = collections.Counter()
    by_file = collections.defaultdict(collections.Counter)
    listed = collections.defaultdict(list)
    for code, file, line, text in hits(args.clippy_json):
        if only is not None and file not in only:
            continue
        by_lint[code] += 1
        by_file[code][file] += 1
        if code in show:
            listed[code].append(f"{file}:{line}  {text[:140]}")

    print(f"{sum(by_lint.values())} warnings, {len(by_lint)} lints")
    print("\nslop lints:")
    for lint in SLOP:
        print(f"  {by_lint[lint]:5}  {lint}")
    print("\nmost frequent:")
    for lint, n in by_lint.most_common(args.top):
        print(f"  {n:5}  {lint}")
    for lint in sorted(show):
        print(f"\n{lint} ({len(listed[lint])}):")
        for row in sorted(listed[lint]):
            print(f"  {row}")
        if len(listed[lint]) > 40:
            print("  by file:")
            for file, n in by_file[lint].most_common(15):
                print(f"    {n:4}  {file}")


if __name__ == "__main__":
    main()
