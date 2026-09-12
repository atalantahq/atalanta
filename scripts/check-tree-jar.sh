#!/usr/bin/env bash
# Reproducibility check for crates/velocity-android/assets/velocity-tree.jar.
#
# The jar is a d8 dex jar built from probes/android/velocity-tree-server by
# that directory's build.sh (javac --release 11 against the platform's
# android.jar, then d8 --release --min-api 21). The Android driver embeds the
# committed jar with include_bytes!, so a reviewer should be able to confirm
# it was built from the committed Java source and nothing else.
#
# This script rebuilds the jar from source in a temporary directory (it never
# writes to the source tree or replaces the committed jar) and compares:
#
#   1. the jar bytes (byte-equal: done);
#   2. otherwise, the sorted entry list and a SHA-256 of each entry's
#      uncompressed content. A jar can differ only in its zip container
#      (entry timestamps, compression level) while its classes.dex is
#      identical; that still counts as reproduced.
#
# d8 stamps its version into classes.dex (the "~~D8{...}" marker), and its
# output depends on the d8 version and on the class files javac produced, so
# the script picks the build-tools whose d8 matches the committed jar's
# marker. With a different d8 the dex is expected to differ and the result is
# reported as "not comparable" rather than as a mismatch.
#
# Usage: scripts/check-tree-jar.sh [--keep]
#   --keep    leave the temporary build directory in place and print it
# Env:     ANDROID_HOME / ANDROID_SDK_ROOT (default ~/Library/Android/sdk),
#          PLATFORM (android-NN), BUILD_TOOLS (version) to override the picks.
# Exit:    0 reproduced, 1 differs, 2 cannot check (missing tools), 3 not
#          comparable (no build-tools with the d8 that built the jar).
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
SRC="$ROOT/probes/android/velocity-tree-server"
JAR="$ROOT/crates/velocity-android/assets/velocity-tree.jar"
KEEP=0
[ "${1:-}" = "--keep" ] && KEEP=1

die() { echo "check-tree-jar: $*" >&2; exit 2; }

for tool in javac java unzip shasum; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool not found"
done
[ -f "$JAR" ] || die "no committed jar at $JAR"
[ -d "$SRC/src" ] || die "no source at $SRC/src"

SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"
[ -d "$SDK/build-tools" ] || die "Android SDK build-tools not found under $SDK (set ANDROID_HOME)"
[ -d "$SDK/platforms" ] || die "Android SDK platforms not found under $SDK"

# Same platform choice as build.sh: the newest numeric android-NN. Only its
# android.jar is used, as a compile/library classpath (it never ends up in
# the output), so the exact level rarely matters.
if [ -z "${PLATFORM:-}" ]; then
    PLATFORM=$(find "$SDK/platforms" -mindepth 1 -maxdepth 1 -name 'android-*' -exec basename {} \; \
        | grep -E '^android-[0-9]+$' | sort -t- -k2 -n | tail -1)
fi
ANDROID_JAR="$SDK/platforms/$PLATFORM/android.jar"
[ -f "$ANDROID_JAR" ] || die "missing $ANDROID_JAR"

# The d8 that built the committed jar, from the marker in classes.dex.
MARKER=$(unzip -p "$JAR" classes.dex | LC_ALL=C grep -ao '~~D8{[^}]*}' | head -1 || true)
WANT_SHA=$(printf '%s' "$MARKER" | sed -nE 's/.*"sha-1":"([0-9a-f]+)".*/\1/p')
WANT_VER=$(printf '%s' "$MARKER" | sed -nE 's/.*"version":"([^"]+)".*/\1/p')

if [ -z "${BUILD_TOOLS:-}" ]; then
    BUILD_TOOLS=""
    while IFS= read -r bt; do
        d8v=$("$SDK/build-tools/$bt/d8" --version 2>/dev/null | head -1 || true)
        if [ -n "$WANT_SHA" ] && printf '%s' "$d8v" | grep -q "$WANT_SHA"; then
            BUILD_TOOLS=$bt
            break
        fi
    done < <(find "$SDK/build-tools" -mindepth 1 -maxdepth 1 -type d -exec basename {} \; | sort -V)
