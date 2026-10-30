#!/bin/sh
# Set the workspace version everywhere it is written:
#   scripts/bump-version.sh 0.3.0
# Cargo.toml's [workspace.package] version and the `=X.Y.Z` pins of the
# workspace's own crates in [workspace.dependencies], then Cargo.lock.
# CHANGELOG.md gets a `## [X.Y.Z] - unreleased` heading if it has none.
# The npm packages and the Homebrew formula take their version from the tag.
set -eu

new=${1:?usage: bump-version.sh X.Y.Z}
new=${new#v}
case $new in
    [0-9]*.[0-9]*.[0-9]*) ;;
    *) echo "bump-version: '$new' is not X.Y.Z" >&2; exit 1 ;;
esac

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
old=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
[ -n "$old" ] || { echo "bump-version: no [workspace.package] version" >&2; exit 1; }

tmp=$(mktemp)
awk -v old="$old" -v new="$new" '
    /^\[/ { section = $0 }
    section == "[workspace.package]" && $0 == "version = \"" old "\"" { $0 = "version = \"" new "\"" }
    section == "[workspace.dependencies]" && /^velocity-/ { gsub("version = \"=" old "\"", "version = \"=" new "\"") }
    { print }
' Cargo.toml >"$tmp"
mv "$tmp" Cargo.toml

cargo update --workspace --offline >/dev/null 2>&1 || cargo update --workspace >/dev/null

if ! grep -q "^## \[$new\]" CHANGELOG.md; then
    tmp=$(mktemp)
    awk -v new="$new" '
        { print }
        /^## \[Unreleased\]/ && !done { print ""; print "## [" new "] - unreleased"; done = 1 }
    ' CHANGELOG.md >"$tmp"
    mv "$tmp" CHANGELOG.md
fi

echo "version $old -> $new"
grep -c "=$new\"" Cargo.toml | xargs printf '%s workspace crate pins updated\n'
