#!/usr/bin/env bash
# CycloneDX SBOMs for what Velocity ships, into OUT_DIR:
#
#   velocity-cli.cdx.json         the `velocity` binary: every crate in the
#                                 dependency graph for all target platforms
#                                 (cargo-cyclonedx; build deps left out)
#   inspector-frontend.cdx.json   the inspector web UI embedded in the binary:
#                                 production npm dependencies only
#                                 (npm sbom, from package-lock.json)
#
# Usage: scripts/sbom.sh OUT_DIR [VERSION]
# Needs cargo-cyclonedx (cargo install --locked cargo-cyclonedx) and npm.
# Used by .github/workflows/security.yml (artifact) and release.yml (assets);
# VERSION, when given, is put into the file names (velocity-X.Y.Z-...).
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
OUT=${1:?usage: scripts/sbom.sh OUT_DIR [VERSION]}
VERSION=${2:-}
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
prefix=${VERSION:+velocity-$VERSION-}

cd "$ROOT"
# cargo-cyclonedx writes <name>.json (--override-filename) next to each
# crate's Cargo.toml; a unique name, moved out right away, leaves nothing behind in the tree.
tmpname="sbom-$$"
cargo cyclonedx --manifest-path velocity-cli/Cargo.toml --format json \
    --spec-version 1.5 --target all --no-build-deps --all-features \
    --override-filename "$tmpname" -q
mv "velocity-cli/$tmpname.json" "$OUT/${prefix}velocity-cli.cdx.json"
# Other workspace members get one too; they are not shipped on their own.
find crates velocity-cli -maxdepth 2 -name "$tmpname.json" -delete

(cd crates/velocity-inspector/frontend &&
    npm sbom --sbom-format cyclonedx --omit=dev --package-lock-only) \
    > "$OUT/${prefix}inspector-frontend.cdx.json"

for f in "$OUT/${prefix}velocity-cli.cdx.json" "$OUT/${prefix}inspector-frontend.cdx.json"; do
    python3 -I -c 'import json, sys
d = json.load(open(sys.argv[1]))
assert d["bomFormat"] == "CycloneDX", sys.argv[1]
print(sys.argv[1] + ": " + str(len(d.get("components", []))) + " components")' "$f"
done
