# Atalanta Echo (Android)

A tiny Android test app that shows the intent extras it was launched with. Use it to check end to end that the values in Atalanta's `launchApp: arguments:` reach an app. You need it only when working on or verifying that feature.

Prerequisites: the Android SDK, and an emulator or device visible to `adb`.

## Build

```
./build.sh
```

The build needs `platforms/android-36` and `build-tools/36.1.0` by default;
override them with `PLATFORM` and `BUILD_TOOLS`. The output is
`build/atalanta-echo.apk`.

## Install

```
adb install -r build/atalanta-echo.apk
```

## Run

```
atalanta run tests/apps/android-echo/echo.yaml
```
