# Contributing to Atalanta

## Getting started

1. Fork the repository
2. Clone your fork: `git clone https://github.com/<your-username>/atalanta.git`
3. Create a branch: `git checkout -b my-feature`
4. Make your changes
5. Run tests: `cargo test --workspace`
6. Submit a pull request

## Development setup

### Prerequisites

- Rust 1.89 or newer, the workspace's `rust-version`. CI tests with the version pinned as `RUST_VERSION` in `.github/workflows/ci.yml`. Install it with [rustup](https://rustup.rs).
- For Android testing: Android SDK platform-tools with `adb` on `PATH`, and an emulator. `atalanta devices up --android 1` boots one from an existing AVD.
- For iOS testing: macOS with Xcode and its command line tools. WebDriverAgent is cloned at a pinned tag and built on the first iOS run (about a minute, once per Xcode).
- For the web driver's tests: Chrome. Without it about 150 web tests skip themselves and still report a pass. CI installs Chrome and fails on any such skip.
- `atalanta doctor` (or `cargo run -p atalanta-cli -- doctor`) checks all of this.

### Building

```bash
cargo build --workspace
```

Cargo does not build the console's web UI. Run `npm ci && npm run build` in `crates/atalanta-inspector/frontend` once. Without it the inspector serves its API and a "UI not built" page.

### Running tests

```bash
cargo test --workspace
```

CI runs the same tests with `cargo nextest run --workspace --profile ci` (one process per test).

### Running the CLI

```bash
cargo run -p atalanta-cli -- validate tests/fixtures/simple_login.yaml
cargo run -p atalanta-cli -- run tests/smoke/android-settings.yaml   # needs a booted Android emulator
```

## Code guidelines

- Before every commit: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings` (CI fails on any warning).
- Add tests for new functionality. Device behaviour that was not run on a device goes in `docs/STATUS.md`.
- A user-visible change gets one line in `CHANGELOG.md` under `## [Unreleased]`; the reasoning goes in the commit message.
- Keep commits focused and atomic.

## Architecture

Atalanta is a Cargo workspace. The full map is [CLAUDE.md](CLAUDE.md) (architecture section) and [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

| Crate | Purpose |
|-------|---------|
| `atalanta-common` | Shared types (`Selector`, `Action`, `Element`), the `PlatformDriver` trait, error classification, retries and the circuit breaker |
| `atalanta-core` | YAML parser and validator, selector engine, sync (settle), healing, the executor |
| `atalanta-android` | Android driver (adb, the on-device tree server) |
| `atalanta-ios` | iOS driver (WebDriverAgent or AtalantaRunner, simctl, devicectl for real iPhones) |
| `atalanta-web` | Web driver (Chrome DevTools Protocol) |
| `atalanta-headless` | Headless rendering of Android XML and iOS XIB layouts, no device |
| `atalanta-flutter-bridge` | Flutter widget tests through `flutter test` on the host |
| `atalanta-runner` | Suite orchestration, sharding, the device farm and leases, reporters |
| `atalanta-api` | The `http` step (API tests) |
| `atalanta-nl` | Natural-language steps: model providers, the planner and grounding |
| `atalanta-explore` | `atalanta explore`: the agent loop that writes a deterministic YAML test |
| `atalanta-inspector` | The console (web UI and API) and the baseline review page |
| `atalanta-mcp` | The MCP server for AI agents |
| `atalanta-doctor` | Environment diagnostics behind `atalanta doctor` |
| `atalanta-migrate` | Maestro to Atalanta YAML converter |
| `atalanta-cli` | The `atalanta` command |

## Reporting issues

- Use GitHub Issues; the bug report form asks for what is listed below. [SUPPORT.md](SUPPORT.md) says where else to go. For a security problem, do not open an issue: follow [SECURITY.md](SECURITY.md).
- Include reproduction steps
- Include platform details (OS, device or simulator)
- Include the Atalanta version (`atalanta version`)

## Conduct

Everyone taking part follows the [code of conduct](CODE_OF_CONDUCT.md).

## License

By contributing, you agree that your contributions will be licensed under the GNU Affero General Public License, version 3 only (AGPL-3.0-only).
