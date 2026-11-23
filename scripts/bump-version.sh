#!/bin/sh
# Set the workspace version everywhere it is written:
#   scripts/bump-version.sh 0.3.0 [--date]
# Cargo.toml's [workspace.package] version and the `=X.Y.Z` pins of the
# workspace's own crates in [workspace.dependencies], then Cargo.lock.
# CHANGELOG.md: everything under `## [Unreleased]` moves into
# `## [X.Y.Z]` (the heading is added, as `- unreleased`, if missing; an
# existing section keeps its entries below the moved ones), because the
# release notes are that section alone and release.yml refuses a tag while
# [Unreleased] has entries. --date also stamps today's date (UTC) on the
# heading, which the release preflight requires.
# The npm packages and the Homebrew formula take their version from the tag.
set -eu

new=${1:?usage: bump-version.sh X.Y.Z [--date]}
new=${new#v}
stamp=0
[ "${2:-}" = "--date" ] && stamp=1
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
    section == "[workspace.dependencies]" && /^atalanta-/ { gsub("version = \"=" old "\"", "version = \"=" new "\"") }
    { print }
' Cargo.toml >"$tmp"
mv "$tmp" Cargo.toml

cargo update --workspace --offline >/dev/null 2>&1 || cargo update --workspace >/dev/null

if ! grep -q "^## \[$new\]" CHANGELOG.md; then
    # The new heading straight under [Unreleased]: its entries now belong to it.
    tmp=$(mktemp)
    awk -v new="$new" '
        { print }
        /^## \[Unreleased\]/ && !done { print ""; print "## [" new "] - unreleased"; done = 1 }
    ' CHANGELOG.md >"$tmp"
    mv "$tmp" CHANGELOG.md
else
    # The section exists already: move [Unreleased]'s entries to its top.
    tmp=$(mktemp)
    awk -v new="$new" '
        /^## \[Unreleased\]/ { print; print ""; inunrel = 1; next }
        inunrel && /^## \[/ { inunrel = 0 }
        inunrel && /^\[[^]]+\]: / { inunrel = 0 }
        inunrel { held[++n] = $0; next }
        index($0, "## [" new "]") == 1 && !done {
            print
            # trim the held block: leading/trailing blank lines
            first = 1; while (first <= n && held[first] ~ /^[[:space:]]*$/) first++
            last = n; while (last >= first && held[last] ~ /^[[:space:]]*$/) last--
            if (first <= last) { print ""; for (i = first; i <= last; i++) print held[i] }
            done = 1; next
        }
        { print }
    ' CHANGELOG.md >"$tmp"
    mv "$tmp" CHANGELOG.md
fi

if [ "$stamp" = 1 ]; then
    today=$(date -u +%Y-%m-%d)
    tmp=$(mktemp)
    awk -v new="$new" -v today="$today" '
        $0 == "## [" new "] - unreleased" { $0 = "## [" new "] - " today }
        { print }
    ' CHANGELOG.md >"$tmp"
    mv "$tmp" CHANGELOG.md
fi

echo "version $old -> $new"
grep -c "=$new\"" Cargo.toml | xargs printf '%s workspace crate pins updated\n'
