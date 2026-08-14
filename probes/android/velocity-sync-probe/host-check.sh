#!/usr/bin/env bash
# Host JVM checks for the sync probe's Java logic. Gradle isn't needed:
# javac type-checks src/main/java against android.jar, then the checks run on
# the host JVM (android.jar is on the classpath for type resolution only).
# The Kotlin sources (VelocitySyncProbe.kt) are NOT compiled here (no kotlinc):
# keep the Kotlin glue minimal and review it by hand.
set -euo pipefail
cd "$(dirname "$0")"
SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"
PLATFORM="${PLATFORM:-$(ls "$SDK/platforms" | grep -E '^android-[0-9]+$' | sort -t- -k2 -n | tail -1)}"
ANDROID_JAR="$SDK/platforms/$PLATFORM/android.jar"
OUT="${OUT:-$(mktemp -d)}"
mkdir -p "$OUT/stubs" "$OUT/classes" "$OUT/fakes" "$OUT/test"
# OkHttp: a stub with OkHttp's signatures, so VelocityOkHttpInterceptor (compileOnly
# OkHttp in Gradle) type-checks and runs here.
javac --release 11 -d "$OUT/stubs" $(find src/hostTest/stubs -name '*.java')
javac --release 11 -Xlint:all -Werror -cp "$ANDROID_JAR:$OUT/stubs" -d "$OUT/classes" $(find src/main/java -name '*.java')
# Test-only fakes with the real class and field names (android.os.MessageQueue,
# React Native's ReactApplication, MountItemDispatcher, JavaTimerManager, ...):
# the probe's reflection runs against them. They shadow android.jar's stubs, so
# they come first on the classpath; the probe itself is compiled without them.
javac --release 11 -cp "$OUT/stubs" -d "$OUT/fakes" $(find src/hostTest/fakes -name '*.java')
CP="$OUT/fakes:$OUT/classes:$OUT/stubs:$ANDROID_JAR"
javac --release 11 -cp "$CP" -d "$OUT/test" $(find src/test/java -name '*.java')
java -cp "$OUT/test:$CP" com.velocity.sync.HostChecks
