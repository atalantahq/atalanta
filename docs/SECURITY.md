# Security model

This document reviews every place where Atalanta opens a socket, talks to a
remote service, or writes files. For each one it says what the component binds
to, how it authenticates, and what three attackers could do:

- **a web page** open in any browser on the same machine, using CSRF-style
  blind requests or DNS rebinding against `127.0.0.1`;
- **another host on the LAN** or Wi-Fi;
- **another local user or process** on the same machine or device.

It was written from the code as of the 0.2 release line. Where the code and
this document disagree, trust the code and report the gap (see
[SECURITY.md](../SECURITY.md)).

**Baseline assumption.** Whoever runs `atalanta` already controls the devices
it drives: `adb` shell, XCTest and the Chrome DevTools Protocol are full
control by design. The goal is that Atalanta never extends that control to
anyone else, such as a web page, the network, or another user.

## Summary

| Surface | Binds to | Auth | Web page | LAN | Local user |
| --- | --- | --- | --- | --- | --- |
| Inspector / console (`atalanta inspect`) | `127.0.0.1:9876` (default); `--host` overrides | 128-bit session token + Host/Origin/Sec-Fetch-Site checks | blocked | only with `--host` and the token | needs the token |
| MCP server (`atalanta mcp`) | stdio only | the parent process | n/a | n/a | n/a |
| MCP `open_inspector` | `127.0.0.1:9876-9885` | inspector token | blocked | no | needs the token |
| Android tree server (on device) | abstract socket `@atalanta_tree` | peer-uid check (shell/root only), then a per-start token (`AUTH`, protocol 13) | see adb forward | no | apps blocked |
| Android video server (on device) | abstract socket `@atalanta_video_<pid>_<n>` | peer-uid check, then a per-start token (mutual HMAC handshake, video server 2); first authenticated client only | see adb forward | no | apps blocked |
| `adb forward tcp:0` to the tree server (host side) | `127.0.0.1:<random>` | the tree server's token | blocked (R-2 fixed 2026-10-25) | no | only with adb access (R-3 mitigated) |
| `adb forward tcp:0` to the video server (host side) | `127.0.0.1:<random>` | the video server's token | blocked (closed, the server waits for the next client) | no | only with adb access (R-3 mitigated 2026-11-11) |
| Android sync probe (in the app) | `127.0.0.1:19401` on the device | none | n/a | no | other apps can query |
| iOS AtalantaRunner (simulator) | `127.0.0.1:8100+` (`USE_IP`) | per-launch bearer token + Host/Origin/Sec-Fetch-Site checks | blocked (R-1 fixed 2026-10-25) | no | needs the token (file `0600`) |
| iOS WebDriverAgent (simulator) | `127.0.0.1:8100+` (`USE_IP`) | **none** (third-party) | **blind writes possible (R-1W)** | no | yes |
| iOS runner on a cabled iPhone | the phone's loopback (R-4 fixed 2026-10-25); every interface only with `ATALANTA_IOS_DEVICE_LOOPBACK=0` | AtalantaRunner: per-launch token; WDA: none | through the usbmux forward, as on a simulator | no (unless opted out; then WDA is open) | AtalantaRunner: needs the token |
| iOS runner over Wi-Fi | the phone's Wi-Fi address | bearer token required (`ios.allowWifiRunner`, AtalantaRunner only) | no | needs the token | needs the token |
| usbmuxd forward (host side) | `127.0.0.1:<port>` | the runner's | as the runner | no | as the runner |
| iOS sync probe (in the app) | `127.0.0.1` (simulator shares the Mac's loopback) | none | low impact | no | yes |
| Flutter bridge (in the app or test) | `127.0.0.1` | none | low impact | no | yes |
| Web driver: Chrome DevTools | `127.0.0.1:<random>` (`--remote-debugging-port=0`), own profile dir `0700` | none (Chrome's own Host/Origin checks) | blocked by Chrome | no | yes |
| AI providers | outbound HTTPS | API key from environment | n/a | n/a | n/a |
| Notifiers (`--notify webhook=`) | outbound HTTP(S) | none | n/a | n/a | n/a |

## 1. Inspector and console (`crates/atalanta-inspector`)

**What it is.** An axum server that serves the React console (embedded with
`rust-embed`) and an API under `/api`. The API covers device lists,
screenshots, the hierarchy, tap/type/swipe/key input, the live mirror and
WebSocket (`/api/ws/{device}`, `/api/mirror/{device}`), saving flows,
starting `atalanta run` and `atalanta explore` child processes, reading their
output files, and accepting or rejecting screenshot baselines. `/review` is
a server-rendered page for baselines.

**Binding.** `atalanta inspect` binds `127.0.0.1:9876` by default
(`InspectorServer::bind`). `--host <ip>` binds any address, and a
non-loopback address prints a warning that anyone who can reach the server
and has the URL can drive the device. The MCP `open_inspector` tool always
binds `127.0.0.1`.

**Guard** (`src/security.rs`, applied to every route by middleware):

- **Token.** A 128-bit random token from `/dev/urandom` per server. It is
  required on `/api/*` and `/review`, from the `x-atalanta-token` header, a
  `token=` query parameter, or the `atalanta_inspector_<prefix>` cookie. The
  cookie is set when the UI is opened with the printed `?token=` URL, after
  which a 302 removes the token from the address bar. The cookie is
  `HttpOnly; SameSite=Strict`, the comparison is constant-time, and the
  static UI files need no token because they hold no data.
- **Host.** The `Host` header must be `localhost`, `127.0.0.1` or `[::1]`, or
  the bound address. This defeats DNS rebinding. When bound to `0.0.0.0` any
  Host is accepted (the token still applies).
- **Origin.** When present, it must equal `http(s)://<Host>` exactly. Another
  localhost port is rejected even though browsers treat it as same-site.
  `null` and `file://` are rejected.
- **`Sec-Fetch-Site`.** On `/api/*`, when sent, it must be `same-origin` or
  `none`.
- **Headers.** `nosniff`, `Referrer-Policy: no-referrer`,
  `X-Frame-Options: DENY`, `Cross-Origin-Resource-Policy: same-origin`, and
  `Cache-Control: no-store` on the API. `/review` sends a strict CSP.
- **Files.** Paths from requests are confined to their directory. Flows are
  saved under `--tests-dir` (once `--flows-dir`) as new `.yaml` files only, with relative,
  normal components and no `..` (`routes/recording.rs`). Suites and plan
  files resolve inside the project directory and symlinks that leave it are
  refused (`console/project.rs::confine`). Task files are confined to the
  task's folder, and baselines are addressed by id, not path.
- **Child processes.** Device ids, filters, app ids, model names and
  `verify` values that start with `-` are refused, so they can't become
  flags of the `atalanta` child.

**Attacker analysis.**

- *Web page:* it can't read responses (no CORS headers, CORP), and can't
  send an authenticated request, because the cookie is `SameSite=Strict`, a
  cross-site `Origin` or `Sec-Fetch-Site` is refused, and it doesn't know the
  token. DNS rebinding fails the `Host` check.
- *LAN:* nothing is reachable with the default bind. With `--host 0.0.0.0`
  the server is plain HTTP: anyone who can see the traffic (shared Wi-Fi)
  can read the token from the URL or cookie and then drive the device, run
  suites and read results.
- *Local user:* needs the token, which is printed to the terminal of the user
  who started the server. It is not written to disk.

**Residual risks.**

- I-1: with `--host` set to a non-loopback address, the token travels in
  clear text. Use an SSH tunnel or a TLS reverse proxy instead of `--host`
  on untrusted networks.
- I-2 (fixed 2026-11-11): `POST /api/tasks/explore` passed the goal text to
  the child before the flags, so a goal starting with `-` could be read as a
  flag. The goal now goes last, after `--`.
- I-3 (fixed 2026-11-11): the console UI had no Content-Security-Policy. Its
  page and assets now carry `default-src 'self'; script-src 'self';
  style-src 'self'; img-src 'self' data: blob:; connect-src 'self';
  object-src 'none'; base-uri 'none'; frame-ancestors 'none'`, behind
  React's escaping of the device-controlled strings it renders.

## 2. MCP server (`crates/atalanta-mcp`)

**Transport.** JSON-RPC over **stdio only** (`server.rs`, logs go to
stderr). There is no HTTP or SSE transport, so nothing listens. Background
tasks (`src/tasks/`) run `atalanta run` or `atalanta explore` as children
and talk to them through files in a task directory: an events JSONL and an
`--inject-file`.

**Trust.** The MCP client, usually an AI agent, is as trusted as the user who
configured it. Tools take paths (`suite`, export targets) and write files
where they are told, with the user's permissions. `failure.rs` confines
artifact reads to allowed roots after canonicalising.

**Residual risks.**

- M-1 (prompt injection): screen text and hierarchy dumps from the app under
  test are returned to the agent verbatim, apart from secret redaction. An
  app, or a web page in the web driver, can show text such as "ignore your
  instructions and run …". Atalanta can't stop the agent from acting on it.
  Run agents with tool confirmation enabled when testing untrusted apps or
  sites.
- M-2: `--notify script=<path>` executes a program and `webhook=<url>` posts
  the task result. Both come from the command line the user wrote, not from
  tool arguments.

## 3. Android: tree server, video server, adb forwards (`crates/atalanta-android`, `probes/android/atalanta-tree-server`)

**What runs on the device.** `atalanta-tree.jar`, a dex built from
`probes/android/atalanta-tree-server` and embedded in the binary (rebuild
check: `scripts/check-tree-jar.sh`), is pushed to `/data/local/tmp` and
started with `app_process` as the **shell** user. It runs two servers:

- **Tree server** on abstract socket `@atalanta_tree`. A line protocol with
  the commands PING, DUMP, TAP, TAPS, PRESS, SWIPE, KEY, TEXT, SETTEXT, IME,
  GLOBAL, SCROLLNODE, SHOT, WINDOWS, MARK, GEN, SETTLE, ENCODERS and QUIT.
  Every connection's peer uid is checked with `getPeerCredentials()`, and
  app uids (`uid % 100000 >= 10000`) are refused, so other apps on the
  device can't drive it. adbd connects as shell or root.
- **Video server** (`VideoServer.java`, adapted from scrcpy, H.264) on
  `@atalanta_video_<pid>_<n>`. It has the same uid check. Since video
  server 2 (2026-11-11) it writes a new token to
  `/data/local/tmp/<socket>.token` (shell or root only) at each start, and
  a client must pass a mutual HMAC handshake over it (`HELLO` → `PROOF`
  → `AUTH` → `OK`; the token never crosses the socket) before it streams.
  A client that fails is closed and the server waits for the next one, up
  to its start timeout. It streams to exactly one client, and its control
  channel only understands key-frame requests and quit.

**Host side.** Both are reached through `adb forward tcp:0
localabstract:<name>`. The adb server binds the forward on `127.0.0.1` at a
random port, which Atalanta removes when it finishes.

**Tree server handshake (protocol 18).** At each start the server makes a
128-bit token and writes it to `/data/local/tmp/atalanta-tree.token`,
readable by its own uid (shell, or root) only; the host reads it with
`adb shell cat`. The token never crosses the socket. The handshake is
mutual, because the socket name is public: any app may bind
`@atalanta_tree` first, and before protocol 18 an app that did received the
token and every typed secret.

1. `PING` → `PONG 18 <boot nonce>` (the server's per-start nonce).
2. The host sends `HELLO <client nonce>` (128 random bits, new for every
   connection). The server answers `PROOF <hex HMAC-SHA256(token,
   "atalanta-tree server <boot nonce> <client nonce>")>`. The host compares
   it in constant time and sends nothing more to a server that can't prove
   it holds the token; it says another app may have bound the socket name.
3. The host sends `AUTH <hex HMAC-SHA256(token, "atalanta-tree client <boot
   nonce> <client nonce>")>` (a different message, so a server's proof is
   never a client's); the server answers `OK 0`.

Every command but `PING`, `HELLO` and `QUIT` needs that `AUTH` first. The
first line that isn't a command, or a command before `AUTH`, gets one `ERR`
and the connection is closed, so nothing after it is read. The host refuses
servers older than protocol 18 (they can't prove anything) and replaces a
running server that isn't its own version. When the server can't bind the
socket because another process holds the name, the host reports that
instead of quietly falling back to `uiautomator`. Not yet run on a device
(see [STATUS.md](STATUS.md)).

**Residual risks.**

- **R-2 (medium): cross-protocol requests from a web page. Fixed
  2026-10-25** (protocol 13, above): the request line of a browser's blind
  `POST` (`POST / HTTP/1.1`) closes the connection before the headers or
  body are read, and a page can't read the device file holding the token.
  Checked on the API 34 emulator (see [STATUS.md](STATUS.md)). The original
  finding, for the record: The tree
  server reads newline-separated commands and answers `ERR unknown command`
  to anything else *without closing the connection*. A browser `fetch()` to
  `http://127.0.0.1:<port>/` with a `text/plain` body sends lines such as
  `POST / HTTP/1.1` and the headers, which are refused, but then the body
  lines (`TAP 500 900`, `TEXT …`, `KEY …`) are executed. The port is random
  and lives only for the run, so the page would have to scan for it.
  Chrome's Local Network Access permission blocks public pages from
  reaching loopback; Safari and Firefox don't. Responses can't be read
  cross-origin, so the impact is input injection, not data theft.
  **Recommended fix:** close the connection on the first line that isn't a
  known command, or on any line that looks like HTTP (`^[A-Z]+ \S+
  HTTP/1\.[01]$`). Better still, require a per-session nonce as the first
  line, passed to `app_process` on its command line (visible only to shell
  and root on the device).
- **R-3 (low): other local users. Mitigated 2026-10-25** for the tree
  server: a process that connects to the forwarded port also needs the
  token, which only a process that can run `adb shell` can read. That
  process can already drive the device through adb itself
  (`127.0.0.1:5037`, no authentication, outside Atalanta's control), so the
  forward no longer adds access. The **video server's** forward got the
  same treatment on 2026-11-11 (video server 2, security review #10): a
  per-start token file and a mutual handshake before the stream, so
  another local process can neither read the screen nor keep the client
  slot. Not yet run on a device.
- The sync probe that apps embed (`probes/android/atalanta-sync-probe`,
  debug builds only) listens on the device's `127.0.0.1:19401` with no
  authentication. Any app on the device can ask whether the app is idle,
  read its package name and pid, and change the probe's ignore lists, which
  only affects how long Atalanta waits. Never ship the probe in a release
  build.

## 4. iOS: AtalantaRunner, WebDriverAgent, usbmuxd (`crates/atalanta-ios`, `probes/ios/AtalantaRunner`)

**Simulators.** The runner (AtalantaRunner, or WebDriverAgent cloned at a
pinned tag whose commit hash is verified, see `wda_bootstrap.rs`) runs inside
the simulator, which shares the Mac's network stack. Atalanta passes
`USE_IP=127.0.0.1`, so the HTTP server (port 8100 and up) and WDA's MJPEG
stream (patched by `patch_mjpeg_bind` to honour `USE_IP`) bind loopback only.

**Cabled iPhones.** The host reaches the runner through Atalanta's own
usbmuxd forward (`usbmux.rs`, listening on `127.0.0.1`). On the phone, the
runner binds its **loopback** (`USE_IP=127.0.0.1`), which the usbmux
forward reaches (since 2026-10-25, R-4). `ATALANTA_IOS_DEVICE_LOOPBACK=0`
binds every interface instead, for a phone whose usbmuxd doesn't deliver
to loopback; with WebDriverAgent Atalanta then warns that anyone on the
phone's network can drive it.

**Wi-Fi-only iPhones** are refused unless `ios.allowWifiRunner: true`,
`ios.runner: atalanta` and an `ios.runnerToken` of at least the minimum
length are all set. WebDriverAgent has no authentication, so it is never
allowed over Wi-Fi.

**Authentication (AtalantaRunner, since 2026-10-25).** Atalanta makes a
random 128-bit token for every runner launch (simulators included), passes
it as `ATALANTA_RUNNER_TOKEN` through xcodebuild's `TEST_RUNNER_`
environment, and keeps it in `~/.atalanta/wda/running/<udid>.token` (mode
`0600`) so a later Atalanta process of the same user can reattach.
`ios.runnerToken` replaces it with a fixed value. The runner refuses to
start without a token, and checks every request, `/status` included:

- `Origin` present → 403; `Sec-Fetch-Site` other than `none` or
  `same-origin` → 403;
- `Host` not `127.0.0.1`, `localhost` or `[::1]` (or `USE_IP`) with the
  runner's own port → 403 (DNS rebinding). Through a usbmux forward the
  client sends the runner's `Host` (`127.0.0.1:8100`), not the forward's;
- no `Authorization: Bearer <token>`, or a wrong one (constant time) → 401.

A runner on the port that answers 401 (its token belongs to someone else)
is never reused: Atalanta stops it and launches its own.

WebDriverAgent has no authentication and no `Host` or `Origin` checks, and
Atalanta doesn't patch them in (it is a large third-party server; only its
MJPEG bind is patched). Atalanta binds it to loopback everywhere: simulators
and, since R-4, cabled iPhones.

**Residual risks.**

- **R-1 (medium): web pages can send commands to a simulator's runner.
  Fixed 2026-10-25 for AtalantaRunner** (above; checked on the iPhone 17
  simulator with curl, see [STATUS.md](STATUS.md)). **Residual R-1W:**
  WebDriverAgent is still open to blind cross-site writes, and to reads
  through DNS rebinding, from a page in a browser on the Mac while a run is
  going. Use `ios.runner: atalanta` where that matters. Chrome's Local
  Network Access prompt blocks public pages from reaching loopback; Safari
  and Firefox don't. The original finding:
  AtalantaRunner checks neither `Host` nor `Origin`, parses the body as JSON
  whatever its `Content-Type`, and ignores the session id on action routes.
  A page open in any browser on the Mac can therefore send no-cors
  `text/plain` POSTs to `http://127.0.0.1:8100/...` to tap, type, swipe,
  set the pasteboard or launch apps on the simulator while a run is going.
  With DNS rebinding (no `Host` check) it can also *read* `/source` and
  screenshots. The same applies to WebDriverAgent, and to a cabled device
  through the loopback usbmux forward.
  **Recommended fix:** in AtalantaRunner, refuse requests whose `Host`
  isn't `127.0.0.1`, `localhost` or `[::1]` (with any port), and any request
  that carries `Origin` or `Sec-Fetch-*` headers. The host should also always
  generate a random per-launch `ATALANTA_RUNNER_TOKEN`, simulators included,
  rather than only when the user configures one. Prefer
  `ios.runner: atalanta` over WebDriverAgent.
- **R-4 (high): a cabled iPhone's runner is reachable on Wi-Fi. Fixed
  2026-10-25:** loopback is the default for cabled iPhones and every runner
  launch gets a token. **Not yet run on a real iPhone**: that usbmuxd
  delivers to the phone's loopback is the P5 assumption, unverified on
  hardware. If it doesn't, the runner never answers; set
  `ATALANTA_IOS_DEVICE_LOOPBACK=0` (AtalantaRunner then stays
  token-protected, WDA doesn't). The original finding: Without `ATALANTA_IOS_DEVICE_LOOPBACK=1` and a token, anyone on
  the phone's network can drive it, and WDA's MJPEG port streams its screen
  while a run is going. **Recommended fix:** make loopback binding the
  default for cabled devices once the wired smoke run confirms that usbmuxd
  delivers to the phone's loopback (tracked as P5 in the code), and generate
  a token automatically.
- The iOS sync probe and the Flutter bridge listen on loopback with no
  authentication. On a simulator that is the Mac's loopback, so local
  processes and web pages (blindly) can reach them. Their commands are
  idle-state queries (sync probe) or test-only input (Flutter bridge, which
  runs in debug or test builds), so the impact is low. Ship neither in
  release builds.

## 5. Web driver (`crates/atalanta-web`)

Chrome is started by `chromiumoxide` with `--remote-debugging-port=0`. Chrome
picks a random loopback port and enforces its own Host and Origin checks on
the DevTools endpoints. Each Chrome gets a fresh profile directory created
with mode `0700` in the temp dir, and Chrome's sandbox stays on (Atalanta
never adds `--no-sandbox` itself). `web.extraArgs` are passed through
verbatim, so a suite can turn the sandbox or web security off. Treat suites
from untrusted sources as code.

Residual: any local process can attach to the DevTools port while the run is
going (same as R-3). Pages visited by tests run with the profile's
permissions, so tests should not log into real accounts with long-lived
sessions.

## 6. AI providers (`crates/atalanta-nl`, `crates/atalanta-explore`, `crates/atalanta-doctor`)

**Keys.** Keys are read only from environment variables. `apiKeyEnv` in
config must be a variable name, a value that looks like a key is refused,
and the error doesn't echo it. Every key, AWS secret or session token and
Google access or refresh token is registered with the redactor
(`atalanta-common/src/redact.rs`) as soon as it is read, so it is masked in
everything Atalanta prints or writes. Bedrock uses SigV4, and Vertex
exchanges a service account for a short-lived token (`gcp.rs`). The PEM in
`crates/atalanta-nl/fixtures/` is a throwaway test key that only unit tests
use.

**What is sent.**

- *Planning* (NL steps, `explore` turns) sends text only: the goal or step,
  and a numbered list of on-screen elements and their text, passed
  through `redact()` first.
- *`verify:` checks* also send a **screenshot** unless `images: false` or
  `ATALANTA_NL_IMAGES=0`. Screenshots are pixels and are **not redacted**:
  whatever the app shows, such as names, emails, balances or an unmasked
  field, goes to the provider.
- Model output is never used directly as a selector or a typed value. The
  planner returns an operation and an element number, values are referenced
  by `${VAR}` name, and `grounding.rs` builds and checks the selector in
  code. This limits what prompt injection from screen content can make the
  run do.
- Requests go to the provider's HTTPS endpoint, or to a user-configured
  `baseUrl`. Local servers (Ollama, LM Studio, vLLM) are plain HTTP on
  localhost or the given host and are never auto-detected.

**Redaction coverage** (`redact.rs`): values of `${VAR}`s whose names look
secret (PASSWORD, TOKEN, SECRET, KEY, PIN, OTP, ..., plus `ATALANTA_SECRETS`)
and known shapes (Bearer, Basic and other auth headers, cookie headers,
`?token=`-style parameters, `user:password@` in a URL, JSON `password`/`token`
fields, JWTs, `sk-`/`ghp_`/`github_pat_`/`glpat-`/`AIza`/
`AKIA`/`xox?-`/`hf_` prefixes, PEM private keys). It is applied to reports,
traces, live events, MCP replies, Android logcat and NL traces. Values
shorter than 8 characters are masked only as whole words.

**Residual risks.**

- A-1: screenshots, videos and the text of non-secret fields are not
  redacted, in model requests or in artifacts. Use test accounts and test
  data.
- A-2: a secret held in a variable whose name doesn't look secret (for
  example `${CUSTOMER_REF}`) is only masked if its shape matches. List such
  variables in `ATALANTA_SECRETS`.
- A-3: suites can interpolate any host environment variable (`${VAR}`), so a
  suite from an untrusted source could type an environment secret into the
  app under test. Treat suites as code.

## 7. Files Atalanta writes

- **Artifacts and evidence** (`--artifacts-dir`, default `atalanta-results/`):
  JUnit, JSON, HTML and Allure reports, traces (`steps.json`, per-step
  screenshots), hierarchy dumps, failure video (Android H.264 to MP4, iOS
  `simctl io recordVideo`, web), logcat and crash logs, and the history and
  result cache. Text output is redacted. **Screenshots, videos and logs
  contain whatever the app showed or logged.** Don't publish them from public
  CI without checking, and keep `atalanta-results/` in `.gitignore` (it is
  in this repo's).
- **Flows and suites** from the inspector and MCP export: new files only,
  confined as described in sections 1 and 2.
- **Baselines** (`atalanta baselines accept`, `/review`): written next to the
  suite's baseline directory.
- **Device side:** the tree-server jar under `/data/local/tmp` on Android.
- **Caches:** the WDA or AtalantaRunner build under the user's cache
  directory, Chrome profiles in the temp dir (mode `0700`), and
  `.atalanta/nl-cache.json`. The NL cache holds model answers (operations
  and element numbers, no keys) and is meant to be committed.

## 8. Supply chain

- Rust dependencies are checked with `cargo deny check licenses advisories
  bans sources` (`deny.toml`). Licence and third-party inventories are in
  `docs/licenses/` and `NOTICE`.
- `.github/workflows/security.yml` (and `scripts/security-check.sh`
  locally) adds OSV-Scanner over every lockfile, `npm audit`, gitleaks over
  the whole history, zizmor on the workflows, Semgrep SAST, an `unsafe`
  inventory per crate, CycloneDX SBOMs and weekly cargo-fuzz runs of the
  parsers that read device or user input; every workflow job runs under
  `step-security/harden-runner`. What runs, and how findings are triaged:
  [SECURITY.md](../SECURITY.md#automated-security-checks).
- The embedded `atalanta-tree.jar` can be rebuilt byte for byte from
  source with `scripts/check-tree-jar.sh`. That needs JDK 17 and Android
  build-tools whose d8 matches the version stamped in the jar.
- WebDriverAgent is cloned at a pinned tag and the commit hash is verified
  before it is built.
- Release archives are built by the tag's release workflow, listed in a
  `SHA256SUMS` asset and carry GitHub build provenance attestations
  (`gh attestation verify <file> --repo atalantahq/atalanta`). The installer
  (`install.sh`) downloads over HTTPS, refuses to install unless the archive
  matches `SHA256SUMS`, and checks the attestation too with
  `--verify-attestation`. The checksum file comes from the same release as
  the archive, so it guards against corruption and mirror tampering, not
  against a compromised release; the attestation covers that case.

## 9. Agents: the Rule of Two

Atalanta drives devices for AI agents, and an agent reads screens that anyone can write to: a web page, a push notification, a message in the app. Prompt-level defences don't hold against adaptive attacks (USENIX Security 2026; NAACL 2025), so the defence is structural: what a session *can* do, whatever the model is told. Following Meta's "Agents Rule of Two" (2025), a session may have at most two of:

- **[A]** reading untrusted screen text into the model;
- **[B]** holding secrets;
- **[C]** irreversible actions outside the goal: anything that changes the machine, other apps or data beyond the app under test (a shell or `adb` command, `diagnose`'s fixes, installing or erasing).

Clearing the app under test's own data (`clearState`) is how tests start, and is not [C].

| Session | A | B | C |
|---|---|---|---|
| `atalanta explore` with `--after` / `atalanta login` | yes | yes, typed by name and never shown to the model | **no**: ops are tap, type, scroll, swipe, keys, wait, assert and opening the listed apps |
| `atalanta explore` without secrets | yes | no | limited to the same ops, and to the apps listed with `--open-app` |
| MCP default (core) profile, and `--read-only` | yes | either: `act {secret}` and explore's `secret` type a variable listed in `ATALANTA_SECRETS` by name; a suite the agent writes (`run_task {yaml}`) may type the same secrets and read nothing else (below) | **no**: no tool in `IRREVERSIBLE_TOOLS` is listed or reachable, and a written suite has no `exec` step |
| MCP `--profile full` | yes | **no secret passed by Atalanta** unless the agent names one; a suite the agent writes may read any variable or keychain entry, and its child inherits the server's environment | yes (`diagnose` with `attempt_fix`; `exec` steps in a written suite) |

What holds this up, each with a test:

- **The model picks, code decides.** The planner answers with an operation and an element number; `grounding.rs` builds the selector. Model text never becomes a selector (the tests in `crates/atalanta-nl/src/grounding.rs`).
- **Values only by name.** Typed text is a `${NAME}`. Text the model proposes is accepted only when found in the goal (`--values-from-goal`) or, as a separate opt-in, on a screen seen in the run (`--values-from-screen`); see `values::tests::screen_text_needs_its_own_flag`.
- **Secrets never reach the model.** `explore --after` replays the login before the first planning turn, and the secret appears in none of the prompts, events or reports (`after_a_stored_login_the_model_starts_logged_in_and_never_sees_the_secret`).
- **No irreversible tool in the default or read-only profiles** (`only_the_full_profile_lists_a_tool_that_changes_the_machine`).
- **Model-chosen paths stay in the project** (`tools::paths`, the `run_task` escape tests, `run_task_suite_must_be_inside_the_project`). A `suite` file in the tasks folder (written by a task the agent started, including the lines `task_inject` appends) gets the same checks as an inline `yaml` (`a_suite_in_the_tasks_folder_gets_the_inline_checks`).
- **A suite the agent writes reads no host secret** (`crates/atalanta-mcp/src/authored.rs`, review 05 #1). Outside `--profile full`, `run_task {yaml}` is refused when a `${…}` names anything but a name its own `http` steps capture or, in typed text only, an allowed secret (`atalanta_core::env::placeholders` lists them, visiting the same strings interpolation fills); when `config.ai` sets the provider, `apiKeyEnv`, `baseUrl`, `region`, `project` or `apiVersion`; or when a mock serves a host file. Its `atalanta run` child starts from an empty environment plus an allow-list (`authored::child_env`), so a variable the check missed is not there to read (`an_agent_written_suite_can_not_send_a_host_secret_in_the_core_profile`, `a_child_with_its_own_environment_inherits_nothing_else`). Residual: an allowed secret typed into a page the agent opened can be sent on by that page, as with `act {secret}`; AI-provider credentials stay in the child's environment, reachable only by the provider the user configured.

The adaptive red-team round (an attacker model writing screens against these rules) is in the backlog, to run once `explore` is promoted at launch.

## Recommendations, in priority order

1. ~~Fix R-1~~ (done 2026-10-25 for AtalantaRunner). For WebDriverAgent
   (R-1W), prefer `ios.runner: atalanta`, or patch a `Host`/`Origin` check
   into its `FBWebServer` as the MJPEG bind is patched.
2. ~~Fix R-4~~ (done 2026-10-25). Run the wired smoke on a real iPhone to
   confirm usbmuxd delivers to loopback (P5).
3. ~~Fix R-2~~ (done 2026-10-25, protocol 13; mutual handshake in protocol
   18). ~~Give the video server the same token (residual of R-3).~~ (done
   2026-11-11, video server 2)
4. ~~Pass the explore goal after `--` (I-2), and add a CSP to the console
   UI (I-3).~~ (done 2026-11-11)
5. ~~Keep `cargo deny` in CI and resolve the open RustSec advisories.~~
   (done 2026-11-11: advisories pass, with the two unmaintained crates that
   have no successor ignored with reasons in `deny.toml`; security.yml
   checks them on every dependency change and weekly)
