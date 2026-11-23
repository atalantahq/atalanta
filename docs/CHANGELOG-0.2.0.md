# CHANGELOG 0.2.0: the long form

The `## [Unreleased]` entries in [CHANGELOG.md](../CHANGELOG.md) are one line each. This page keeps each long entry's full text as it stood on 2026-11-11: the measurements, the benchmark episodes that found the problem, and the reasoning. Each CHANGELOG line links to its entry here.

## Behaviour changes that can fail existing suites

<a id="ios-sheets"></a>

- **[may fail suites]** **iOS refuses taps behind sheets, popovers and in-app alerts.** A target behind a Sheet, Popover or in-app Alert, or behind the `PopoverDismissRegion` dimming view, is now refused as covered instead of being tapped (which used to hit the dimming view and dismiss the sheet). A bare selector prefers the match in front of the sheet. SwiftUI and UIKit page sheets are caught only through `PopoverDismissRegion`. Not yet run on a simulator or device.

<a id="ios-back"></a>

- **[may fail suites]** **iOS `pressKey: back` matches only a real Back button.** It taps a button whose label or id is exactly "Back", or id `BackButton`, and otherwise swipes from the left edge; the positional "left-most button in the left third" fallback is gone. Closing a sheet matches whole names only (`close`, `cancel`, `dismiss`, `✕`, `xmark`, `xmark.*`, `close_button`, `closeButton`): no `close *` prefix (it matched "Close account") and no bare `x`. Not yet run on a simulator or device.

<a id="explore-gates"></a>

- **[may fail suites]** **`atalanta explore` trusts a test only after its gates.** It replays 5 times by default (was 1), with timing jitter after the first. Then the wrong-path replay taps a neighbour of the last tap and the test must fail, and at least one assertion must name goal text. An untrusted test is still written, but explore exits 1 and the report's `gates.trusted` is false; `--no-gates` skips them. See [docs/EXPLORE.md](EXPLORE.md#trust-gates).

<a id="mcp-core"></a>

- **[may fail suites]** **`atalanta mcp` serves five tools by default.** The core profile lists `observe`, `act`, `run`, `artifacts` and `verify_change`; the first four each stand for several of the full tools and run exactly what they would. Their definitions are about 850 tokens of the agent's context instead of about 5,000. `act` also returns what changed on screen. An agent or config that calls a full tool (`tap`, `snapshot`, `run_task`…) gets an error naming the core call; `--profile full` serves all 32 tools (the 31 of earlier 0.2.0 builds plus `verify_change`). The installed agent rules teach the core tools. See [docs/MCP.md](MCP.md#core-tools).

<a id="values-goal"></a>

- **[may fail suites]** **`explore --values-from-goal` no longer copies screen text.** Text shown on a screen is untrusted (the app or a page wrote it), so copying it into a typed value now needs `--values-from-screen`. `--values-from-goal` alone accepts goal text and numbers. Pass both for the old behaviour; the AndroidWorld harness does.

<a id="memory-check"></a>

- **[may fail suites]** **The memory check fails the test.** With `config.performance.enabled`, a test whose peak total PSS rises more than `pssGrowthThresholdPct` (default 15) above its saved baseline now fails with `memory regression: …`, in the sequential and the parallel runner. It used to print a warning, in the sequential runner only. Only a clean pass updates the baseline, so the run after a breach fails too, until memory comes back down. The setting was `heapGrowthThresholdPct`, which is still read with a warning: it never measured heap. Android only.

<a id="iphone-loopback"></a>

- **[may fail suites]** **Cabled iPhones bind loopback by default.** `ATALANTA_IOS_DEVICE_LOOPBACK` flips: it used to opt in with `=1`; now loopback is the default and `=0` opts out. That usbmuxd delivers to the phone's loopback hasn't been checked on a real iPhone yet (P5). If the runner never answers on a cabled iPhone, set `ATALANTA_IOS_DEVICE_LOOPBACK=0`.

<a id="runner-token"></a>

- **[may fail suites]** **AtalantaRunner needs a token.** A AtalantaRunner you start yourself (for `ATALANTA_WDA_URL`) must get `ATALANTA_RUNNER_TOKEN`, with the same value in `ios.runnerToken` for Atalanta. Scripts that call the runner directly must send `Authorization: Bearer <token>` and no `Origin` header. Without a token the runner exits at launch.

<a id="tree-protocol"></a>

