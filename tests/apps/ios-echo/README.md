# Velocity Echo

Tiny iOS test app that shows the launch arguments it was started with. It lets us
check end to end that Velocity's `launchApp: arguments:` values reach an app on
the iOS simulator. Each argument is passed as `-name value` (booleans as `YES` / `NO`),
so the app can also read it through `UserDefaults`; the `default.<name>` rows show that.

## Build

```
./build.sh
```

Needs Xcode and `xcodegen`. It prints the path to `VelocityEcho.app`.

## Install and run on a booted simulator

```
xcrun simctl install booted build/Build/Products/Debug-iphonesimulator/VelocityEcho.app
velocity run tests/apps/ios-echo/echo.yaml
```
