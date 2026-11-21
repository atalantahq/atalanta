# Velocity Echo

Tiny Android test app that shows the intent extras it was launched with, so we can check end to end that Velocity's `launchApp: arguments:` values reach an app.

## Build

```
./build.sh
```

Needs the Android SDK (platforms/android-36 and build-tools/36.1.0 by default,
overridable via `PLATFORM` / `BUILD_TOOLS`). Output: `build/velocity-echo.apk`.

## Install

```
adb install -r build/velocity-echo.apk
```

## Run

```
velocity run tests/apps/android-echo/echo.yaml
```
