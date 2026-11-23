# Atalanta Echo (iOS)

A tiny iOS test app that shows the launch arguments it was started with. Use it
to check end to end that the values in Atalanta's `launchApp: arguments:` reach
an app on the iOS simulator. You need it only when working on or verifying that
feature.

Each argument is passed as `-name value` (booleans as `YES` / `NO`), so the app
can also read it through `UserDefaults`; the `default.<name>` rows show that.

Prerequisites: Xcode, `xcodegen` and a booted simulator.

## Build

```
./build.sh
```

It prints the path to `AtalantaEcho.app`.

## Install and run on a booted simulator

```
xcrun simctl install booted build/Build/Products/Debug-iphonesimulator/AtalantaEcho.app
atalanta run tests/apps/ios-echo/echo.yaml
```