- **[may fail suites]** **Tree server protocol 13.** An older Atalanta that finds a protocol 13 server running asks it to quit and starts its own, as for any other version. Third-party tools that talked to `@atalanta_tree` directly must authenticate first: since protocol 18 with the `HELLO`/`PROOF`/`AUTH` HMAC handshake (see Security), not the bare token.

  Note (2026-11-11): the heading said "protocol 13"; the current protocol is 18, and the replace-on-mismatch rule applies to every version.

<a id="assert-not-visible"></a>

- **[may fail suites]** **`assertNotVisible` is stricter.** In 0.1.0 it passed whenever the lookup failed for any reason. Now it waits for the element to go, and fails in three cases: the screen can't be read until the deadline, the app has crashed, or another app is in front. Fix: for a step that leaves the app on purpose, add `appInFront: false` to the `assertNotVisible` step. The check is skipped automatically after `openLink`/`goto`, `launchApp` of another app and `pressKey: home`.

<a id="actionable"></a>

- **[may fail suites]** **Actions wait for an actionable target.** A `tap`, `doubleTap`, `longPress`, `inputText` or `clearText` waits until its target is enabled and its position is stable, and fails with "not actionable" if that never happens. In 0.1.0 it acted at once. Fix: wait for the real ready state. Opt-out: `force: true` on the step, or `actionability: lenient` in `config` (wait, then act anyway with a warning).

<a id="ios-covered"></a>

- **[may fail suites]** **iOS taps on covered elements fail.** In 0.1.0, iOS tapped the centre of the element's frame, even under the keyboard, a navigation or tab bar, an alert, the status bar, or off screen. Now a fully covered target fails with "covered by …", and a partly covered one is tapped on its visible part. Fix: dismiss the keyboard or scroll first. Opt-out: `ATALANTA_IOS_TAP_GUARD=off` turns off every guard; `ATALANTA_IOS_TAP_GUARD_STATUS_BAR=off` turns off only the status-bar rule.

<a id="merged-label"></a>

- **[may fail suites]** **`assertVisible` is no longer satisfied by part of a merged label.** An element found only as one part of a merged accessibility label (for example "Home" inside "Home, tab, 1 of 4") now fails, and the message names the full label. Fix: assert the full label, or use `textContains:` or `textMatches:`.

<a id="strict-parsing"></a>

- **[may fail suites]** **Stricter suite parsing.** Unknown keys inside an action (a misspelt `txet:`, or `timeoutMs` nested in the wrong map) are errors; in 0.1.0 they were silently ignored. `runFlow` with `when:` or another unknown key is an error; in 0.1.0 the condition was ignored and the flow always ran. Two tests with the same name are also an error. Fix: run `atalanta validate` on every suite, which exits 2 and names each problem.

<a id="exit-codes-ci"></a>

- **[may fail suites]** **Exit codes changed for CI scripts.** Configuration and usage errors exit 2, device problems (busy, unknown, lost) exit 3, unclassified errors and caught panics exit 5, and signals exit 129, 130 or 143. In 0.1.0 most of these exited 1. Fix: update scripts that treat any non-zero code as "tests failed", or that expect 1.

<a id="empty-selection"></a>

- **[may fail suites]** **An empty selection exits 2.** `--tags` or `--filter` that match no test now exit 2 and list the suite's tags; in 0.1.0 the run passed with zero tests. An empty shard, or an empty `--changed-since` selection without `--fail-if-none`, still exits 0. Fix: correct the tag or filter.

<a id="android-typing"></a>

- **[may fail suites]** **Android typing is checked.** Text goes in through the tree server, so any Unicode works. A field that reads back different text, other than formatting such as a mask, fails the step instead of being typed again; this includes text cut by `maxLength`. When the tree server can't run, the `adb shell input text` fallback refuses non-ASCII text, password fields and secrets. Fix: assert the formatted value, keep text within the field's limit, and check `atalanta doctor --deep` if the tree server doesn't start.

<a id="inspector-token"></a>

- **[may fail suites]** **The inspector is local and token-protected.** In 0.1.0 it listened on every interface with permissive CORS. Now it binds `127.0.0.1`, every API call needs the token from the printed URL, and requests from other origins are refused. Scripts that called the API without the token get 401. Fix: use the printed URL, or pass `--host 0.0.0.0` for LAN access, which still needs the token.

<a id="android-dialogs"></a>

