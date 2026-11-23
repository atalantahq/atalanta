#!/usr/bin/env python3
# SPDX-FileCopyrightText: the Atalanta authors
# SPDX-License-Identifier: AGPL-3.0-only

"""Count `unsafe` code per crate and compare with the pinned baseline.

    scripts/unsafe-inventory.py            # print the per-crate report
    scripts/unsafe-inventory.py --check    # exit 1 when a crate has more than its baseline
    scripts/unsafe-inventory.py --write    # rewrite scripts/unsafe-baseline.txt

What counts: `unsafe {` blocks and `unsafe fn` / `unsafe impl` / `unsafe
extern` / `unsafe trait` items in the crate's .rs files (src/, tests/,
benches/, build.rs), one per occurrence, comment lines skipped. A grep,
not a compiler pass (cargo-geiger needs a full build per target and does
not count a workspace's own crates any better), so it errs on the side of
counting: a string literal that says "unsafe {" counts too.

--check fails when any crate's count rises above its line in
scripts/unsafe-baseline.txt, and in particular when a crate pinned at 0
gains its first `unsafe`. That is a review gate, not a ban: if the new
unsafe is justified (with a `// SAFETY:` comment; clippy's
undocumented_unsafe_blocks lint asks for one), run --write and commit the
baseline change with it, so the reviewer sees the count move. A count that
drops is reported so the baseline can be lowered.
"""

import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BASELINE = os.path.join(ROOT, "scripts", "unsafe-baseline.txt")
SKIP_DIRS = {"target", "node_modules", "dist", "frontend", ".git"}
UNSAFE = re.compile(r"(?<![\w])unsafe(\s*\{|\s+(fn|impl|extern|trait)\b)")


def crates():
    """(name, dir) for every workspace crate and the fuzz crate."""
    out = []
    for base in ("crates",):
        for name in sorted(os.listdir(os.path.join(ROOT, base))):
            d = os.path.join(ROOT, base, name)
            if os.path.isfile(os.path.join(d, "Cargo.toml")):
                out.append((name, d))
    for name in ("atalanta-cli", "fuzz"):
        d = os.path.join(ROOT, name)
        if os.path.isfile(os.path.join(d, "Cargo.toml")):
            out.append((name, d))
    return out


def count(crate_dir):
    n = 0
    for dirpath, dirnames, filenames in os.walk(crate_dir):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
        for f in filenames:
            if not f.endswith(".rs"):
                continue
            with open(os.path.join(dirpath, f), encoding="utf-8", errors="replace") as fh:
                for line in fh:
                    stripped = line.lstrip()
                    if stripped.startswith("//") or stripped.startswith("*"):
                        continue
                    code = line.split("//", 1)[0]
                    n += len(UNSAFE.findall(code))
    return n


def read_baseline():
    pins = {}
    if not os.path.exists(BASELINE):
        return pins
    with open(BASELINE, encoding="utf-8") as fh:
        for line in fh:
            line = line.split("#", 1)[0].strip()
            if line:
                name, n = line.split()
                pins[name] = int(n)
    return pins


def main():
    counts = {name: count(d) for name, d in crates()}
    if "--write" in sys.argv:
        with open(BASELINE, "w", encoding="utf-8") as fh:
            fh.write(
                "# unsafe occurrences per crate (scripts/unsafe-inventory.py --write).\n"
                "# --check fails when a crate goes above its number; raise it only in\n"
                "# the same change as the reviewed unsafe code.\n"
            )
            for name, n in sorted(counts.items()):
                fh.write(f"{name} {n}\n")
        print(f"wrote {os.path.relpath(BASELINE, ROOT)}")
        return 0

    pins = read_baseline()
    bad = 0
    print(f"{'crate':<28}{'unsafe':>8}{'baseline':>10}")
    for name, n in sorted(counts.items()):
        pin = pins.get(name)
        note = ""
        if pin is None:
            note = "  (not in baseline: treated as 0)"
            pin = 0
        if n > pin:
            note += "  <-- above baseline"
            bad += 1
        elif n < pin:
            note += "  (below baseline: lower it with --write)"
        print(f"{name:<28}{n:>8}{pin:>10}{note}")
    total = sum(counts.values())
    print(f"{'total':<28}{total:>8}")
    if "--check" in sys.argv and bad:
        print(
            f"error: {bad} crate(s) have more unsafe code than scripts/unsafe-baseline.txt allows; "
            "review it, then scripts/unsafe-inventory.py --write",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