fi
COMPARABLE=1
if [ -z "$BUILD_TOOLS" ]; then
    COMPARABLE=0
    BUILD_TOOLS=$(find "$SDK/build-tools" -mindepth 1 -maxdepth 1 -type d -exec basename {} \; | sort -V | tail -1)
    echo "warning: no build-tools has the d8 that built the jar (${WANT_VER:-unknown}, ${WANT_SHA:-no marker}); using $BUILD_TOOLS" >&2
fi
D8="$SDK/build-tools/$BUILD_TOOLS/d8"
[ -x "$D8" ] || die "missing $D8"
if [ -z "$WANT_SHA" ] || ! "$D8" --version 2>/dev/null | head -1 | grep -q "$WANT_SHA"; then
    COMPARABLE=0
fi

WORK=$(mktemp -d "${TMPDIR:-/tmp}/velocity-tree-jar.XXXXXX")
# shellcheck disable=SC2317  # invoked by the EXIT trap
cleanup() {
    if [ "$KEEP" = 1 ]; then
        echo "kept: $WORK"
    else
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT

echo "javac:       $(javac -version 2>&1)"
echo "platform:    $PLATFORM"
echo "build-tools: $BUILD_TOOLS ($("$D8" --version 2>/dev/null | head -1))"
echo "jar marker:  ${MARKER:-none}"

# Mirror build.sh, writing only under $WORK.
mkdir -p "$WORK/classes" "$WORK/test" "$WORK/out"
SOURCES=()
while IFS= read -r -d '' f; do SOURCES+=("$f"); done \
    < <(find "$SRC/src" -name '*.java' -print0 | sort -z)
javac --release 11 -cp "$ANDROID_JAR" -d "$WORK/classes" "${SOURCES[@]}"
if [ -d "$SRC/test" ]; then
    TESTS=()
    while IFS= read -r -d '' f; do TESTS+=("$f"); done \
        < <(find "$SRC/test" -name '*.java' -print0 | sort -z)
    if [ "${#TESTS[@]}" -gt 0 ]; then
        javac --release 11 -cp "$WORK/classes:$ANDROID_JAR" -d "$WORK/test" "${TESTS[@]}"
        java -cp "$WORK/test:$WORK/classes:$ANDROID_JAR" com.velocity.tree.HostChecks >/dev/null
    fi
fi
CLASSES=()
while IFS= read -r -d '' f; do CLASSES+=("$f"); done \
    < <(find "$WORK/classes" -name '*.class' -print0 | sort -z)
"$D8" --release --min-api 21 --lib "$ANDROID_JAR" --output "$WORK/out/velocity-tree.jar" "${CLASSES[@]}"
BUILT="$WORK/out/velocity-tree.jar"

sha() { shasum -a 256 "$1" | cut -d' ' -f1; }
echo "committed:   $(sha "$JAR")  $(wc -c < "$JAR" | tr -d ' ') bytes"
echo "rebuilt:     $(sha "$BUILT")  $(wc -c < "$BUILT" | tr -d ' ') bytes"

if cmp -s "$JAR" "$BUILT"; then
    echo "REPRODUCED: byte-identical jar"
    exit 0
fi

# Entry list + per-entry content hashes (container-independent).
entries() {
    unzip -Z1 "$1" | LC_ALL=C sort | while IFS= read -r e; do
        case "$e" in */) continue ;; esac
        printf '%s  %s\n' "$(unzip -p "$1" "$e" | shasum -a 256 | cut -d' ' -f1)" "$e"
    done
}
entries "$JAR" > "$WORK/committed.txt"
entries "$BUILT" > "$WORK/rebuilt.txt"
if cmp -s "$WORK/committed.txt" "$WORK/rebuilt.txt"; then
    echo "REPRODUCED: same entries and per-entry content; only the zip container differs"
    cat "$WORK/committed.txt"
    exit 0
fi

echo "DIFFERS: entry content does not match"
diff "$WORK/committed.txt" "$WORK/rebuilt.txt" || true
if [ "$COMPARABLE" = 0 ]; then
    echo "NOT COMPARABLE: the jar was built with d8 ${WANT_VER:-unknown}; this rebuild used build-tools ${BUILD_TOOLS}" >&2
    exit 3
fi
echo "hint: a different javac (class files feed d8) or edited source since the jar was built are the usual causes; rebuild with probes/android/velocity-tree-server/build.sh and commit the jar" >&2
exit 1
