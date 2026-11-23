# Atalanta

Cross-platform mobile UI testing framework in Rust. Maestro-style YAML DSL, single static binary, MCP server for AI agents. Workspace of 15 crates under `crates/` + `atalanta-cli/`.

## Build & test

- Build: `cargo build --workspace`
- Test: `cargo test --workspace`
- Lint before committing: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings`
  (run it before every commit, not after CI fails)
- E2E smoke: `/verify-e2e` skill (builds the binary and runs a real suite on a simulator)

## Architecture (orient here first)

- `atalanta-common`: shared types (`Selector`, `Action`, `Element`), `PlatformDriver` trait, error classification (transient → retry, permanent → fail fast), resilience (retry + circuit breaker), live events (`live_events.rs`: the one `ATALANTA_EVENTS_FILE` JSONL channel the MCP tasks and the console tail; keep the executor's hook to its one-line calls)
- `atalanta-core`: YAML parser/validator, `SelectorEngine` (a lookup cache with generation invalidation), `AdaptiveSyncEngine` (`sync.rs`), native sync integration (`native_sync.rs`), system dialog auto-dismiss (`dialog_handler.rs`), self-healing selectors (`healing.rs`), executor
- `atalanta-android` / `atalanta-ios`: `PlatformDriver` impls (ADB shell / WDA HTTP + simctl). iOS keeps one WDA connection per device (`driver.rs` `DeviceConn`); real iPhones go through `devicectl.rs` and the usbmuxd forward in `usbmux.rs` (simulator vs device is decided by UDID shape). WDA is cloned at a pinned tag and patched before its build so its MJPEG stream honours `USE_IP` (`patch_mjpeg_bind` in `wda_bootstrap.rs`)
- `atalanta-runner`: suite orchestration, sharding (count-balanced with stable hash tie-breaks, or duration-balanced with `--shard-timings`), device farm + cross-process leases (`lease.rs`), parallel runner as a supervisor plus one actor per device (retries on another device, quarantine), quarantine list, result cache, reporters (JUnit/JSON/HTML/GitHub/Allure)
- React Native has no crate of its own: `framework: react_native` is a device profile (native drivers + the sync probe). Headless RN is a config error (`validator::RN_NO_HEADLESS`); the old `atalanta-rn-bridge` sidecar was removed
- `atalanta-inspector`: axum web UI + API (the console in `src/console/`: mirror, input under the lease, run and explore tasks as `atalanta` children, doctor diagnostics plugged in by the CLI), plus the server-rendered, token-protected `/review` page for failed `assertScreenshot` baselines (`atalanta baselines review|accept|reject`). The React frontend's `frontend/dist` is built by CI and the release job (`npm ci && npm run build`) and embedded by rust-embed; `build.rs` never runs npm, and a build without `dist/` serves the API with a "UI not built" page
- `atalanta-mcp`: JSON-RPC stdio MCP server for AI agents; background tasks in `src/tasks/` (`run_task` runs `atalanta run` or, with a `goal`, `atalanta explore --json` as a child; `task_inject` appends to its `--inject-file`)
- `atalanta-doctor`: environment diagnostics (`run_checks(&Options) -> Report`, `apply_safe_fixes`) behind `atalanta doctor`; tests fake adb/xcrun/xcodebuild via `Env::from_vars` (a PATH of scripts), never the process env
- `atalanta-migrate`: Maestro → Atalanta YAML converter
- `atalanta-api`: the `http` step (API testing): `HttpSession` sends a request, runs every `expect` check (status, headers, RFC 9535 JSONPath via `serde_json_path`, body, `maxMs`) and its `capture:`; one cookie jar per test. The step types live in `atalanta-common/src/http.rs` (Action needs them). Captured `${NAME}`s are left alone at load time (`env::interpolate_suite` records them in `config.captured_vars`) and filled in by the executor as the test runs (`env::bind_step`). A run whose selected tests have only `http`/`wait`/`setConfig` steps gets `NoDeviceDriver` (stand-ins `http-0…`, never leased; `atalanta_core::http_only_tests`). Docs: `docs/DSL.md#http-and-api-tests`
- `atalanta-nl`: natural-language steps. The planner returns `{op, element}` only; `grounding.rs` builds and checks the selector in code. Keep it that way: never let model output become a selector or a value directly
- `atalanta-explore`: `atalanta explore "<goal>"`. The agent loop on top of the executor and `atalanta-nl` (goal turns: `atalanta-nl/src/goal.rs`), the deterministic YAML writer, and the replay with no planner attached. Same rule: the model returns an op and an element number, and values only by name. Docs: `docs/EXPLORE.md`
- `probes/ios/AtalantaSyncProbe` (Swift) and `probes/android/atalanta-sync-probe`: in-app native sync probes — separate toolchains (xcodebuild / gradle), NOT built by cargo
- `probes/ios/AtalantaRunner` (Swift XCUITest, WDA-compatible HTTP routes): Atalanta's own iOS runner, embedded via `crates/atalanta-ios/src/runner_src.rs` and built on first use with `ATALANTA_IOS_RUNNER=atalanta`. After editing the Swift or `project.yml`, run `xcodegen generate` there and commit the `.xcodeproj`
- `probes/android/atalanta-tree-server` (Java): on-device UI-tree server (protocol 19: dumps, input injection, screenshots, windows, accessibility-event settle, encoders; a per-start token used in a mutual `HELLO`/`AUTH` HMAC handshake (the server proves it holds the token before the host sends anything; the token never crosses the socket), and any non-command line closes the connection) plus `VideoServer.java` (scrcpy-derived H.264 screen stream, its own `app_process`, behind the same kind of token handshake) the Android driver embeds as `crates/atalanta-android/assets/atalanta-tree.jar`. After editing `Server.java`, run its `build.sh` (needs only the Android SDK + JDK) and commit the jar; bump `Server.VERSION` and `SERVER_VERSION` in `tree_server.rs` together when the protocol changes
- Device-backed tests: `ATALANTA_TEST_ANDROID=<serial> cargo test -p atalanta-android` also runs the tree server against a real device (`ATALANTA_TEST_STRICT=1` makes a missing device fail those tests instead of skipping them)

## Conventions

- Docs: `docs/README.md` indexes architecture, DSL guide, drivers, CLI/exit codes, CI, MCP, explore, AI providers and verification status. `CONTEXT.md` and that index were checked against the code on 2026-11-11; the pages themselves were last checked as a set on 2026-10-25 and have been updated piecemeal since. Trust the code over them; update them when behaviour changes. `docs/DSL-REFERENCE.md` is generated from `schema.rs` (`ATALANTA_BLESS=1 cargo test -p atalanta-core --test dsl_reference`) and `docs/MCP.md` must list every MCP tool: tests fail otherwise. Mark anything not run on a device in `docs/STATUS.md`.
- Error handling: classify new errors as transient/permanent/ambiguous in `atalanta-common/src/error.rs` — retry behavior depends on it. Exit codes: 1=test failure, 2=config, 3=device, 4=suite timeout, 5=internal.
- Device regression suites: `tests/smoke/*.yaml` (the platform Settings apps) and `examples/`; run them on a device after sync/selector/dialog changes.
- iOS automation needs macOS + Xcode; Android needs `adb` in PATH (`ATALANTA_ADB_PATH` to override).
