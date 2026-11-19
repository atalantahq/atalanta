# Velocity

**Mobile UI tests in YAML, one static binary, written in Rust.**

Velocity runs YAML test suites on Android emulators, iOS simulators and Chrome. It is one static binary: no JVM, no Node, no Appium server. Real Android hardware has been used once (one tablet, the Settings app); real iPhones are supported in the code but have never been run ([STATUS.md](docs/STATUS.md)).

Three things set it apart:

- **An AI-written test must fail when the feature is broken, and that is checked before the test is saved.** `velocity explore` replays the test it wrote 5 times with no model, then replays it with the last tap moved to a neighbouring element (it must fail), and requires an assertion that names the goal. Only then is the test marked trusted ([trust gates](docs/EXPLORE.md#trust-gates)). In a small pilot (11 goals, 4 rounds with fixes between them; not the pre-registered test in [bench/killtest/](bench/killtest/PREREG.md)), the final round caught 6 of 6 seeded regressions with no false passes ([bench/pilot/](bench/pilot/README.md)). The wrong-path check has not yet been seen catching a weak test on its own.
- **The model is never on the critical path.** A saved test replays in CI with zero model calls (a call during replay is an internal error). Model output never becomes a selector or a secret, and by default never a typed value: the model names an element number, code builds the selector, and text is typed only by name (`${NAME}`). Opt-ins widen that: `explore --values-from-goal` and `--values-from-screen` (code checks the text is in the goal or on a screen seen), and `--values-from-screenshots` and `--values-from-model`, which code cannot check. In the pilot a goal cost about $0.08 to explore once; each CI run after that costs nothing in model calls.
- **Failure evidence on your own machine.** A failed test keeps a video cut from a ring buffer, the device and app logs with crashes named, and one `trace.html` with each step's screenshot, tap point, selector and matched element. No cloud account. Measured overhead with video and logs on: about 15% per step on an Android emulator.

[Install](#install) · [The story](#from-zero-to-ci) · [Console](#the-console) · [Agents (MCP)](#agents-mcp) · [API tests](#api-tests) · [Failure evidence](#failure-evidence) · [AI providers](#ai-providers) · [Security](#security-notes) · [Glossary](docs/GUIDE.md#glossary) · [Docs](docs/README.md)

## Install

> **Pre-release.** Velocity has not been published yet (0.2.0 is unreleased); on 2026-11-10 all four routes below returned 404. Until then, build from a checkout: `cargo install --path velocity-cli --locked` (see [Install details](#install-details) for the console UI).

Pick one:

```bash
curl -fsSL https://raw.githubusercontent.com/omkar-dev/velocity/main/install.sh | sh   # macOS / Linux, x86_64 / arm64
brew install omkar-dev/velocity/velocity
npm install --save-dev @velocity-test/cli
cargo install velocity-cli --locked
```

Then check the machine with `velocity doctor`. Every route installs the same release. Windows is not supported (WSL2 works as Linux). Installer flags, release archives, signature checks and building from source: [Install details](#install-details).

| To test | You need |
|---|---|
| Android | Android SDK platform-tools (`adb` on `PATH`) and a running emulator or device |
| iOS | macOS with Xcode and a booted simulator. The first iOS run clones and builds WebDriverAgent (needs git and network, about a minute, once per Xcode) |
| Web | Chrome (runs headless by default) |

## From zero to CI

### 1. Check the machine, write a first test

With an emulator or simulator running and your app on it (`velocity devices up --android 1` or `--ios 1` boots one from an existing AVD or simulator):

```bash
velocity doctor            # adb, Xcode, devices, runner builds, Chrome, AI keys, your suites; --fix applies the safe fixes
velocity init --run        # reads the app's launch screen, writes velocity/<app>.yaml and a CI example, runs it
```

`velocity init` asserts one or two stable elements of the first screen (an id first, else a short unique text). It asks what it needs on a terminal; in CI pass everything: `velocity init --platform android --app-id com.example.app --yes --run`. It never overwrites a file without `--force`.

### 2. Let an agent explore a goal

```bash
velocity explore "open Network & internet" --app-id com.android.settings
```

An agent works through the goal on the device, one operation per turn. The model only picks an operation and an element number; code builds every selector and fills every value. The session is written as plain YAML with an `assertVisible` after each screen change and assertions on the evidence the checker cited for the goal. Volatile text (times, prices, counters) is never asserted and `--secret` values are written only as `${NAME}`. The test is then replayed with **zero model calls** to prove it holds, and repaired once if a replay fails. `--script plan.json` plays a fixed plan instead of a model. Details: [docs/EXPLORE.md](docs/EXPLORE.md).

### 3. Run it, review it, commit it

```bash
velocity run open-network-internet.yaml
```

The test is ordinary YAML; you can also write one by hand:

```yaml
appId: com.example.app
tests:
  - name: Log in
    steps:
      - launchApp: { clearState: true }
      - inputText: { selector: { id: email }, text: "qa@example.com" }
      - inputText: { selector: { id: password }, text: "${LOGIN_PASSWORD}" }
      - tap: { id: login_button }
      - assertVisible: { text: "Welcome" }
      - do: "open the first order"          # a natural-language step, resolved by a model and cached once verified
```

Every step waits for its element and for the screen to *settle*, that is, stop changing (no sleeps); a target must be enabled and still before it is tapped. Failures point at `file:line`. A test that passes only on a retry is reported as flaky. Suite format: [docs/DSL.md](docs/DSL.md) and the generated [DSL reference](docs/DSL-REFERENCE.md).

### 4. CI

```bash
velocity run tests/ --parallel 4 --report junit,github --retries 1   # every booted device, failed tests retried on another device
velocity run tests/ --shard 2/4 --shard-timings history/             # one CI shard, balanced by past durations
velocity run tests/ --changed-since origin/main                      # only the tests a change can affect
```

Each device is *leased* (a per-device lock file under `~/.velocity/leases`), so parallel runs, the console and an MCP agent never drive the same device. `velocity init` writes a sharded GitHub Actions example to `velocity/ci/`. Exit codes: 0 passed, 1 a test failed, 2 configuration, 3 device, 4 suite timeout, 5 internal. Sharing history between jobs, cancelled jobs, secrets and baselines: [docs/CI.md](docs/CI.md).

## The console

```bash
velocity inspect --open
```

One browser page for every booted Android device, iOS simulator and the web browser:

- **Live mirror.** Android streams H.264 from Velocity's on-device encoder (~30 fps, decoded by WebCodecs); iOS simulators stream WebDriverAgent's MJPEG (~26 fps); the web streams Chrome's screencast. Click, drag, scroll and type on it; driving takes the device's lease, watching doesn't.
- **Element overlay.** Hover the mirror to see the element and the selector Velocity would use, with "copy as YAML".
- **Run panel.** Run a suite or one test from the project; steps stream in live, and a failure's `trace.html`, video and logs are a click away.
- **Explore panel.** Type a goal, watch the agent's turns, steer it with an instruction, then read the YAML it wrote, its replays (no model calls) and the tokens it spent, and save it into the project.
- **Diagnostics.** `velocity doctor`'s checks, with their fixes.

Flags and the HTTP API: [docs/CLI.md](docs/CLI.md#inspect).

## Agents (MCP)

```bash
velocity mcp install         # registers the server with every client found (Claude Code, Cursor, Codex, Windsurf, VS Code, Cline, Gemini CLI) and installs the agent rules
```

The server (`velocity mcp`, JSON-RPC over stdio) serves five tools by default (`observe`, `act`, `run`, `artifacts`, `verify_change`); `--profile full` serves all 32. An agent reads the screen with `snapshot` (one line per element with a ref like `@e12`; `mode: "delta"` returns only what changed), acts by ref, exports what it did as YAML with assertions, and debugs with `describe_failure`, `inspect_trace` and `diagnose`. `run_task` runs a suite in the background, or with a `goal` runs `velocity explore`: the agent steers it with `task_inject`, polls `task_status` for the turn, then reviews the YAML it wrote and runs it. Passwords are typed by environment variable name and never returned. [docs/MCP.md](docs/MCP.md).

Agents with a shell can skip MCP: `velocity snapshot`, `velocity tap @e7`, `velocity type @e3 "…"`, `velocity export --out flow.yaml`. Each action waits for the screen to settle and prints what changed; a stale ref fails with `STALE_REF` instead of tapping something else; `--json` gives a code and a retry rule on failure. `velocity skills get cli` prints the guide for the installed version. [docs/CLI.md](docs/CLI.md#agent-commands).

## API tests

An `http` step sends a request, checks the response (status, headers, JSONPath values, body, time), and captures values as `${NAME}` for later steps, UI steps included:

```yaml
- http:
    method: POST
    url: /orders                    # after config.http.baseUrl
    json: { sku: "A1", qty: 2 }
    expect: { status: 201, json: { items[0].sku: A1, total: { gt: 0 } } }
    capture: { ORDER_ID: id }
- tap: { text: "Orders" }
- assertVisible: { text: "${ORDER_ID}" }
```

A suite with only `http` steps is an API test suite: it runs without a device, in parallel with `--parallel`, into the same reports. A failure lists every check that failed and the start of the body. [docs/DSL.md](docs/DSL.md#http-and-api-tests).

## Failure evidence

A failed test keeps, next to its trace in `velocity-results/traces/<test>/`:

- **`video.mp4`**: on Android from Velocity's own scrcpy-style encoder, muxed in Velocity (no ffmpeg), cut from a ring buffer of the last 30 s; on iOS simulators from `simctl io recordVideo`; on the web the screencast frames.
- **Logs**: logcat (the device's and the app's own lines; crashes and ANRs are named in the failure), the simulator's log stream for the app, or the browser console and failed requests. Secrets are masked.
- **`trace.html`**: one self-contained page with every step, its screenshot with the tap point marked, the selector and the element it resolved to, the video seeked to the selected step and the log lines around it. `velocity trace view velocity-results` opens an index.

A passing test keeps nothing (`--video always`/`--logs always` keeps them anyway). `--pause-on-failure` stops at the first failing step with the device untouched: Enter goes on, `r` retries the step, `q` aborts. Reports (`html`, `junit`, `json`, `allure`) link it all. [docs/CLI.md](docs/CLI.md#failure-evidence).

## AI providers

Natural-language steps, `verify:` checks and `velocity explore` use one model service, chosen by name: 19 presets over three wire formats, including Anthropic, OpenAI, Azure OpenAI, Gemini, Bedrock, Vertex, OpenRouter, Groq, Mistral, DeepSeek, xAI, Ollama, LM Studio, vLLM and any OpenAI-compatible endpoint.

```bash
VELOCITY_NL_MODEL=gemini:gemini-3.8-flash velocity run app.yaml   # set only that provider's key variable
velocity explore "add a card" --app-id com.example.app --model anthropic:claude-sonnet-5-5
```

Whatever the provider, model output never becomes a selector or a typed value: code builds and checks both. Plans are cached once a run verifies them, so a stable screen replays without calling the model. The default models are provisional (unmeasured); `velocity nl-eval --model … --recommend` compares models on a fixed task set. [docs/AI-PROVIDERS.md](docs/AI-PROVIDERS.md).

## Security notes

- **The console** listens on 127.0.0.1 by default. Every request and WebSocket needs the per-session token from the printed URL, and `Host`/`Origin` are checked, so other sites and DNS rebinding can't reach it. `--host 0.0.0.0` exposes the device to the network behind the token, with a warning.
- **WebDriverAgent on simulators.** A simulator shares the Mac's network. Velocity binds WDA's HTTP server to loopback (`USE_IP`), and patches its WDA clone so the MJPEG screen stream, which upstream opens on every interface without authentication, listens on loopback too. A WDA you start yourself (`VELOCITY_WDA_URL`), or one an older Velocity launched and kept running (`VELOCITY_KEEP_WDA`), still serves the simulator's screen to the LAN on its MJPEG port until it is restarted.
- **Real iPhones.** The runner and WDA's stream bind the phone's loopback, reached through the USB forward (`VELOCITY_IOS_DEVICE_LOOPBACK=0` binds every interface). A Wi-Fi-only iPhone needs `ios.allowWifiRunner` with VelocityRunner, which requires its per-launch token on every request ([docs/DRIVERS.md](docs/DRIVERS.md)).
- **Secrets** are read from environment variables, masked in every report, log and trace, never sent to a model, and written into generated YAML only as `${NAME}`. Over MCP an agent can type only the names listed in `VELOCITY_SECRETS`.

## More

- [docs/GUIDE.md](docs/GUIDE.md): the long form (selectors and roles, natural-language steps, visual checks, healing, sharding, app state and caching, headless, configuration, a command cheat sheet).
- [docs/](docs/README.md): architecture, drivers, CLI and exit codes, CI, MCP, explore, AI providers.
- [docs/GUIDE.md#glossary](docs/GUIDE.md#glossary): the terms these docs use (settle, lease, ref, grounding, trust gates, …).
- [docs/GUIDE.md#troubleshooting](docs/GUIDE.md#troubleshooting): first-run failures and what to do next.
- [docs/STATUS.md](docs/STATUS.md): what has been verified on emulators, simulators, browsers and hardware, and what has not. Real Android hardware: one tablet, Settings only (2026-11-08). Real iPhones: never run.
- `velocity migrate maestro ./flows/` converts Maestro flows. Commands with no Velocity equivalent (`runScript`, `evalScript`, `copyTextFrom`, `setLocation` and others) are skipped and listed in its report, and a `runFlow` with `when:` always runs, with a warning.

## Install details

- **Install script.** Downloads the binary for your machine, checks it against the release's `SHA256SUMS`, and installs into `~/.velocity/bin` without sudo. Options: `--dir DIR`; `--version X.Y.Z` (or `VELOCITY_VERSION`); `--no-modify-path` to leave your shell profile alone; `--keep-quarantine` (macOS, below).
- **macOS signing.** Release binaries are ad-hoc signed, not Developer ID signed or notarised. `curl` does not quarantine what it downloads, so usually nothing happens. If the installed binary does carry the `com.apple.quarantine` flag, the script removes it after the `SHA256SUMS` check and says so. `--keep-quarantine` (or `VELOCITY_KEEP_QUARANTINE=1`) leaves the flag on; then approve the binary in System Settings > Privacy & Security on first run.
- **Release archives.** `velocity-<version>-<target>.tar.gz` for `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` (static). Each has a build provenance attestation: `gh attestation verify <file> --repo omkar-dev/velocity`.
- **npm.** `npx @velocity-test/cli --version` runs it without installing.
- **From source.** `brew install --HEAD omkar-dev/velocity/velocity` builds `main` with the console UI. A plain `cargo build` from a checkout has no console UI until you run `npm ci && npm run build` in `crates/velocity-inspector/frontend`; until then it serves the API and a page saying so.

Contributing: [CONTRIBUTING.md](CONTRIBUTING.md). License: GNU AGPL v3 ([LICENSE](LICENSE)).
