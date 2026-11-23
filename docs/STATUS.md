# Verification status

Atalanta is pre-release. What has been run, where, and what has not. Last updated 2026-11-23.

Each feature has one of four states:

- **Hardware**: run on a real phone or tablet.
- **Verified**: run on an Android emulator, an iOS simulator or real headless Chrome.
- **Host tests**: tested on the development machine against fakes or mock servers, never on a device.
- **Not run**: no run of any kind yet.

## Summary

| Target | State |
|---|---|
| Android emulators (API 33 and 34, arm64) | Verified for most features. Some recent changes are host tests only; they are listed below. |
| iOS simulators (iOS 26, Xcode 26) | Verified through WebDriverAgent and AtalantaRunner. Some recent changes are host tests only. |
| Web | Verified with headless Chrome on macOS, and on Linux in CI. Firefox and Safari are not supported yet. |
| Android hardware | One tablet (Android 14): the Settings app and the trust gates. |
| iPhone hardware | One iPhone 12 (iOS 26.5, wired): Settings, a test app and the trust gates. The Wi-Fi runner and AtalantaRunner have not been run on a phone. |
| API tests | Verified against local HTTP servers on macOS. Not yet run over TLS against a real server. |
| Device clouds | Not run. |

## By feature

### Writing and running tests

| Feature | State | Not yet verified |
|---|---|---|
| Parser, validator, schema, selectors, settle, healing, visual comparison | Host tests | Covered on devices indirectly by the smoke and test-app suites |
| `launchApp` arguments reaching the app | Hardware (iPhone 12), Verified (Android emulator) | — |
| Parallel runs, sharding, retries on another device | Verified | Large device pools |
| `http` steps and API-only suites | Verified (local servers, macOS) | TLS against a real server, Linux |
| Proxies and custom certificates | Host tests | A real proxy |
| Memory check (`config.performance`) | Host tests | The profiler on a device; Android only |
| `exec` step, `drag`, login with keychain and TOTP secrets | Verified (headless Chrome) | Android and iOS |
| Maestro converter | Host tests | Device-only commands (permissions, media, keychain, airplane mode) |
| `validate`, `doctor`, `init` | Verified | — |

### Android

| Feature | State | Not yet verified |
|---|---|---|
| Tree server (UI reads, input, screenshots) | Verified up to protocol 17; Hardware (tablet, Settings) | Protocols 18 and 19 (mutual handshake, active-window check) on a device |
| Video and logs for failures | Verified | The video server's mutual handshake on a device |
| System dialogs and permission prompts | Verified | Localized permission prompts |
| `hideKeyboard`, scrolling with the keyboard up | Verified | Hiding the keyboard before a covered tap; taps forced with `force: true`. iOS has no `hideKeyboard` |
| Status bar freeze (`freezeStatusBar`) | Host tests | Any device |
| In-app sync probe | Host tests | A real app on a device |

### iOS

| Feature | State | Not yet verified |
|---|---|---|
| WebDriverAgent driver | Verified; Hardware (iPhone 12) | — |
| AtalantaRunner (Atalanta's own runner) | Verified | A real iPhone |
| Real iPhones over USB | Hardware (iPhone 12) | — |
| Real iPhones over Wi-Fi | Host tests | Any phone |
| Crash detection on real iPhones | Host tests | Any phone |
| Tap checks (`ios.verifyTap`) | Verified | A real iPhone |
| Content under sheets, popovers and alerts | Hardware (iPhone 12, SwiftUI page sheet) | Popovers and in-app alerts |
| `pressKey: back`, picker wheels | Host tests | Simulator and device |
| In-app sync probe | Host tests | A real app |
| WebDriverAgent screen stream bound to loopback | Builds; not observed at run time | Run-time check |

### Web

| Feature | State | Not yet verified |
|---|---|---|
| Chrome driver: roles, frames, mocks, device profiles, parallel contexts | Verified (macOS and Linux) | — |
| Popups and new tabs, iframe banners, per-key typing | Verified (macOS) | Linux |
| Firefox and Safari | Not built | — |

### Agents and AI

| Feature | State | Not yet verified |
|---|---|---|
| `atalanta explore` with a real model | Verified (trust-benchmark pilot, AndroidWorld runs) | iOS observation geometry; the second-pass repair options |
| Trust gates (`atalanta gate`, `verify-change`) | Hardware (iPhone 12, Android tablet) | — |
| Agent commands (`snapshot`, `tap`, `type`, `export`) | Verified; Hardware (tablet) | — |
| MCP server | Verified (Android emulator) | iOS over MCP |
| `atalanta mcp install` | Host tests | The client apps reading the written config |
| AI providers (Anthropic, OpenAI-compatible, Azure, Bedrock, Vertex) | Host tests against mock servers; the pilot used Anthropic | The other providers against their real APIs |
| Default models | Provisional | `atalanta nl-eval --recommend` with real keys |

### Failure evidence and the console

| Feature | State | Not yet verified |
|---|---|---|
| Video, logs and `trace.html` (Android, iOS simulator, web) | Verified | iOS hardware |
| `--pause-on-failure` | Verified | — |
| Console live mirror | Verified (Android, iOS simulator, web) | iOS hardware |
| Console input, run and explore panels, diagnostics | Verified (Android) | iOS |
| Console HTTP API | Verified | — |
| Screenshot review page (`/review`) | Host tests | A browser run |

### Frameworks

| Feature | State | Not yet verified |
|---|---|---|
| React Native | Host tests (sync probes with React Native's real names) | A React Native app on a device with the probe |
| Flutter bridge | Host tests | A real Flutter app |

### CI and distribution

| Feature | State | Not yet verified |
|---|---|---|
| Opt-in CI workflows (device smoke, Flutter bridge, probes) | Syntax checked | Never run on GitHub |
| GitHub Action (`action.yml`) | Host tests | Never run on GitHub |
| Release packages (install script, Homebrew, npm, crates.io) | Not published | The first release |

## Real hardware

- **iPhone 12** (iOS 26.5, wired), November 2026: the Settings smoke suite, a page-sheet check, the trust gates, and `launchApp` arguments with a test app. Four problems found in the first run were fixed the same day.
- **Android tablet** (Android 14, 1200x1920), November 2026: the agent commands and the trust gates in Settings. The Settings smoke suite is written for phones and stops at a step that the tablet's two-pane layout makes meaningless.

## Performance

Measured on one Apple Silicon Mac with an Android 14 emulator and iOS 26 simulators. Numbers will vary with the machine.

| What | Result |
|---|---|
| Android UI tree read (tree server) | 2.7 ms median, 5.8 ms p90; `uiautomator dump` takes about 2 s |
| Android Settings smoke suite (6 steps) | 2–4 s per test |
| iOS Settings smoke suite (5 steps) | 8–9 s with WebDriverAgent kept running; about 19 s otherwise |
| Cold runner build (Xcode 26) | AtalantaRunner 25 s; WebDriverAgent 47 s |
| `atalanta --version`, `validate` | under 20 ms |
| Peak memory, Android smoke run | about 15 MB |
| Failure evidence overhead (video and logs on) | about 15% per step on an Android emulator |

More measurements and how to reproduce them: [BENCHMARKS.md](BENCHMARKS.md).

## Reporting a result

If you run Atalanta on a device or setup that this page lists as not run, a short issue with what you ran and what happened helps more than anything else at this stage.
