#!/bin/sh
# Print one version's section of CHANGELOG.md (without its heading): the
# body between `## [X.Y.Z]` and the next `## [` heading. The release
# workflow uses it as the GitHub release notes.
#
#   scripts/changelog-section.sh 0.2.0 [CHANGELOG.md]
#
# Exits 1 if the version has no section, or the section is empty.
set -eu

version=${1:?usage: changelog-section.sh <version> [changelog]}
file=${2:-CHANGELOG.md}

[ -f "$file" ] || { echo "changelog-section: $file not found" >&2; exit 1; }

out=$(awk -v v="$version" '
    index($0, "## [" v "]") == 1 { on = 1; next }
    on && /^## \[/ { exit }
    on && /^\[[^]]+\]: / { exit }
    on { print }
' "$file")

# Trim leading and trailing blank lines.
out=$(printf '%s\n' "$out" | awk 'NF { found = 1 } found' | awk '
    { lines[NR] = $0 } NF { last = NR }
    END { for (i = 1; i <= last; i++) print lines[i] }')

if [ -z "$out" ]; then
    echo "changelog-section: no entry for $version in $file" >&2
    exit 1
fi
printf '%s\n' "$out"
