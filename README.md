# Atalanta

[![npm](https://img.shields.io/npm/v/@atalantahq/cli?label=npm&color=blue)](https://www.npmjs.com/package/@atalantahq/cli)
[![Android](https://img.shields.io/badge/android-API_33--34-blue?logo=android)](docs/DRIVERS.md)
[![iOS](https://img.shields.io/badge/ios-26-blue?logo=apple)](docs/DRIVERS.md)
[![Chromium](https://img.shields.io/badge/chromium-headless-blue?logo=googlechrome)](docs/DRIVERS.md)

[![CI](https://github.com/atalantahq/atalanta/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/atalantahq/atalanta/actions/workflows/ci.yml)
[![Security](https://github.com/atalantahq/atalanta/actions/workflows/security.yml/badge.svg?branch=main)](https://github.com/atalantahq/atalanta/actions/workflows/security.yml)
[![OpenSSF Best Practices](https://www.bestpractices.dev/projects/17034/badge)](https://www.bestpractices.dev/projects/17034)
[![Licence](https://img.shields.io/badge/licence-AGPL--3.0--only-blue)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.89%2B-blue?logo=rust)](Cargo.toml)
[![crates.io](https://img.shields.io/crates/v/atalanta-cli?logo=rust&color=blue)](https://crates.io/crates/atalanta-cli)
[![Dependencies](https://deps.rs/repo/github/atalantahq/atalanta/status.svg)](https://deps.rs/repo/github/atalantahq/atalanta)
[![Fuzzing](https://img.shields.io/badge/fuzzing-cargo--fuzz-blue?logo=rust)](fuzz/)

**Mobile, web and API tests in YAML.** Atalanta runs test suites on Android emulators, iOS simulators and Chrome from one static binary written in Rust. No JVM, no Node, no Appium server.

An AI agent can write the tests. Each one is checked before it is saved, and it replays in CI without calling a model.

Atalanta is pre-release: 0.2.0 has not been published yet. [docs/STATUS.md](docs/STATUS.md) lists what has been verified, and on which devices.

## Features

- Write tests in YAML. Every step waits for its element and for the screen to settle, so suites need no sleeps.
- Let an agent write them: `atalanta explore "<goal>"` drives the app and saves the session as a YAML test.
- An agent-written test is kept only if it passes five replays with no model and fails when its last tap is moved to a neighbouring element.
- Saved tests make no model calls. The model picks an operation and an element number; code builds every selector and fills every value.
- Test APIs in the same suites. An `http` step checks the response and captures values for later UI steps.
- A failed test keeps a video, the device and app logs, and one `trace.html` with each step's screenshot and selector. No cloud account.
- A local console mirrors every device and browser, shows the selector under the cursor, and runs suites and agent sessions.
- Agents can drive devices through an MCP server or plain shell commands.
- Run in parallel across devices, shard by past durations, or run only the tests a change can affect.
- Use any of 19 AI providers, local models included.

### Platforms

| Platform | Driven with | Requires | Verified on |
|---|---|---|---|
| Android | adb and Atalanta's on-device UI server | Android SDK platform-tools | Emulators (API 33, 34); one Android 14 tablet (Settings) |
| iOS | WebDriverAgent or Atalanta's own XCUITest runner | macOS with Xcode | iOS 26 simulators; one wired iPhone 12 |
| Web | Chrome DevTools Protocol | Chrome or Chromium | Headless Chrome on macOS and Linux |
| API | HTTP | Nothing | |

Windows is not supported. WSL2 works as Linux.

## Install

Until the first release, build from a checkout:

```bash
cargo install --path atalanta-cli --locked
```

Once released:

```bash
curl -fsSL https://raw.githubusercontent.com/atalantahq/atalanta/main/install.sh | sh   # macOS / Linux
brew install atalantahq/tap/atalanta
npm install --save-dev @atalantahq/cli
cargo install atalanta-cli --locked
```

Every route installs the same release. The first iOS run builds WebDriverAgent, which takes about a minute once per Xcode version.

## Quick start

Start an emulator or simulator with your app installed, then:

```bash
atalanta doctor          # checks adb, Xcode, devices, Chrome and AI keys; --fix applies the safe fixes
atalanta init --run      # writes a first test from the app's launch screen and runs it
```

Let an agent write a test for a goal, then run it:

```bash
atalanta explore "open Network & internet" --app-id com.android.settings
atalanta run open-network-internet.yaml
```

Run in CI:

```bash
atalanta run tests/ --parallel 4 --report junit,github --retries 1
atalanta run tests/ --shard 2/4 --shard-timings history/
atalanta run tests/ --changed-since origin/main
```

`atalanta init` also writes a sharded GitHub Actions example to `atalanta/ci/`. See [docs/CI.md](docs/CI.md).

## Example

```yaml
appId: com.example.app
tests:
  - name: Log in and open an order
    steps:
      - launchApp: { clearState: true }
      - inputText: { selector: { id: email }, text: "qa@example.com" }
      - inputText: { selector: { id: password }, text: "${LOGIN_PASSWORD}" }
      - tap: { id: login_button }
      - assertVisible: { text: "Welcome" }
      - do: "open the first order"          # natural language, cached once verified
      - http:
          method: GET
          url: https://api.example.com/orders/latest
          expect: { status: 200 }
          capture: { ORDER_ID: id }
      - assertVisible: { text: "${ORDER_ID}" }
```

Secrets are typed by name (`${LOGIN_PASSWORD}`) and masked in every report, log and trace. The full format is in [docs/DSL.md](docs/DSL.md).

## Console

```bash
atalanta inspect --open
```

One browser page for every booted device and the web browser. It has a live mirror you can click and type on, an element overlay that shows the selector Atalanta would use, and panels to run suites, watch an agent explore and read a failure's trace. See [docs/CLI.md](docs/CLI.md#inspect).

## AI coding agents

```bash
atalanta mcp install
```

This registers the MCP server with Claude Code, Cursor, Codex, Windsurf, VS Code, Cline and Gemini CLI. An agent reads the screen as one line per element, acts by reference, and exports what it did as a YAML test. See [docs/MCP.md](docs/MCP.md).

Agents with a shell can skip MCP: `atalanta snapshot`, `atalanta tap @e7`, `atalanta export --out flow.yaml`. See [docs/CLI.md](docs/CLI.md#agent-commands).

## Documentation

- [Guide](docs/GUIDE.md): selectors, natural-language steps, visual checks, sharding, configuration and troubleshooting.
- [Explore](docs/EXPLORE.md): how agent-written tests are checked.
- [AI providers](docs/AI-PROVIDERS.md): models, keys and local models.
- [Drivers](docs/DRIVERS.md): what each platform needs and supports.
- [Security](docs/SECURITY.md): the threat model. Report vulnerabilities through [SECURITY.md](SECURITY.md).
- [Status](docs/STATUS.md): what has been verified, and on what.
- [All docs](docs/README.md).

Coming from Maestro? `atalanta migrate maestro ./flows/` converts your flows and lists anything it skipped.

## Install details

- **Install script.** Checks the download against the release's `SHA256SUMS` and installs into `~/.atalanta/bin` without sudo. Flags: `--dir DIR`, `--version X.Y.Z`, `--no-modify-path`, `--keep-quarantine`.
- **macOS signing.** Release binaries are ad-hoc signed, not notarised. If the binary carries the quarantine flag, the script removes it after the checksum check. With `--keep-quarantine`, approve it in System Settings > Privacy & Security on first run.
- **Release archives.** Built for macOS (arm64, x86_64) and static Linux (arm64, x86_64), each with a build provenance attestation: `gh attestation verify <file> --repo atalantahq/atalanta`.
- **Console UI from source.** `brew install --HEAD atalantahq/tap/atalanta` includes it. A plain `cargo build` does not; run `npm ci && npm run build` in `crates/atalanta-inspector/frontend` first.

## Acknowledgements

Atalanta's Android screen streaming is adapted from [scrcpy](https://github.com/Genymobile/scrcpy). Its iOS runner speaks the API of [WebDriverAgent](https://github.com/appium/WebDriverAgent). The YAML format takes ideas from [Maestro](https://github.com/mobile-dev-inc/maestro). Visual diffs use the anti-aliasing detector from [pixelmatch](https://github.com/mapbox/pixelmatch). Web device profiles come from [Playwright](https://github.com/microsoft/playwright). Details are in [NOTICE](NOTICE).

## Licence

Atalanta is licensed under the GNU Affero General Public License, version 3 only (`AGPL-3.0-only`): see [LICENSE](LICENSE), and [NOTICE](NOTICE) for third-party material. Using the binary to test your own apps puts no obligations on those apps or their tests. Copying or adapting Atalanta's code into another project, or running a modified version as a network service, means releasing that work's source under the AGPL-3.0 and keeping the copyright notices. AI agents: read [AGENTS.md](AGENTS.md) before copying code from this repository.

Questions and bugs: [GitHub Issues](https://github.com/atalantahq/atalanta/issues). Contributing: [CONTRIBUTING.md](CONTRIBUTING.md).
