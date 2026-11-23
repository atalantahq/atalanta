#!/usr/bin/env bash
# Build the Atalanta tree server into a dex jar and copy it where the Rust
# Android driver embeds it (crates/atalanta-android/assets/atalanta-tree.jar).
#
# Needs only the Android SDK (the platform and build-tools in toolchain.env)
# and a JDK (toolchain.env's TREE_JDK; javac output feeds d8) — no Gradle.
# Re-run after editing Server.java and commit the updated jar.
set -euo pipefail
cd "$(dirname "$0")"
SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"
# Pinned, not "the newest installed": the jar must be reproducible
# (scripts/check-tree-jar.sh, run by CI). PLATFORM / BUILD_TOOLS override.
# shellcheck disable=SC1091  # toolchain.env, next to this script
. ./toolchain.env
PLATFORM="${PLATFORM:-$TREE_PLATFORM}"
BUILD_TOOLS="${BUILD_TOOLS:-$TREE_BUILD_TOOLS}"
ANDROID_JAR="$SDK/platforms/$PLATFORM/android.jar"
D8="$SDK/build-tools/$BUILD_TOOLS/d8"
for f in "$ANDROID_JAR" "$D8"; do
    [ -e "$f" ] || { echo "missing $f: sdkmanager \"platforms;$PLATFORM\" \"build-tools;$BUILD_TOOLS\"" >&2; exit 1; }
done

# find's output as an array, one path per element, sorted the way
# scripts/check-tree-jar.sh sorts it (the order d8 sees can change the
# jar's bytes). bash 3.2 (macOS) has no mapfile.
files() {
    FILES=()
    while IFS= read -r -d '' f; do FILES+=("$f"); done \
        < <(find "$1" -name "$2" -print0 | sort -z)
}

rm -rf build && mkdir -p build/classes
files src '*.java'
javac --release 11 -cp "$ANDROID_JAR" -d build/classes "${FILES[@]}"
# Pure helpers checked on the host JVM before packaging.
mkdir -p build/test
files test '*.java'
javac --release 11 -cp "build/classes:$ANDROID_JAR" -d build/test "${FILES[@]}"
java -cp "build/test:build/classes:$ANDROID_JAR" com.atalanta.tree.HostChecks
files build/classes '*.class'
"$D8" --release --min-api 21 --lib "$ANDROID_JAR" --output build/atalanta-tree.jar \
    "${FILES[@]}"
OUT=../../../crates/atalanta-android/assets
mkdir -p "$OUT"
cp build/atalanta-tree.jar "$OUT/atalanta-tree.jar"
echo "built $(wc -c < "$OUT/atalanta-tree.jar") bytes -> $OUT/atalanta-tree.jar ($PLATFORM, build-tools $BUILD_TOOLS)"
