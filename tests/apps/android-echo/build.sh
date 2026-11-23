#!/usr/bin/env bash
# Build the Atalanta Echo test app (tests/apps/android-echo) into an APK,
# the same way as probes/android/atalanta-tree-server/build.sh: no Gradle.
set -euo pipefail
cd "$(dirname "$0")"
SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"
# Pinned, same as the tree server: TREE_PLATFORM / TREE_BUILD_TOOLS, overridable
# with PLATFORM / BUILD_TOOLS.
# shellcheck disable=SC1091  # toolchain.env, next to the tree-server script
. ../../../probes/android/atalanta-tree-server/toolchain.env
PLATFORM="${PLATFORM:-$TREE_PLATFORM}"
BUILD_TOOLS="${BUILD_TOOLS:-$TREE_BUILD_TOOLS}"
ANDROID_JAR="$SDK/platforms/$PLATFORM/android.jar"
AAPT2="$SDK/build-tools/$BUILD_TOOLS/aapt2"
D8="$SDK/build-tools/$BUILD_TOOLS/d8"
ZIPALIGN="$SDK/build-tools/$BUILD_TOOLS/zipalign"
APKSIGNER="$SDK/build-tools/$BUILD_TOOLS/apksigner"
for f in "$AAPT2" "$D8" "$ZIPALIGN" "$APKSIGNER" "$ANDROID_JAR"; do
    [ -e "$f" ] || { echo "missing $f: sdkmanager \"platforms;$PLATFORM\" \"build-tools;$BUILD_TOOLS\"" >&2; exit 1; }
done

rm -rf build
mkdir -p build/classes

"$AAPT2" link -o build/base.apk --manifest AndroidManifest.xml -I "$ANDROID_JAR" --min-sdk-version 21 --target-sdk-version 34

javac --release 11 -cp "$ANDROID_JAR" -d build/classes src/dev/atalanta/echo/MainActivity.java

"$D8" --release --min-api 21 --lib "$ANDROID_JAR" --output build/ build/classes/dev/atalanta/echo/MainActivity.class

( cd build && zip -q -j base.apk classes.dex )

"$ZIPALIGN" -f 4 build/base.apk build/aligned.apk

keytool -genkeypair \
    -alias echo \
    -storepass android \
    -keypass android \
    -keyalg RSA \
    -keysize 2048 \
    -validity 10000 \
    -dname "CN=Atalanta Echo" \
    -keystore build/debug.keystore

"$APKSIGNER" sign \
    --ks build/debug.keystore \
    --ks-pass pass:android \
    --key-pass pass:android \
    --out build/atalanta-echo.apk \
    build/aligned.apk

echo "built $(wc -c < build/atalanta-echo.apk) bytes -> tests/apps/android-echo/build/atalanta-echo.apk"
