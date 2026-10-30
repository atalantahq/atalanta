# Velocity

Cross-platform mobile UI testing framework in Rust. Maestro-style YAML DSL, single static binary, MCP server for AI agents. Workspace of 15 crates under `crates/` + `velocity-cli/`.

## Build & test

- Build: `cargo build --workspace`
- Test: `cargo test --workspace`
- Lint before committing: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings`
  (history is full of follow-up clippy-fix commits — run this before every commit, not after CI fails)
- E2E smoke: `/verify-e2e` skill (builds the binary and runs a real suite on a simulator)

## Architecture (orient here first)

- `velocity-common`: shared types (`Selector`, `Action`, `Element`), `PlatformDriver` trait, error classification (transient → retry, permanent → fail fast), resilience (retry + circuit breaker), live events (`live_events.rs`: the one `VELOCITY_EVENTS_FILE` JSONL channel the MCP tasks and the console tail; keep the executor's hook to its one-line calls)
- `velocity-core`: YAML parser/validator, `SelectorEngine` (a lookup cache with generation invalidation), `AdaptiveSyncEngine` (`sync.rs`), native sync integration (`native_sync.rs`), system dialog auto-dismiss (`dialog_handler.rs`), self-healing selectors (`healing.rs`), executor
- `velocity-android` / `velocity-ios`: `PlatformDriver` impls (ADB shell / WDA HTTP + simctl). iOS keeps one WDA connection per device (`driver.rs` `DeviceConn`); real iPhones go through `devicectl.rs` and the usbmuxd forward in `usbmux.rs` (simulator vs device is decided by UDID shape). WDA is cloned at a pinned tag and patched before its build so its MJPEG stream honours `USE_IP` (`patch_mjpeg_bind` in `wda_bootstrap.rs`)
- `velocity-runner`: suite orchestration, sharding (count-balanced with stable hash tie-breaks, or duration-balanced with `--shard-timings`), device farm + cross-process leases (`lease.rs`), parallel runner as a supervisor plus one actor per device (retries on another device, quarantine), quarantine list, result cache, reporters (JUnit/JSON/HTML/GitHub/Allure)
- React Native has no crate of its own: `framework: react_native` is a device profile (native drivers + the sync probe). Headless RN is a config error (`validator::RN_NO_HEADLESS`); the old `velocity-rn-bridge` sidecar was removed (D-44/45)
- `velocity-inspector`: axum web UI + API (the console in `src/console/`: mirror, input under the lease, run and explore tasks as `velocity` children, doctor diagnostics plugged in by the CLI), plus the server-rendered, token-protected `/review` page for failed `assertScreenshot` baselines (`velocity baselines review|accept|reject`, U1). The React frontend's `frontend/dist` is built by CI and the release job (`npm ci && npm run build`) and embedded by rust-embed; `build.rs` never runs npm, and a build without `dist/` serves the API with a "UI not built" page (D-42)
- `velocity-mcp`: JSON-RPC stdio MCP server for AI agents; background tasks in `src/tasks/` (`run_task` runs `velocity run` or, with a `goal`, `velocity explore --json` as a child; `task_inject` appends to its `--inject-file`)
- `velocity-doctor`: environment diagnostics (`run_checks(&Options) -> Report`, `apply_safe_fixes`) behind `velocity doctor`; tests fake adb/xcrun/xcodebuild via `Env::from_vars` (a PATH of scripts), never the process env
- `velocity-migrate`: Maestro → Velocity YAML converter
- `velocity-api`: the `http` step (API testing): `HttpSession` sends a request, runs every `expect` check (status, headers, RFC 9535 JSONPath via `serde_json_path`, body, `maxMs`) and its `capture:`; one cookie jar per test. The step types live in `velocity-common/src/http.rs` (Action needs them). Captured `${NAME}`s are left alone at load time (`env::interpolate_suite` records them in `config.captured_vars`) and filled in by the executor as the test runs (`env::bind_step`). A run whose selected tests have only `http`/`wait`/`setConfig` steps gets `NoDeviceDriver` (stand-ins `http-0…`, never leased; `velocity_core::http_only_tests`). Docs: `docs/DSL.md#http-and-api-tests`, rationale `docs/research/landscape-api-testing.md`
- `velocity-nl`: natural-language steps. The planner returns `{op, element}` only; `grounding.rs` builds and checks the selector in code. Keep it that way: never let model output become a selector or a value directly
- `velocity-explore`: `velocity explore "<goal>"`. The agent loop on top of the executor and `velocity-nl` (goal turns: `velocity-nl/src/goal.rs`), the deterministic YAML writer, and the replay with no planner attached. Same rule: the model returns an op and an element number, and values only by name. Docs: `docs/EXPLORE.md`
- `probes/ios/VelocitySyncProbe` (Swift) and `probes/android/velocity-sync-probe`: in-app native sync probes — separate toolchains (xcodebuild / gradle), NOT built by cargo
- `probes/ios/VelocityRunner` (Swift XCUITest, WDA-compatible HTTP routes): Velocity's own iOS runner, embedded via `crates/velocity-ios/src/runner_src.rs` and built on first use with `VELOCITY_IOS_RUNNER=velocity`. After editing the Swift or `project.yml`, run `xcodegen generate` there and commit the `.xcodeproj`
- `probes/android/velocity-tree-server` (Java): on-device UI-tree server (protocol 13: dumps, input injection, screenshots, windows, accessibility-event settle, encoders; a per-start `AUTH` token, and any non-command line closes the connection) plus `VideoServer.java` (scrcpy-derived H.264 screen stream, its own `app_process`) the Android driver embeds as `crates/velocity-android/assets/velocity-tree.jar`. After editing `Server.java`, run its `build.sh` (needs only the Android SDK + JDK) and commit the jar; bump `Server.VERSION` and `SERVER_VERSION` in `tree_server.rs` together when the protocol changes
- Device-backed tests: `VELOCITY_TEST_ANDROID=<serial> cargo test -p velocity-android` also runs the tree server against a real device

## Conventions

- Docs: `docs/README.md` (architecture, DSL guide, drivers, CLI/exit codes, CI, MCP, explore, AI providers, verification status) and `CONTEXT.md` were checked against the code on 2026-10-25. Trust the code over them; update them when behaviour changes. `docs/DSL-REFERENCE.md` is generated from `schema.rs` (`VELOCITY_BLESS=1 cargo test -p velocity-core --test dsl_reference`) and `docs/MCP.md` must list every MCP tool: tests fail otherwise. Mark anything not run on a device in `docs/STATUS.md`.
- Error handling: classify new errors as transient/permanent/ambiguous in `velocity-common/src/error.rs` — retry behavior depends on it. Exit codes: 1=test failure, 2=config, 3=device, 4=suite timeout, 5=internal.
- Design rationale and the literature behind sync, healing, NL steps and scheduling: `docs/research/README.md` (includes the prioritised "not done yet" list).
- Device regression suites: `tests/smoke/*.yaml` (the platform Settings apps) and `examples/`; run them on a device after sync/selector/dialog changes.
- iOS automation needs macOS + Xcode; Android needs `adb` in PATH (`VELOCITY_ADB_PATH` to override).
