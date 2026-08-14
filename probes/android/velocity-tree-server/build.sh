#!/usr/bin/env bash
# Build the Velocity tree server into a dex jar and copy it where the Rust
# Android driver embeds it (crates/velocity-android/assets/velocity-tree.jar).
#
# Needs only the Android SDK (platforms + build-tools) and a JDK 11+ — no Gradle.
# Re-run after editing Server.java and commit the updated jar.
set -euo pipefail
cd "$(dirname "$0")"
SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"
PLATFORM="${PLATFORM:-$(ls "$SDK/platforms" | grep -E '^android-[0-9]+$' | sort -t- -k2 -n | tail -1)}"
BUILD_TOOLS="${BUILD_TOOLS:-$(ls "$SDK/build-tools" | sort -V | tail -1)}"
ANDROID_JAR="$SDK/platforms/$PLATFORM/android.jar"
D8="$SDK/build-tools/$BUILD_TOOLS/d8"

rm -rf build && mkdir -p build/classes
javac --release 11 -cp "$ANDROID_JAR" -d build/classes $(find src -name '*.java')
# Pure helpers checked on the host JVM before packaging.
mkdir -p build/test
javac --release 11 -cp "build/classes:$ANDROID_JAR" -d build/test $(find test -name '*.java')
java -cp "build/test:build/classes:$ANDROID_JAR" com.velocity.tree.HostChecks
"$D8" --release --min-api 21 --lib "$ANDROID_JAR" --output build/velocity-tree.jar \
    $(find build/classes -name '*.class')
OUT=../../../crates/velocity-android/assets
mkdir -p "$OUT"
cp build/velocity-tree.jar "$OUT/velocity-tree.jar"
echo "built $(wc -c < "$OUT/velocity-tree.jar") bytes -> $OUT/velocity-tree.jar ($PLATFORM, build-tools $BUILD_TOOLS)"
