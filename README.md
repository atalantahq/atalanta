# Velocity

**Mobile UI tests that an AI writes once and CI replays for free. Written in Rust.**

Velocity runs YAML test suites on Android emulators and devices, iOS simulators and devices, and Chrome. It is one static binary: no JVM, no Node, no Appium server. An AI agent can explore your app and write a deterministic test; the test then runs at native speed with no model in the loop.

[Install](#install) · [The story](#from-zero-to-ci) · [Console](#the-console) · [Agents (MCP)](#agents-mcp) · [API tests](#api-tests) · [Failure evidence](#failure-evidence) · [AI providers](#ai-providers) · [Security](#security-notes) · [Docs](docs/README.md)

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/omkar-dev/velocity/main/install.sh | sh   # macOS / Linux, x86_64 / arm64
brew install omkar-dev/velocity/velocity
npm install --save-dev @velocity-test/cli        # or: npx @velocity-test/cli --version
cargo install velocity-cli --locked
```

Every route installs the same release, console web UI included. The script downloads the binary for your machine, checks it against the release's `SHA256SUMS`, and installs into `~/.velocity/bin` without sudo (`--dir DIR`, `--version X.Y.Z` or `VELOCITY_VERSION` to choose; `--no-modify-path` to leave your shell profile alone). Release archives are `velocity-<version>-<target>.tar.gz` for `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` (static); each has a build provenance attestation (`gh attestation verify <file> --repo omkar-dev/velocity`). Windows is not supported (WSL2 works as Linux). `brew install --HEAD omkar-dev/velocity/velocity` builds `main` from source with the UI. A plain `cargo build` from a checkout has no UI until you run `npm ci && npm run build` in `crates/velocity-inspector/frontend`; it then serves the API with a page saying so.

Needs: Android SDK platform-tools (`adb`) for Android; macOS with Xcode for iOS (WebDriverAgent is cloned at a pinned tag and built once per Xcode); Chrome for the web.

## From zero to CI

### 1. Check the machine, write a first test

With an emulator or simulator running and your app on it:

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

Every step waits for its element and for the screen to settle (no sleeps); a target must be enabled and still before it is tapped. Failures point at `file:line`. A test that passes only on a retry is reported as flaky. Suite format: [docs/DSL.md](docs/DSL.md) and the generated [DSL reference](docs/DSL-REFERENCE.md).

### 4. CI

```bash
velocity run tests/ --parallel 4 --report junit,github --retries 1   # every booted device, failed tests retried on another device
velocity run tests/ --shard 2/4 --shard-timings history/             # one CI shard, balanced by past durations
velocity run tests/ --changed-since origin/main                      # only the tests a change can affect
```

Devices are leased across processes, so parallel runs, the console and an MCP agent never drive the same device. `velocity init` writes a sharded GitHub Actions example to `velocity/ci/`. Exit codes: 0 passed, 1 a test failed, 2 configuration, 3 device, 4 suite timeout, 5 internal. Sharing history between jobs, cancelled jobs, secrets and baselines: [docs/CI.md](docs/CI.md).

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

The server (`velocity mcp`, JSON-RPC over stdio) has 31 tools. An agent reads the screen with `snapshot` (one line per element with a ref like `@e12`; `mode: "delta"` returns only what changed), acts by ref, exports what it did as YAML with assertions, and debugs with `describe_failure`, `inspect_trace` and `diagnose`. `run_task` runs a suite in the background, or with a `goal` runs `velocity explore`: the agent steers it with `task_inject`, polls `task_status` for the turn, then reviews the YAML it wrote and runs it. Passwords are typed by environment variable name and never returned. [docs/MCP.md](docs/MCP.md).

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
- [docs/STATUS.md](docs/STATUS.md): what has been verified on real emulators, simulators and browsers, and what has not. Real iPhones and Android hardware have not been used yet.
- `velocity migrate maestro ./flows/` converts Maestro flows.

Contributing: [CONTRIBUTING.md](CONTRIBUTING.md). License: Apache 2.0 ([LICENSE](LICENSE)).
