#!/usr/bin/env bash
# Host JVM checks for the sync probe's Java logic. Gradle isn't needed:
# javac type-checks src/main/java against android.jar, then the checks run on
# the host JVM (android.jar is on the classpath for type resolution only).
# The Kotlin sources (VelocitySyncProbe.kt) are NOT compiled here (no kotlinc):
# keep the Kotlin glue minimal and review it by hand.
set -euo pipefail
cd "$(dirname "$0")"
SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"
# The newest android-NN installed (type resolution only; nothing shipped).
if [ -z "${PLATFORM:-}" ]; then
    best=0
    for d in "$SDK"/platforms/android-*; do
        n=${d##*/android-}
        case $n in '' | *[!0-9]*) continue ;; esac
        [ "$n" -gt "$best" ] && best=$n
    done
    PLATFORM=android-$best
fi
ANDROID_JAR="$SDK/platforms/$PLATFORM/android.jar"
OUT="${OUT:-$(mktemp -d)}"
# find's output as an array, one path per element.
files() {
    FILES=()
    while IFS= read -r -d '' f; do FILES+=("$f"); done < <(find "$1" -name '*.java' -print0 | sort -z)
}
mkdir -p "$OUT/stubs" "$OUT/classes" "$OUT/fakes" "$OUT/test"
# OkHttp: a stub with OkHttp's signatures, so VelocityOkHttpInterceptor (compileOnly
# OkHttp in Gradle) type-checks and runs here.
files src/hostTest/stubs
javac --release 11 -d "$OUT/stubs" "${FILES[@]}"
files src/main/java
javac --release 11 -Xlint:all -Werror -cp "$ANDROID_JAR:$OUT/stubs" -d "$OUT/classes" "${FILES[@]}"
# Test-only fakes with the real class and field names (android.os.MessageQueue,
# React Native's ReactApplication, MountItemDispatcher, JavaTimerManager, ...):
# the probe's reflection runs against them. They shadow android.jar's stubs, so
# they come first on the classpath; the probe itself is compiled without them.
files src/hostTest/fakes
javac --release 11 -cp "$OUT/stubs" -d "$OUT/fakes" "${FILES[@]}"
CP="$OUT/fakes:$OUT/classes:$OUT/stubs:$ANDROID_JAR"
files src/test/java
javac --release 11 -cp "$CP" -d "$OUT/test" "${FILES[@]}"
java -cp "$OUT/test:$CP" com.velocity.sync.HostChecks
