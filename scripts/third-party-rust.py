#!/usr/bin/env python3
"""Generate docs/licenses/THIRD-PARTY-RUST.md from Cargo.lock.

    scripts/third-party-rust.py            # print to stdout
    scripts/third-party-rust.py --write    # rewrite the committed file
    scripts/third-party-rust.py --check    # exit 1 when the committed file is stale

Reads `cargo metadata --locked --all-features` (every target platform, dev
dependencies included), so it needs only cargo: no cargo-deny or cargo-about.
`cargo deny check licenses` (deny.toml) is still what enforces the allow-list;
this file is the human-readable inventory NOTICE points at. CI runs --check,
so a Cargo.lock change without a regenerated inventory fails.
"""

import collections
import json
import os
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "docs", "licenses", "THIRD-PARTY-RUST.md")

# Licence identifiers that carry a copyleft obligation if taken.
COPYLEFT = ("GPL", "LGPL", "AGPL", "MPL", "EPL", "CDDL")


def metadata():
    cmd = ["cargo", "metadata", "--format-version", "1", "--locked", "--all-features"]
    if os.environ.get("CARGO_NET_OFFLINE") or "--offline" in sys.argv:
        cmd.append("--offline")
    out = subprocess.run(cmd, cwd=ROOT, check=True, capture_output=True, text=True).stdout
    return json.loads(out)


def is_copyleft(expr):
    return any(tok in expr for tok in COPYLEFT)


def render(meta):
    members = set(meta["workspace_members"])
    crates = sorted(
        (p for p in meta["packages"] if p["id"] not in members),
        key=lambda p: (p["name"], [int(x) if x.isdigit() else x for x in p["version"].replace("-", ".").split(".")]),
    )
    counts = collections.Counter((p.get("license") or "UNKNOWN") for p in crates)

    with_copyleft = [p for p in crates if is_copyleft(p.get("license") or "")]
    choice = [p for p in with_copyleft if " OR " in p["license"] or "/" in p["license"]]
    only = [p for p in with_copyleft if p not in choice]

    lines = [
        "# Third-party Rust dependencies",
        "",
        "Generated from `Cargo.lock` by `scripts/third-party-rust.py` (`cargo metadata --locked "
        "--all-features`: every target platform, dev dependencies included). Do not edit by hand: "
        "run `scripts/third-party-rust.py --write` after dependency changes (CI fails when this "
        "file is stale). `cargo deny check licenses` (see `deny.toml`) enforces the allow-list.",
        "",
        "Velocity itself is Apache-2.0. Where a crate offers a choice of licences "
        "(for example `MIT OR Apache-2.0`), Velocity uses it under a permissive option.",
    ]
    if choice:
        lines += [
            "",
            "Crates whose choice includes a copyleft option Velocity does not take: "
            + ", ".join(f"`{p['name']} {p['version']}` (`{p['license']}`)" for p in choice)
            + ".",
        ]
    if only:
        lines += [
            "",
            "Crates under a copyleft licence only (used unmodified as libraries): "
            + ", ".join(f"`{p['name']} {p['version']}` (`{p['license']}`)" for p in only)
            + ".",
        ]
    lines += [
        "",
        f"Crates listed: {len(crates)} (excluding the {len(members)} workspace crates).",
        "",
        "## Count by license expression",
        "",
        "| License expression | Crates |",
        "| --- | ---: |",
    ]
    for expr, n in sorted(counts.items(), key=lambda kv: (-kv[1], kv[0])):
        lines.append(f"| `{expr}` | {n} |")
    lines += [
        "",
        "## Crates",
        "",
        "| Crate | Version | License | Repository |",
        "| --- | --- | --- | --- |",
    ]
    for p in crates:
        repo = p.get("repository") or ""
        lines.append(f"| {p['name']} | {p['version']} | `{p.get('license') or 'UNKNOWN'}` | {repo} |")
    return "\n".join(lines) + "\n"


def main():
    text = render(metadata())
    if "--write" in sys.argv:
        with open(OUT, "w", encoding="utf-8") as f:
            f.write(text)
        print(f"wrote {os.path.relpath(OUT, ROOT)}", file=sys.stderr)
    elif "--check" in sys.argv:
        with open(OUT, encoding="utf-8") as f:
            current = f.read()
        if current != text:
            print(
                f"{os.path.relpath(OUT, ROOT)} is stale: run scripts/third-party-rust.py --write",
                file=sys.stderr,
            )
            sys.exit(1)
    else:
        sys.stdout.write(text)


if __name__ == "__main__":
    main()
