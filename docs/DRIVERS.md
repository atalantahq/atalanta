# Drivers

Every platform implements `PlatformDriver` (`crates/velocity-common/src/traits.rs`). The
executor talks only to that trait. Its methods fall into these groups:

- **Lifecycle:** `prepare`, `cleanup`, `health_check`, `restart_session`.
- **Devices:** `list_devices`, `boot_device`, `shutdown_device`.
- **Apps:** install (by hash), launch (optionally clearing state), stop, reset, save and restore state, liveness and crash reports.
- **Tree:** find elements, the hierarchy (with a cache the executor invalidates after actions), visibility and text.
- **Input:** tap, double tap, long press, text, swipe and scroll, keys, `tapXY`, `inputRaw`.
- **Screen:** screenshots, screen size, system bars, status-bar freeze.
- **Sync:** settle on the device, the in-app probe port, network-busy.
- **Web-only:** navigation, back, refresh, mocks. These are errors on other drivers.

Every driver is wrapped in `ResilientDriver` (`resilience.rs`). It adds retries for transient errors and a circuit breaker per device.

> **Verification status.** The device paths are tested on the host against fake adb, WDA,
> devicectl and usbmuxd. On 2026-10-25 the Android and iOS drivers were also run on an Android 14
> emulator and iOS 26 simulators (Xcode 26.0.1); no real device has been used. The web driver is
> tested against real headless Chrome on macOS. Items below marked *(unverified on devices)* have
> never run on hardware. See [STATUS.md](STATUS.md).

## Choosing a driver

Velocity looks for the platform in this order:

1. `--platform`
2. `config.platform`
3. The shape of `--device` (`Platform::of_device_id`):
   - a simulator UUID or an iPhone UDID means iOS;
   - `web` means the web;
   - anything else (`emulator-5554`, a USB serial, `host:port/serial`) means Android.
4. If none of these gives one, it uses iOS.

`driver: device|headless` (`--driver`) and `framework: native|react_native|flutter` (`--framework`; otherwise detected from the app build) pick the implementation:

| driver | framework | Implementation |
|---|---|---|
| device | any | `IosDriver`, `AndroidDriver`, or `WebDriver` (Chrome) |
| headless | native | `velocity-headless` (layout renderer) |
| headless | flutter | `velocity-flutter-bridge` |
| headless | react_native | config error (`RN_NO_HEADLESS`, exit 2) |
| headless | any, on web | usage error |

React Native and Flutter on a device use the ordinary native driver. `framework` there changes only validation warnings.

## Android (`crates/velocity-android`)

- **Transport:**
  - `adb` from PATH, or `VELOCITY_ADB_PATH`.
  - `VELOCITY_ADB_MODE=tcp` speaks the adb wire protocol directly. Installs and forwards still use the binary.
  - `VELOCITY_ADB_SERVERS=host:port,…` adds remote adb servers. Their devices are named `host:port/serial`.
