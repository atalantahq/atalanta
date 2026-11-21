# Velocity: context for AI agents

Velocity is a UI testing framework for Android, iOS and the web, written in Rust. Tests are YAML suites in a Maestro-like DSL. It ships as one `velocity` binary, and it has an MCP server so AI agents can drive devices. License: AGPL-3.0-only.

Read the code before trusting any doc. This file was checked against the code on 2026-11-11; [docs/](docs/README.md) pages carry their own dates.

## Where things are

- `velocity-cli/`: the binary. Commands are in `src/commands/*.rs`: init, doctor, run, validate, lint, schema, settings, shard-plan, device(s), inspect, baselines, heal, history, results, trace, repl, explore, verify-change, gate, login, api, mcp, migrate, nl-eval, signing, daemon, version, and the agent commands (`agent.rs`: snapshot, tap, type, press, swipe, scroll, launch, stop, assert, wait, screenshot, export, session, skills).
- `crates/velocity-common`: shared types (`Selector`, `Action`, `Element`, `Step`), the `PlatformDriver` trait, `VelocityError` with error kinds and exit codes, the settings table, resilience (retry and circuit breaker), redaction, and live events (`live_events`: with `VELOCITY_EVENTS_FILE` set, one JSON line per test and step start and end, plus explore's turns; the MCP tasks and the console read it).
- `crates/velocity-core`:
  - parsing and checking: parser, validator, JSON Schema (`schema.rs`), the generated DSL reference (`reference.rs`);
  - running: executor, selector engine, sync (`sync.rs`, `native_sync.rs`), dialog handler, healing;
  - screenshots: visual diff and baselines (`visual.rs`, `baselines.rs`);
  - other: traces and their `trace.html` viewer (`trace_html.rs`), impact selection.
- `crates/velocity-nl`: natural-language steps. The model returns `{op, element}`. `grounding.rs` builds the selector and checks it in code.
  - Providers are presets over three wire formats (`provider.rs`; [docs/AI-PROVIDERS.md](docs/AI-PROVIDERS.md)).
  - The cache is written only for plans the run verified, and keyed without test data.
  - `checker.rs` runs `verify:` checks: a verdict must cite real elements.
- `crates/velocity-doctor`: the environment checks behind `velocity doctor` (`run_checks` → `Report`, `apply_safe_fixes`), shared with the MCP `diagnose` tool and the console's diagnostics (both in process). Every external command has a deadline; secrets are reported by name only.
- `crates/velocity-explore`: `velocity explore`. An agent loop (observe, plan one op and element number, ground in code, act through the executor, record), the YAML writer (assertions on new screens and on the checker's evidence; values as `${NAME}`), the zero-model replay, and one repair round. It is a library (`ExploreJob`, `ExploreSession`, events, inject and stop) behind the CLI; MCP (`run_task` with a `goal`) and the console run `velocity explore --json` as a child. [docs/EXPLORE.md](docs/EXPLORE.md).
- `crates/velocity-runner`: scheduling, sharding, parallel devices (a supervisor plus one actor per device), retries, quarantine, result cache, history, leases, reporters (JUnit, JSON, HTML, GitHub, Allure).
- Drivers:
  - `crates/velocity-android`: adb plus an embedded on-device tree server; the same jar has a scrcpy-style screen video server (`video.rs`, `mp4.rs`) and logcat capture (`logcat.rs`) for failure evidence.
  - `crates/velocity-ios`: simctl and devicectl, plus WebDriverAgent (built from a pinned clone, patched so its MJPEG stream honours `USE_IP`) or Velocity's own runner.
  - `crates/velocity-web`: Chrome over CDP.
  - `crates/velocity-headless` and `crates/velocity-flutter-bridge`: headless.
- `crates/velocity-inspector`: an axum web UI and API (the console in `src/console/`: live device mirror, input under the device lease, suite runs and explore sessions as child processes with live events, saving an explored test into the project, doctor diagnostics, tasks), a token-protected `/review` page for screenshot baselines, and an embedded React build.
- `crates/velocity-mcp`: JSON-RPC over stdio. It serves 5 tools by default (`profile.rs`: `observe`, `act`, `run`, `artifacts`, `verify_change`) and 33 with `--profile full`, background tasks (`run_task`: a suite run, or an exploration with `goal`, steered by `task_inject`), compact snapshots with session refs, `diagnose` (`velocity-doctor`), and `velocity mcp install` for agent clients.
- `crates/velocity-migrate`: converts Maestro flows to Velocity.
- `crates/velocity-api`: API testing, the `http` step. It sends a request, checks status, headers, JSONPath values (RFC 9535) and the body, and captures values as `${NAME}` for the test's later steps, UI steps included. A suite of only `http` steps runs with no device (`NoDeviceDriver`). The step types are in `velocity-common/src/http.rs`. [docs/DSL.md](docs/DSL.md#http-and-api-tests).
- `probes/`: native code that cargo does not build: the Android tree server (Java), VelocityRunner (Swift XCUITest), and the in-app sync probes.
- `tests/smoke/`: Settings-app smoke suites for Android and iOS, and a web page suite. `examples/web-example/`: a web sample.

## Rules worth knowing

- **Errors:** classify each new error in `velocity-common/src/error.rs`. The kind decides retries and the exit code: 1 test failure, 2 config, 3 device, 4 suite timeout, 5 internal. Signals exit 128 plus the signal number.
- **Settings:** new driver or runtime settings go in `velocity-common/src/settings.rs`. They show up in `velocity settings`, `--set` and the DSL reference.
- **New DSL keys:** add them to `schema.rs`, then regenerate `docs/DSL-REFERENCE.md` with `VELOCITY_BLESS=1 cargo test -p velocity-core --test dsl_reference`. Tests fail when the schema, the parser and that file disagree.
- **MCP tools:** each one needs a section in `docs/MCP.md`, which a test checks.
- **Natural-language steps:** never let model output become a selector or a value directly.
- **Screenshot baselines:** they live at `<suite dir>/velocity-baselines/<platform>[-headless]/<W>x<H>/<name>.png`.
  - A missing baseline fails the step after writing it.
  - A failure leaves review files under `<artifacts>/diffs/`, which `velocity baselines` accepts.
- **Verification:** device code is tested against fakes, and the web against real headless Chrome. On 2026-10-25 the drivers also ran on an Android 14 emulator and iOS 26 simulators. One real Android device has been used since (a tablet, the Settings app only, 2026-11-08); a real iPhone never has. See [docs/STATUS.md](docs/STATUS.md) before claiming device behaviour.

## Build

```bash
cargo build -p velocity-cli
cargo test -p <crate>
cargo fmt --all && cargo clippy -p <crate> --all-targets -- -D warnings
```

Docs: [docs/README.md](docs/README.md). Design rationale: [docs/research/README.md](docs/research/README.md).
