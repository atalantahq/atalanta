# Contributing to Velocity

Thanks for your interest in contributing to Velocity! This document provides guidelines for contributing.

## Getting Started

1. Fork the repository
2. Clone your fork: `git clone https://github.com/<your-username>/velocity.git`
3. Create a branch: `git checkout -b my-feature`
4. Make your changes
5. Run tests: `cargo test --workspace`
6. Submit a pull request

## Development Setup

### Prerequisites

- Rust 1.89 or newer (the workspace's `rust-version`; CI tests with the version pinned as `RUST_VERSION` in `.github/workflows/ci.yml`). Install via [rustup](https://rustup.rs).
- For Android testing: Android SDK platform-tools with `adb` on `PATH`, and an emulator (`velocity devices up --android 1` boots one from an existing AVD).
- For iOS testing: macOS with Xcode and its command line tools. WebDriverAgent is cloned at a pinned tag and built on the first iOS run (about a minute, once per Xcode).
- For the web driver's tests: Chrome. Without it about 150 web tests skip themselves and still report a pass; CI installs Chrome and fails on any such skip.
- `velocity doctor` (or `cargo run -p velocity-cli -- doctor`) checks all of this.

### Building

```bash
cargo build --workspace
```

The console's web UI is not built by cargo: run `npm ci && npm run build` in `crates/velocity-inspector/frontend` once, or the inspector serves its API with a "UI not built" page.

### Running Tests

```bash
cargo test --workspace
```

CI runs the same tests with `cargo nextest run --workspace --profile ci` (one process per test).

### Running the CLI

```bash
cargo run -p velocity-cli -- validate tests/fixtures/simple_login.yaml
cargo run -p velocity-cli -- run tests/smoke/android-settings.yaml   # needs a booted Android emulator
```

## Code Guidelines

- Before every commit: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings` (CI fails on any warning).
- Add tests for new functionality. Device behaviour that was not run on a device goes in `docs/STATUS.md`.
- A user-visible change gets one line in `CHANGELOG.md` under `## [Unreleased]`; the reasoning goes in the commit message or `docs/research/`.
- Keep commits focused and atomic.

## Architecture

Velocity is a Cargo workspace. The full map is [CLAUDE.md](CLAUDE.md) (architecture section) and [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

| Crate | Purpose |
|-------|---------|
| `velocity-common` | Shared types (`Selector`, `Action`, `Element`), the `PlatformDriver` trait, error classification, retries and the circuit breaker |
| `velocity-core` | YAML parser and validator, selector engine, sync (settle), healing, the executor |
| `velocity-android` | Android driver (adb, the on-device tree server) |
| `velocity-ios` | iOS driver (WebDriverAgent or VelocityRunner, simctl, devicectl for real iPhones) |
| `velocity-web` | Web driver (Chrome DevTools Protocol) |
| `velocity-headless` | Headless rendering of Android XML and iOS XIB layouts, no device |
| `velocity-flutter-bridge` | Flutter widget tests through `flutter test` on the host |
| `velocity-runner` | Suite orchestration, sharding, the device farm and leases, reporters |
| `velocity-api` | The `http` step (API tests) |
| `velocity-nl` | Natural-language steps: model providers, the planner and grounding |
| `velocity-explore` | `velocity explore`: the agent loop that writes a deterministic YAML test |
| `velocity-inspector` | The console (web UI and API) and the baseline review page |
| `velocity-mcp` | The MCP server for AI agents |
| `velocity-doctor` | Environment diagnostics behind `velocity doctor` |
| `velocity-migrate` | Maestro to Velocity YAML converter |
| `velocity-cli` | The `velocity` command |

## Reporting Issues

- Use GitHub Issues
- Include reproduction steps
- Include platform details (OS, device/simulator info)
- Include Velocity version (`velocity version`)

## License

By contributing, you agree that your contributions will be licensed under the GNU Affero General Public License, version 3 only (AGPL-3.0-only).
