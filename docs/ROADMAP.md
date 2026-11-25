# Roadmap

What's planned for the next releases. [STATUS.md](STATUS.md) says what has been verified so far; this page says what's next. Plans change; the [changelog](../CHANGELOG.md) is the record of what shipped.

## 0.2.1: fixes

- AtalantaRunner builds again. The rename to Atalanta changed XCTest's own `withVelocity:` argument, so in 0.2.0 `ios.runner: atalanta` fails to build and only WebDriverAgent works.
- `freezeStatusBar` works on Android 14. SystemUI enters demo mode there but never sets the flag Atalanta read back, so the status bar was masked instead of frozen.
- iOS `clearState` on an app with no data container is no longer reported as a crash, and a failed runner build shows the compiler's errors.
- `flutter analyze` passes on Flutter 3.47 and later.

## 0.3.0: Firefox and Safari

The web driver supports only Chrome and Chromium today. 0.3.0 adds Firefox and Safari, so a web suite can run on all three engines.

- **Firefox** over WebDriver BiDi, through the `BrowserProtocol` seam the Chrome driver already uses. Headless on macOS and Linux, and in CI.
- **Safari** over `safaridriver` (WebDriver classic, plus BiDi where Safari supports it). macOS only, and not headless: Safari has no headless mode.
- **Config:** `web.browser: chrome | firefox | safari`, and `--browser` on `atalanta run`, so one suite can run on each engine in a CI matrix.
- **Parity:** roles and accessible names, frames, popups, dialogs, downloads, file inputs and the settle work on every engine. Where an engine can't do something, such as request mocking in Safari, the step fails as a config error that names the browser; it is never skipped.
- **Mobile web:** device profiles stay Chromium's responsive mode. Real mobile Safari, on an iOS simulator, is a later step.
- **Done when** the web smoke suite and the examples pass on Firefox and Safari in CI, and STATUS.md lists each feature per browser.

## Later

- AtalantaRunner, Wi-Fi iPhones and crash detection on real iPhones.
- Device clouds.
- The GitHub Action and the opt-in device workflows run on GitHub.
- API tests over TLS against a real server; AI providers other than Anthropic against their real APIs.