- **[may fail suites]** **Android dialog auto-dismiss is limited to system packages.** Only dialogs from an exact list of system packages are dismissed, and localized permission prompts are matched by the grant button's resource id. An app's own dialog ("Delete?") is no longer auto-confirmed. Fix: tap the app's own dialog buttons in the test.

## Highlights

<a id="hl-explore"></a>

- **`atalanta explore "<goal>" --app-id <id>`**: an AI agent works through a goal on a device and writes an ordinary YAML test with assertions. Atalanta then replays that test with **no model calls** to check it holds, and repairs it once if a replay fails. CI runs the test like any other suite, with no model. `--script plan.json` plays a fixed plan instead of a model. See `docs/EXPLORE.md`.

<a id="hl-console"></a>

- **The console (`atalanta inspect --open`)**: one browser page for every booted Android device, iOS simulator and the web browser. It shows a live mirror you can click, drag and type on (Android H.264 at about 30 fps, iOS simulator MJPEG, Chrome screencast) and an element overlay that gives selectors and YAML. It can also run suites with live steps, run explore sessions you can steer, and show the `atalanta doctor` diagnostics.

<a id="hl-mcp"></a>

- **MCP server upgrade**: the tool count goes from 14 to 32 (five by default; `--profile full` for all). New are compact `snapshot`s with stable `@e` refs and delta mode, background tasks (`run_task`, `task_status`, `task_stop`, `task_inject`) that run suites or explore sessions, `describe_failure`, `inspect_trace` and `diagnose`. `atalanta mcp install` registers the server with Claude Code, Cursor, Codex, Windsurf, VS Code, Cline and Gemini CLI.

<a id="hl-evidence"></a>

- **Failure evidence**: a failed test keeps `video.mp4`, device and app logs, and a self-contained `trace.html` in `atalanta-results/traces/<test>/`. The trace shows every step's screenshot with the tap point, the matched element, the video seeked to the step and the logs around it. Android video comes from Atalanta's own encoder; no ffmpeg is needed. `--pause-on-failure` stops at the failing step so you can look at the device.

<a id="hl-providers"></a>

- **AI steps from 19 provider presets**: natural-language steps (`do:`, `assert:`), `verify:` checks and `explore` can use Anthropic, OpenAI, Azure OpenAI, Gemini, Bedrock, Vertex, OpenRouter, Groq, Mistral, DeepSeek, xAI, Together, Fireworks, DashScope, Perplexity, Ollama, LM Studio, vLLM or any OpenAI-compatible endpoint. Model output never becomes a selector or a typed value directly, and verified plans are cached so stable screens replay without calling the model.

## Added

<a id="selectors"></a>