- **Tree server:**
  - Velocity's own on-device server is a small dex jar embedded in the binary (`assets/velocity-tree.jar`; source in `probes/android/velocity-tree-server`).
  - It is pushed to `/data/local/tmp`, run with `app_process`, and reached through `adb forward`.
  - It keeps one UiAutomation connection and returns `uiautomator dump`-format XML in milliseconds.
  - It also injects input, takes screenshots, lists windows and counts accessibility events.
  - Fallback: `uiautomator dump` and `adb shell input`/`screencap`, with backoff. `android.treeServer: 0` forces the fallback.
  - The server holds the device's only UiAutomation slot. Another automation tool on the same device conflicts with it.
  - Protocol 13 (R-2 in [SECURITY.md](SECURITY.md)): every connection sends `AUTH <token>` first. The token is new each time the server starts and is left in `/data/local/tmp/velocity-tree.token` for shell (or root) only; the host reads it through adb. The first line that isn't a command, such as an HTTP request line from a browser's blind POST to the forwarded port, closes the connection.
  - Protocol 12 adds `ENCODERS` (the device's H.264 encoders). The same jar carries the **video server** (`com.velocity.tree.VideoServer`), started as its own `app_process` like scrcpy's server: a virtual display (Android 14+; SurfaceControl before) mirrors the screen into a MediaCodec H.264 encoder (720p long edge, 2 Mbit/s, 30 fps max, a key frame a second), streamed with scrcpy's 12-byte packet header over its own `adb forward`. The host keeps the last `artifacts.videoSeconds` in memory and muxes a failed test's clip to MP4 itself (`mp4.rs`); `AndroidDriver::video_stream` exposes the live stream (`video.rs`). If the encoder can't be used, `adb shell screenrecord` records each test instead. See [CLI.md](CLI.md#failure-evidence).
- **Logs:** per test, `logcat -v threadtime -T <device time>` (`logcat.rs`): the whole log and the app's pids plus the crash buffer; the app's crashes, native crashes and ANRs go into the failure message.
- **Compose:** `id:` also matches a Compose `testTag`, read through accessibility extra data. Turn it off with `android.composeTestTag: 0`. *(Unverified on devices.)*
- **Input:**
  - Text goes in as `ACTION_SET_TEXT` or as key events (`android.text`).
  - Inputs check focus before typing (`android.focusCheck`).
  - `android.scroll: semantic` uses the list's own scroll action.
- **Animations:** turned off on the first launch and restored afterwards, even if Velocity is killed. `android.keepAnimations: 1` leaves them on.
- **Status bar:** `screenshots.freezeStatusBar` uses SystemUI demo mode. *(Unverified on devices.)*
- **saveState:** `run-as … tar`, so it needs a debuggable build. Otherwise it uses an emulator snapshot.
- **Builds:**
  - `.apk`, or a directory of split APKs (`install-multiple`).
  - `.aab` and `.apks` are refused, with bundletool instructions.
  - Installs are skipped when the device already has the same build (a hash marker on the device).
- **Emulators:** `velocity devices up --android N` starts N read-only instances of one AVD.

## iOS (`crates/velocity-ios`)

- **Simulators:** `xcrun simctl`. `velocity devices up --ios N` clones a golden simulator.
- **Automation:** WebDriverAgent over HTTP.
  - On first use Velocity clones WDA at a pinned tag, builds it once per Xcode under `~/.velocity/wda`, and runs one per simulator.
  - **Pinned commits:** v16.14.1 (where the simulator platform ships `lib_TestingInterop.dylib`) and v16.12.10 (otherwise). A clone of a pinned tag must be at the commit Velocity pins: a fresh clone at another commit stops the bootstrap, a cached clone at another commit is cloned again, and a cached build is reused only when it records the pinned commit (`velocity-wda-ref` next to the build). `VELOCITY_WDA_REF` picks another tag, unchecked.
  - **Listening address.** On a simulator (which shares the Mac's network) WDA's HTTP server is bound to `127.0.0.1` (`USE_IP`). Its MJPEG screen stream (`MJPEG_SERVER_PORT`, WDA's port + 1100; the console's iOS mirror) ignores `USE_IP` upstream and has no authentication, so Velocity patches `FBWebServer.m` in its clone to give the stream the same interface before building (`mjpeg-use-ip` in `velocity-wda-ref`; a build from before the patch is rebuilt once). A WDA you start yourself, or one launched by an older Velocity and kept with `VELOCITY_KEEP_WDA`, still serves it on every interface. On a cabled iPhone both bind the phone's loopback, which the usbmux forward reaches; `VELOCITY_IOS_DEVICE_LOOPBACK=0` binds every interface instead (WDA then has no authentication: Velocity warns). Until 2026-10-25 every interface was the default.
  - `VELOCITY_WDA_URL` uses a WDA you run yourself. `VELOCITY_KEEP_WDA=1` leaves it running between runs; the launching run records which build it ran, so a later run that reattaches knows its ref.
  - **One runner per simulator and port.** A kept runner is adopted only by a run that asks for the same runner kind and build (and, for VelocityRunner, has its token). Any other run stops it first (`simctl terminate`), whatever its kind, and launches its own. Before launching, Velocity checks that nothing listens on the port: a runner of the same simulator is stopped; another process is named in a device error (exit 3), never killed. A runner that fails to come up 3 times within 2 minutes is not launched again, and the step fails with a device error naming the last failure, instead of a relaunch every few seconds (a WDA that can't bind its port aborts at once). A run takes the device's lease before it prepares the driver, so it never touches a runner on a device another Velocity process holds.
  - **Session settings** (D-37), sent after each session starts: `snapshotMaxDepth` (`ios.snapshotMaxDepth`, default 60) and `accessibilityDeadline` (`ios.accessibilityDeadlineMs`). Past the deadline WDA aborts the request with an error. The 5000 ms default goes only to a device whose WDA Velocity built from v16.14.1 or later; an older or externally started WDA gets no deadline unless the setting is set.
  - The v16.14.1 tag for newer Xcode 26.x releases is *unverified*. The older tag was checked on Xcode 26.0.1.
- **VelocityRunner** (`ios.runner: velocity` / `VELOCITY_IOS_RUNNER=velocity`):
  - Velocity's own XCUITest runner (`probes/ios/VelocityRunner`). It serves the subset of WDA's HTTP API Velocity uses and builds much faster than WDA.
  - Built with `xcodebuild` and run on iOS 26 simulators (Xcode 26.0.1, 2026-10-25): a cold build took 25 s against WDA's 47 s. *Unverified on real iPhones.*
  - Every request needs the bearer token Velocity makes per launch (`ios.runnerToken` replaces it), a loopback `Host` with the runner's port, and no `Origin` or cross-site `Sec-Fetch-Site` (R-1 in [SECURITY.md](SECURITY.md)). The runner doesn't start without a token, so one you launch yourself for `VELOCITY_WDA_URL` needs `VELOCITY_RUNNER_TOKEN`, and Velocity needs the same value in `ios.runnerToken`.
- **Real iPhones and iPads** *(unverified on devices)*:
  - `xcrun devicectl` installs, launches and copies crash logs.
  - The runner's port is forwarded over an in-process usbmuxd connection (what `iproxy` does).
  - Signing needs `VELOCITY_IOS_TEAM_ID`. `velocity signing` uses an App Store Connect API key instead of an Apple ID.
  - A Wi-Fi-only device needs `ios.allowWifiRunner` and VelocityRunner (whose token is always on).
  - These are simulator-only: `saveState`/`restoreState`, `clearState` (a device warns; use `reset: { strategy: reinstall }`), and the status-bar freeze.
- **Taps:** a tap guard avoids taps on the keyboard and status bar (`VELOCITY_IOS_TAP_GUARD`, `ios.tapGuardStatusBar`).
- **`ios.verifyTap`** (opt-in, D-38; VelocityRunner only) *(run on an iOS 26 simulator; unverified on real iPhones)*:
  - Before each tap the runner asks XCTest's private hit test (`_XCT_requestElementAtPoint:reply:`, every selector checked with `respondsToSelector:`) what is at the point, and finds that element in the app's snapshot by its accessibility element. It sends the element's type, identifier, label, frame, snapshot path and chain from the root.
  - The host places it in its tree by that path: the target or something inside it passes; anything else is drawn over the target and the tap fails, naming it. When XCTest's element is a container of the target, or can't be placed, the deepest node at the point decides, skipping only nodes the snapshot reports with user interaction off (not being an accessibility element or not "hittable" to XCTest does not count: a plain `UIView` dimming overlay is both and still swallows the touch).
  - A point owned by SpringBoard (status bar, system alert) or another process fails, except inside a web view, whose page WebKit's WebContent process draws. When the deepest node decides and it is the bare root of another, empty window (SwiftUI keeps one after the content window on iOS 26), it is looked through. WebDriverAgent has no hit test: the frame rule alone applies, with one warning.
  - Each verified tap costs one app snapshot.
- **Crash watcher** *(unverified on hardware)*: on a real iPhone, the app's state is polled every second after launch; a crash makes later steps fail at once (see "Sticky crash" in [ARCHITECTURE.md](ARCHITECTURE.md)). `restoreState` and `launchApp` still run.
- **System alerts:** handled unless `ios.systemAlerts: 0`.
- **Failure evidence** (simulators): `simctl io recordVideo --codec h264` per test, stopped with SIGINT (which finalizes the file) and kept on failure; `simctl spawn log stream --style compact --level info` for the app's process. Real devices have neither yet.
- **Needs:** macOS, Xcode (`xcrun`, `xcodebuild`), and git for the WDA clone.

## Web (`crates/velocity-web`)

- **Browser:** Chrome or Chromium only, over the DevTools protocol (`chromiumoxide`).
  - Settings: `web.chromePath`, `headless` (default true), `width`/`height` (default 1280×800).
  - A `BrowserProtocol` seam exists for WebDriver BiDi, but no Firefox or Safari adapter is written.
- **Tree:** an injected script in an isolated world returns the DOM as `Element`s, with computed ARIA roles and accessible names.
- **Input:** `Input.dispatch*Event`. With a touch profile, taps and swipes are touch events.
- **Mobile web:** `web.device` takes any Playwright device descriptor name. It sets the viewport, DPR, user agent, mobile layout and touch. This is Chromium's responsive mode, not Safari.
- **Parallel:** `--parallel N` creates `web-0…web-N-1`.
  - The default `web.parallelMode: contexts` uses browser contexts in one Chrome.
  - `processes` starts one Chrome per worker.
- **Network:**
  - The settle waits for network idle (`web.networkIdle`, `web.networkIgnore`).
  - `web.mocks`, `web.block` and `mockRequest` use the Fetch domain.
  - Service workers are bypassed while mocks are active.
- **Failure evidence:** the CDP screencast (`Page.startScreencast` JPEG frames, the last `videoSeconds`) written as `screencast.json` + frames, and console calls, page errors, `Log` entries and failed or 4xx/5xx requests (`console.log`).
- **Cross-origin iframes:** reached over their own DevTools sessions (`web.frames`). Relations such as `below:` and mocks don't reach inside them.
- **Verification:** tested against real headless Chrome on macOS only. Linux is untested.

## Headless (`crates/velocity-headless`, `crates/velocity-flutter-bridge`)

- **Native headless:**
  - A CPU renderer (Taffy layout, tiny-skia paint) for Android layout XML (also read out of an APK) and iOS `.xib`/`.storyboard`. It needs no emulator.
  - It is an approximate smoke renderer:
    - taps do nothing;
    - `inputText` appends;
    - images, shadows, transforms and custom fonts are not drawn (each gives one warning).
  - Config: `headless.width/height/density/fontScale/appPath/initialLayout`.
- **Flutter headless:**
  - A JSON-lines TCP connection to a Dart process running `flutter test` with the `velocity_flutter_helper` package (`crates/velocity-flutter-bridge/dart`). You start the Dart side yourself.
  - It reads the semantics tree and needs Flutter 3.19 or newer.
  - Tested with `flutter test` on the host only.

## In-app sync probes

These are optional debug-build libraries. They tell Velocity when the app is idle. See [ARCHITECTURE.md#sync](ARCHITECTURE.md#sync).

- **iOS** `probes/ios/VelocitySyncProbe` (SwiftPM):
  - Call `VelocitySyncProbe.start()`. It tracks run loop work, animations, URL requests, timers and `performSelector:afterDelay:` work.
  - React Native: with `framework: react_native` it also waits for JS timers (`RCTTiming`), Fabric mounts and RN loading. *Checked against fake ObjC classes only, not a real RN app.*
  - It uses port 19400 (`VELOCITY_SYNC_PORT`). Real devices reach it over usbmux.
  - *Host type-checked only.*
- **Android** `probes/android/velocity-sync-probe`:
  - Call `VelocitySyncProbe.start(this)`. It tracks the main looper, animations and delayed main-looper messages due within the horizon (1.5 s or `sync.maxTimerMs`; `ignoreTimerCallbacks` excludes the app's own pollers).
  - Network: add `VelocityOkHttpInterceptor` to the app's OkHttp client, or report requests yourself.
  - React Native: with `framework: react_native` it also waits for JS timers, RN network calls, native animations and Fabric mounts. *Host-checked with fakes only.*
  - It uses port 19401 through `adb forward`.
  - Both probes speak protocol 4 (`framework` in `config` and the reply, plus `unknown`).
  - *The Kotlin source has not been compiled in a development run, and nothing here has run on a device.*