- Selectors: `role:` and `name:` (Playwright's role names, mapped per platform), `textMatches:` and `idMatches:` (regex), the state filters `checked:`, `focused:`, `selected:` and `enabled:`, the relations `below`, `above`, `leftOf`, `rightOf`, `childOf` and `containsChild`, and `css:`/`xpath:` on the web.

<a id="run-flags"></a>

- `--retries` (a pass on retry is reported as flaky), `--fail-on-flaky`, `--detect-new-flakes`, `--shuffle`, `--hunt`, `--quarantine`, `--cache-results`, `--set KEY=VALUE`, `--provision`, `--app PATH` (install by hash), `--update-baselines[=missing]`, `--junit-classname`, and `--profile` (Chrome/Perfetto trace).

<a id="mcp-added"></a>

- MCP: 32 tools (`--profile full`) with read-only and destructive annotations, `--read-only`, `--notify` (webhook, script, JSONL, desktop), `type_text` with `secret:` (an environment variable name), session export as YAML with assertions, and MCP protocol versions up to 2025-11-25 plus the stateless 2026-07-28 revision.

<a id="agent-commands"></a>

- **Agent commands: drive a device from the shell, by ref.** `atalanta snapshot`, `tap`, `type`, `press`, `swipe`, `scroll`, `launch`, `stop`, `assert`, `wait`, `screenshot`, `export` and `session`, one MCP tool per process with the server's refs, leases, masking and recording. An action waits for the screen to settle and prints what changed. The session (device, refs, recording) is kept per git worktree or `--session`, so `@e7` means the same element in the next command and `atalanta export` writes the whole session as a test. `--json` prints one envelope (`{"v":1,"ok",…}`). Android, iOS and the web. See [docs/CLI.md](CLI.md#agent-commands).

<a id="agent-lease"></a>

- **An agent command holds the device for its whole length** (the action, the settle and the delta), so a second agent on the same device waits, or gets `DEVICE_BUSY` after 10 s (`ATALANTA_LEASE_WAIT_MS`, also what `atalanta run` now waits for a held device instead of failing at once), instead of acting between an action and the delta that reports it.

<a id="agent-codes"></a>

- **Failure codes for agents.** Every failed MCP tool result carries `structuredContent.error`: a stable `code` (`STALE_REF`, `NOT_FOUND`, `DEVICE_BUSY`, `TIMEOUT`, …), whether to `retry` (`safe`, `observe_first`, `no`), whether it was `delivered`, and the `next` step. The agent commands print the same. See [docs/MCP.md](MCP.md#failures).

<a id="mcp-deadline"></a>

- **MCP calls can't wedge the session.** Every device call has a deadline (55 s, `ATALANTA_MCP_CALL_TIMEOUT_MS`); past it the call is cut off, the driver session rebuilt, and the agent told the effect is unknown. A lost session or connection is rebuilt too, and a read retried once (actions never are).

<a id="drag"></a>

- **`drag: { from, to, holdMs, moveMs }`** for reordering lists (Maestro #1203, its most-voted open issue): press, hold past the long-press time, move onto the target, rest, lift. Android (`input motionevent`), iOS (W3C actions with a pause) and the web. See [docs/DSL.md](DSL.md#reordering-drag).

<a id="exec"></a>

- **`exec` step** (Maestro #1682): run an allowed host program (an `adb` command, a seed script) with no shell, check its exit code and output, and `capture` its output as `${NAME}`. Off unless `config.exec.allow` lists the program; an MCP agent can't add one to a suite it writes outside `--profile full`. See [docs/DSL.md](DSL.md#host-commands-exec).

<a id="failures-md"></a>

- **`failures.md` on every run**: each failed test with the step that failed and its `file:line`, the error, the end of its app log (error lines first, numbered), and the screenshot, trace, video and logs that show it. Its one-line explanation cites one of those files, or says "insufficient evidence" instead of guessing; the header counts both.

<a id="verify-change"></a>

- **`atalanta verify-change "<description>"`** (and the MCP tool `verify_change`): did this change do what it says on the device? A trusted saved flow for the description (keyed by sha256 of the app, the `--after` login, the goal and the platform) replays with no model; on a miss or a failed replay, `explore` writes a new one, saved only if it passes its trust gates. The verdict and the evidence go to one folder: `manifest.json`, `failures.md` (each failure with its step and the files it rests on), `comment.md` for `gh pr comment --body-file`, and the run's reports, video and screenshots. The cache-hit rate is kept and reported. `--script` plays a fixed plan instead of a model.

<a id="logins"></a>

- **Logins an agent can call without seeing the password.** `atalanta login <suite>` runs a stored login flow once and leaves the app logged in; `atalanta explore --after login.yaml` (and MCP `run {goal, after}`) replays it with no model before the first planning turn, and the written test starts with it by name. `${keychain:NAME}` reads the OS keychain (`security` on macOS, `secret-tool` on Linux, or `ATALANTA_KEYCHAIN_CMD`), and `${totp:NAME}` types a fresh RFC 6238 one-time code at its step. Every value is masked. See [docs/DSL.md](DSL.md#logging-in).

<a id="docs-site"></a>

- **Docs site and `llms.txt`.** `docs/book.toml` and `docs/SUMMARY.md` build the docs with mdBook (with search); `.github/workflows/docs.yml` builds them on every docs change, after the snippet and generated-page checks, and deploys them to GitHub Pages on a release. `docs/llms.txt` indexes every page for AI agents, made from `docs/README.md` by a test. [docs/AGENTS.md](AGENTS.md) sets the CLI and skill path next to MCP for coding agents.

<a id="action"></a>

- **The Atalanta GitHub Action** (`action.yml`) and **`atalanta init --ci github`**. The Action installs a pinned release (optionally a pinned sha256), caches the AVD, brings up an Android emulator (one retry with a fresh AVD), an iOS simulator or headless Chrome, runs the suite or a shard, and uploads the results; a device that never came up exits 3, never as a test failure. `init --ci github` writes `.github/workflows/atalanta.yml` with two shards and a `results check` job; the example `init` writes now uses the Action too, instead of `install.sh` from `main` with no pin. `install.sh --sha256 HEX` (`ATALANTA_SHA256`) pins the archive. See [docs/CI.md](CI.md#the-atalanta-action).

<a id="http"></a>

- **API testing: the `http` step and the `atalanta-api` crate.** A step sends a request (`method`, `url`, `headers`, `query`, a `json`/`body`/`form` body) and checks the response with `expect:` (`status`, `headers`, RFC 9535 JSONPath values under `json`, `body`, `maxMs`). Every check runs, and a failure lists each one that failed with the start of the body. Matchers are a plain value, or a map of `eq`, `ne`, `gt`, `gte`, `lt`, `lte`, `contains` (substring, element or object subset), `matches`, `exists`, `type`, `length` and `oneOf`. `capture:` sets `${NAME}` for the test's later steps, UI steps included, and a captured value whose name looks secret is masked like an environment variable's. Each test has its own cookie jar. `retry: { count, intervalMs }` polls a backend that catches up. `config.http` sets `baseUrl`, `headers`, `timeoutMs` and `insecure` for the suite. A suite whose selected tests have only `http` steps (with `wait`/`setConfig`) runs without a device, may leave out `appId`, and works with `--parallel`. See [docs/DSL.md](DSL.md#http-and-api-tests).

<a id="env-file"></a>

- `atalanta run --env-file PATH` (repeatable; also on `shard-plan`) reads `KEY=VALUE` files: `#` comments, `export `, quotes, no shell expansion. `--env` wins over a file, a later file over an earlier one, any file over the environment, and secret-looking names are masked as usual. Nothing is loaded unless named (Maestro's most-voted request after drag, #2817).

<a id="demos"></a>

- **`atalanta explore` shows the model the project's own tests most like the goal** as demonstrations (LearnAct: one demonstration took a model from 19% to 52%). At most two tests and 1,500 characters, typed text only as `${NAME}`, never in `--benchmark`, off with `--no-demos`, looked for under `--demos-from` (default: the folder `--out` writes to). Grounding is unchanged. The prompt version is now `goal-3`.

<a id="failure-kinds"></a>

- **Flakiness per failure kind.** History (format 3, `failureKinds`) keeps the kind of each failed run: not-found, assertion, timeout, crash, infrastructure, http or other. Flaky tests are reported as `flakiness 36% over 6 runs (timeout 27%, not-found 9%)` in the console, in report warnings and in `history merge`. Version 2 files load, with their failures of unknown kind.

<a id="migrate"></a>

- `atalanta migrate maestro`: `killApp` becomes `stopApp`, and the warnings for `runScript`/`evalScript` point to the `http` step. `eraseText` becomes one `pressKey: backspace` per character (50 by default, up to 100), and `clearState: { appId }` keeps its app. A `tapOn` point is converted only when it is in absolute pixels with no selector; a point with a selector is inside that element, and is now skipped with a warning (it used to become a screen tap).

<a id="hunt"></a>

- **`--hunt` explains flakes.** For each flaky test it reports where passing and failing rounds diverge and ranks the likely causes: the screen not done changing, a timeout too tight, order dependence, stress only. `--suggest` adds the YAML change for the top cause and hunts again with it applied in memory; the suite is never edited. Source: DoeFL, ICST 2026; FlakyGuard, ASE 2025.

## Changed

<a id="launch-app"></a>

- **Android `launchApp`** stops the app and reads the clock in one shell, takes the pid with the focus check, keeps the probe's port forward per device, and reuses a screen state seen ready in the last 2 s. That is 9 → 7 adb round trips on a first launch and 7 → 3 right after another (fake adb). On an API 34 emulator a launch took about 230 ms less, and `scrollUntilVisible` with its row already shown went from 326 ms to 8 ms.

<a id="exit-codes"></a>

- **Exit codes** follow one contract: 0 passed; 1 a test failed; 2 usage or configuration; 3 device or infrastructure (including a run where every failure was infrastructure); 4 suite timeout; 5 internal error (a caught panic, or an error of no known kind); 129, 130 and 143 for SIGHUP, SIGINT and SIGTERM. A folder run returns its most severe file's code. See `docs/CLI.md`.

## Deprecated

<a id="ambiguous"></a>

- **Ambiguous action selectors.** A tap or other action whose selector matches several elements warns that this will be an error in 1.0. The named setting `selectors.strict` chooses now: `1` (or `ATALANTA_SELECTORS_STRICT=1`, or `--set selectors.strict=1`) makes it an error today, and `0` keeps the current behaviour and silences the notice.

## Fixed

<a id="picker-wheels"></a>

- **iOS picker wheels: a dead WebDriverAgent is a device error.** A WDA that stops answering during a picker-wheel read now fails as a device error (exit 3), not an assertion failure. Under AtalantaRunner, which serves no picker-wheel routes, a picker-wheel step fails up front with a configuration error. Not yet run on a simulator or device.

<a id="swatch"></a>

- **Explore reads a swatch's colour.** A colour palette's buttons have no name, so the model matched them to the colours asked for by their place in the screenshot and picked wrong ones. Each small unlabeled element that is one solid colour in the turn's screenshot is now listed with it (`<Button> color=#00FFFF (cyan)`), read by code; icons and pictures are left alone.

<a id="pixel-grid"></a>

- **Pixel changes are judged in colour, on a finer grid.** A 1 px cyan stroke on a white canvas moved no cell's luminance enough, so a stroke that was drawn read as no change and the run stopped as stuck. The check now compares each colour channel on a 48×96 grid read from every second pixel; a change confined to one column a few cells tall (a blinking cursor) still does not count.

<a id="swipe-lanes"></a>

- **Repeated swipes inside one element take new lanes.** Every swipe crossed the element's middle, so on a canvas each stroke painted over the last. The first swipe in an element still goes through its middle; later ones are moved a quarter, three eighths, an eighth to either side. Code picks the lane; the model names only the element.

<a id="web-no-change"></a>

- **A tap in a web page that shows no change is not doubted.** A colour button in a drawing page only sets the pen; told "check you picked the right element", the model tapped it until the run stopped as stuck (AndroidWorld BrowserDraw). On a screen holding a web view, a tap that left the page as it was now gets a note saying it may have set state the page doesn't show, and to go on rather than tap it again, also when the tap moved focus or ends a streak.

<a id="late-return"></a>

- **Explore waits for a result that comes after a return.** Typing a message and sending it empties the field, so the screen is again the one before typing; the sent bubble came more than a second later. The model was told its steps "led back here without changing it" and sent the message twice (AndroidWorld SimpleSmsResend). When an action other than back, home or switching apps leads to a screen of the last four turns, explore looks again for up to 2 s first.

<a id="placeholder"></a>

- **An empty Android field no longer reads as filled with its placeholder.** Android reports a field showing its placeholder with the placeholder as its text, and the tree server passed that on: Markor's emptied name field read `"my_note"`, so a `clearText` that worked looked like a no-op and the model cleared it again and again (AndroidWorld MarkorCreateNoteFromClipboard ran out of turns). Such a field's text is now empty; the placeholder stays its `hint`, which text selectors still match. Tree-server protocol 17.

<a id="check-actions"></a>

- **The final check confirms actions, not answers.** Since answers go through the check, it refused correct answers on doubt ("no year is shown", "the list may go on") or against whatever the last screen showed, and the model changed them into wrong ones: AndroidWorld lost 7 question tasks in v7 (81.9% → 72.4%), and 4 still in v8 with facts held only to "not contradicted". Facts the goal asks to report are now code's to ground (the answer must already be grounded in text shown during the run) and not the check's to judge; a goal that only asks for information holds once answered. The actions a goal asks for must still be shown done, which is where the false successes were.

<a id="check-assumption"></a>

- **The final check no longer passes on an assumption.** It passed a Markor note that was typed but never saved ("Save wasn't tapped, so I'm assuming Markor … autosaves"); AndroidWorld found no file. The check is now told never to assume an app saved, sent or synced something by itself: the log or a screen must show it done. It is also told which parts of an accepted answer code found verbatim on a screen, so a fact cut from the log's excerpts is not refused as unseen; parts accepted only word by word, or as a number, are still for it to judge.

<a id="check-log"></a>

- **The final check sees what each action brought on screen.** Its log listed actions with screen titles only, so a step done in an earlier app (a message sent, a booking made) could never be confirmed from the last screen, and 6 of 9 iOSWorld multi-app runs ended unverified for want of evidence the check could not have. Each log line now carries up to ten texts that appeared on the screen after that action (code's record, secrets left out, redacted); the check's log budget is 24,000 characters.

<a id="clear-cache"></a>

- **Android reads the screen from a cleared accessibility cache (API 34+).** After tapping a file in Files, the "Open with" chooser had focus within 0.7 s, but on an API 34 emulator the tree server's reads kept returning the Files window for 2 to 7 s. Each read now clears UiAutomation's cache first, as Appium's UiAutomator2 server does before a page source. `UiAutomation.clearCache` exists only from API 34: on older devices this does nothing (the first version of this entry claimed it fixed AndroidWorld's API 33 emulator; it did not — those tasks were failing on a stale "always open with Chrome" default left by an earlier run). Tree-server protocol 16.

<a id="changed-number"></a>

- **A changed number is progress.** The screen hash folds digits so a clock is not a new screen, which also made a click that only changed a number (a counter, a page's value) read as a no-op: three such clicks stopped the run as stuck. A number that changed now counts as an effect, unless the text ticks on its own (a time, a percentage), and the history line names it (`(text changed: "3" is now "1")`) instead of "(screen unchanged)", so the model and the final check keep the values it saw. Changed texts are named on any screen that kept its shape, also when something else changed with them (a click that moved focus), and never one holding a secret's value.

<a id="back-loop"></a>

- **Explore stops walking a loop through `back`.** The model was told when it came back round to a screen, and walked the same loop again anyway: two AndroidWorld tasks lost 9 of their turns this way (Cancel instead of Confirm on a delete dialog; leaving a setup screen instead of Apply). Once a loop holding a `back` comes round unchanged, one more `back` from the screen it was pressed on is refused, and the model is told so on that screen. A second `back` there is allowed.

<a id="answer-check"></a>

- **An explore answer is checked before it ends the run.** A goal answered with `answer` ended as success on grounding alone, skipping the final check; every one of 16 iOSWorld multi-app false passes came from that path (the app was only searched or opened, the asked change never made). The answer now goes through the same final check as any claimed finish, which reads the whole action log (up to 80 lines) and requires every part of the goal, with facts in the answer. `--verify off` still skips it, and says so in the reason.

<a id="stale-ref"></a>

- **A snapshot ref no longer moves to another element after a navigation.** Elements whose id repeats on screen (every Android Settings row is `#title`) were identified by id and position only, so after tapping into a page `@e9` named whatever row was now ninth, and an agent acting on the ref it saw before would tap the wrong row. Such elements are now also identified by their text; the old ref fails with `STALE_REF`. Found on the emulator, 2026-11-07.

<a id="joined-text"></a>

- **Exported sessions no longer assert on a container's joined text.** After a launch the recorder could pick a web page's `<body>` (or an iOS cell), whose text is its children's joined; a replay matches an element by its own text, so the exported test failed on its second step. Such elements are skipped for text and label assertions, and iOS's `BackButton` id (on every pushed screen) is no longer used as one.

<a id="dom-id"></a>

- **Web elements keep their `id` attribute apart from their label.** The web driver stored a DOM `id` as the accessibility label, so `explore` and exported sessions wrote `tap: { text: plan-go }` for `<button id="plan-go">Continue</button>`, which the page never matches (found by the trust-benchmark pilot, `bench/pilot/`). `Element.dom_id` now holds it: `id:` selectors match it, it is the web's stable id for written tests (`tap: { id: plan-go }`), and web snapshots show it (`#plan-go`).

<a id="replug"></a>

- **A replugged iPhone no longer stays unreachable.** usbmuxd gives a replugged phone a new device number, and the runner's port forward kept the old one, so every request failed until Atalanta restarted. The forward now looks the UDID up again when a connection is refused, and uses the new number from then on (host tests with a fake usbmuxd; not yet checked on hardware).

<a id="proxies"></a>

- **Corporate proxies and custom CAs.** Outbound HTTPS trusted only Mozilla's bundled roots, so a corporate TLS proxy or mitmproxy broke the AI providers and `http` steps. It now trusts the operating system's store too, or, when set, `SSL_CERT_FILE` / `SSL_CERT_DIR` in its place. Requests to the iOS runner never go through `HTTP(S)_PROXY` / `ALL_PROXY`. See [docs/CLI.md](CLI.md#proxies-and-certificates).

<a id="runner-port"></a>

- An iOS runner is never launched into a taken port. A runner of the same simulator (either kind, or one whose token this process doesn't have) is stopped first; any other process holding the port is named in a device error. A runner that fails to come up 3 times within 2 minutes is no longer relaunched. The step fails with the new `RunnerFailing` device error (exit 3, not retried), naming the last failure. Before, a WebDriverAgent that couldn't bind its port aborted and was relaunched every few seconds.

<a id="perf-budget"></a>

- `perf_budget` was flaky (`tap` read 6 or 7 trees). The cause was the healing snapshot file that earlier runs, or the other test running at the same time, left in the crate directory, not timing: the first lookup of a selector reads one extra tree to record its snapshot. Each test now keeps that state in a temp dir, budgets the steady state, and checks that the first run costs at most that one read.

## Security

<a id="rule-of-two"></a>

- **The Rule of Two for agent sessions** ([docs/SECURITY.md](SECURITY.md#9-agents-the-rule-of-two)): a session may read untrusted screens, hold secrets, or take irreversible actions outside the app, but never all three. The default and read-only MCP profiles list no tool that changes the machine (a test keeps it so).

<a id="run-task-paths"></a>

- **MCP `run_task` keeps model-chosen paths in the project.** With a `goal`, its `out` (where the explore child writes the test) and `script` were joined onto the working directory unchecked, so an absolute path, `..` or a symlink could write or read anywhere the user can, for example a shell profile. They must now resolve inside the working directory or the task folder, `out` may not be a symlink, and a working directory of `/` or `$HOME` allows nothing (as for `describe_failure`'s read-only check).

<a id="runner-auth"></a>

- **AtalantaRunner refuses cross-site and unauthenticated requests.** Atalanta makes a random 128-bit token for every runner launch (simulators included) and sends it on every request; the runner refuses to start without one. It also answers 403 to any request with an `Origin` header, a `Sec-Fetch-Site` other than `none`/`same-origin`, or a `Host` that isn't `127.0.0.1`, `localhost` or `[::1]` with its own port (DNS rebinding). The token is kept in `~/.atalanta/wda/running/<udid>.token` (mode `0600`) so a later Atalanta process can reattach. WebDriverAgent is third-party and still unauthenticated; it stays bound to loopback (see `docs/SECURITY.md`, R-1W).

<a id="tree-input"></a>

- **The Android tree server drops non-protocol input, protocol 13.** The first line that isn't a command (the `POST / HTTP/1.1` of a browser's blind request to the forwarded port) closes the connection, so the body is never run. Every connection must also present a per-start token (`AUTH`) that the server leaves in a shell-only file on the device. Older servers (protocol 7 to 12) still work.

<a id="loopback"></a>

- On simulators, WebDriverAgent and AtalantaRunner listen on loopback. Atalanta patches its WebDriverAgent build so the MJPEG screen stream, which upstream serves on every interface without authentication, also listens on loopback. A WebDriverAgent you start yourself (`ATALANTA_WDA_URL`), or one kept running from an older build (`ATALANTA_KEEP_WDA`), still exposes the stream until it is restarted. On real iPhones, WebDriverAgent listens on the phone's interfaces unless `ATALANTA_IOS_DEVICE_LOOPBACK=1`.

  Note (2026-11-11): the last sentence is superseded by "Cabled iPhones bind loopback by default" above: loopback is now the default on cabled iPhones and `ATALANTA_IOS_DEVICE_LOOPBACK=0` opts out.

<a id="secrets"></a>

- Secrets come from environment variables. They are masked in every report, log, trace and GitHub Actions log (`::add-mask::`, multi-line secrets line by line), never sent to a model, and written into generated YAML only as `${NAME}`. Over MCP, an agent can type only the variables listed in `ATALANTA_SECRETS`.

<a id="tree-18"></a>

- **Android tree server protocol 18: mutual HMAC handshake.** The host sends `HELLO <nonce>`, the server answers `PROOF` (an HMAC with the per-start token), and the host then sends `AUTH` with its own HMAC; the token never crosses the socket. The host refuses servers older than 18 and reports an app squatting on the `@atalanta_tree` socket. Not run on a device.

<a id="mcp-suites"></a>

- **Agent-written suites over MCP (outside `--profile full`)** can't read arbitrary `${ENV}` or `${keychain:…}` values, can't send an allowed secret anywhere but typed text, can't override `config.ai` `provider`/`baseUrl`/`apiKeyEnv`, and can't serve a host file from `mockRequest`. Child processes get an allowlisted environment.

## Platform support

<a id="verification"></a>

- **Verification:** Android 14 emulators, iOS 26 simulators and headless Chrome on macOS were exercised for this release. Real iPhones have never been tested. Android hardware has been used once: a SUNMI tablet (Android 14), Settings only, on 2026-11-08. `docs/STATUS.md` lists what has been verified, and on what.
