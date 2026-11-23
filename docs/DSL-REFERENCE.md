# Atalanta DSL reference

<!-- Generated from crates/atalanta-core/src/schema.rs by `atalanta schema --markdown`.
     Do not edit by hand: `cargo test -p atalanta-core --test dsl_reference` fails when this
     file is out of date; `ATALANTA_BLESS=1` on that test rewrites it. -->

Every key a suite file can use. For how the pieces fit together, with examples, see
[DSL.md](DSL.md). Editors can validate suites with the JSON Schema from
`atalanta schema -o atalanta.schema.json`.

## Suite file

- `appId` (string) - Bundle id / package name (a suite of only http steps may leave it out)
- `config` (see below)
- `flows` (see below)
- `tests` (see below, required)

### `tests[]`

- `dependsOn` (list of string)
- `depends_on` (list of string)
- `description` (string) - What the test shows; verify: final checks it on the last screen (the name stands in without it)
- `isolated` (boolean)
- `name` (string, required)
- `sideEffects` (boolean) - Never skip this test from the result cache
- `steps` (list of step, required)
- `tags` (list of string)
- `verify` (one of `off`, `final`, `checkpoints`, `strict`) - This test's AI verification level, over config.verify

### `flows[]`

- `id` (string, required)
- `steps` (list of step, required)

## Steps

A step is a map with one action key and optional modifiers, or a plain-English string (a natural-language step).

Modifiers:

- `checkpoint`: string - The claim an AI check confirms after this step under verify: checkpoints or strict (on natural-language steps also true: their own waitUntil or intent).
- `optional`: boolean - A failure of this step is a warning; the test goes on. On runFlow: the flow stops at its first failing step and the test goes on after it.
- `settle`: one of `none`, `off`, `auto` or map: `capMs`?, `quietMs`?, `reactionMs`? - This step's settle: none (act and move on), auto, or its own timings.
- `timeoutMs`: integer ≥ 0.
- `force`: boolean - Act without the actionability checks (stable, enabled, hit target). Only on `tap`, `doubleTap`, `longPress`, `inputText`, `clearText`, `do`, `act`.
- `appInFront`: boolean - false: may pass while another app is in front. Only on `assertNotVisible`.
- `data`: string or map: `value`. Only on `do`, `act`, `assert`, `expect`.
- `waitUntil`: string. Only on `do`, `act`, `assert`, `expect`.

## Actions

### `launchApp`

Launch the app (or `appId`); `clearState: true` wipes its data first.

Argument: nothing or map: `appId`?, `arguments`?, `clearState`?.

- `appId` (string)
- `arguments` (map of key to value) - Launch arguments passed to the app (name -> string | bool | number); names are `[A-Za-z0-9_.-]`, not starting with `-`
- `clearState` (boolean)

### `stopApp`

Stop the app (or `appId`).

Argument: nothing or map: `appId`?.

- `appId` (string)

### `setConfig`

Change named settings or `sync.*` timings for the rest of this test; the test's end restores them.

Argument: map of key to value.

### `mockRequest`

Web only: answer requests matching `url` with a canned response, delay or block them, until `clearMocks` or the test ends.

Argument: map: `block`?, `body`?, `delayMs`?, `file`?, `headers`?, `json`?, `method`?, `status`?, `times`?, `url`.

- `block` (boolean)
- `body` (string)
- `delayMs` (integer ≥ 0)
- `file` (string)
- `headers` (map of key to value)
- `json` (any value) - JSON body
- `method` (string)
- `status` (integer ≥ 100)
- `times` (integer ≥ 1)
- `url` (string) - Glob over the whole URL (with * or ?), else a substring of it

### `clearMocks`

Web only: drop the mocks this test added (the suite's `web.mocks` stay).

Argument: nothing or map.

### `assertSystemState`

Android only: assert media playback (`mediaPlaying`, `mediaTitle`) or call state (`inCall`), polled until the step timeout.

Argument: map: `inCall`?, `mediaPlaying`?, `mediaTitle`?.

- `inCall` (boolean)
- `mediaPlaying` (boolean)
- `mediaTitle` (string)

### `tap`

Tap the element, after it is visible, enabled, still and not covered (`force: true` skips the checks).

Argument: selector.

### `doubleTap`

Double-tap the element.

Argument: selector.

### `longPress`

Press and hold the element (`durationMs`, default 1000).

Argument: map: `durationMs`?, `selector` or map (keys below).

- `durationMs` (integer ≥ 0)
- `selector` (selector)
- `…` (selector keys) - or the selector's keys inline, beside `durationMs`

### `drag`

Reorder: press the `from` element, hold it (`holdMs`, default 700, past the long-press time), move onto the `to` element over `moveMs` (default 900), rest so it drops in place, and lift.

Argument: map: `from`, `holdMs`?, `moveMs`?, `to`.

- `from` (selector)
- `holdMs` (integer ≥ 0) - Hold before moving, so the list picks the row up (default 700)
- `moveMs` (integer ≥ 0) - How long the move takes (default 900)
- `to` (selector)

### `inputText`

Type `text` into the element (focusing it first).

Argument: map: `selector`, `text`.

- `selector` (selector)
- `text` (string)

### `clearText`

Clear the element's text.

Argument: selector.

### `assertVisible`

Pass once the element is on screen; fail at the step timeout.

Argument: selector.

### `assertNotVisible`

Pass once the element is gone; the app must be in front unless `appInFront: false`.

Argument: selector.

### `assertText`

The element's text equals `expected` (polled until the step timeout).

Argument: map: `expected`, `selector`.

- `expected` (string)
- `selector` (selector)

### `scrollUntilVisible`

Scroll (`direction`, default down; `within` a container) until the element is on screen, up to `maxScrolls`.

Argument: map: `direction`?, `maxScrolls`?, `selector`, `within`?.

- `direction` (one of `up`, `down`, `left`, `right`)
- `maxScrolls` (integer ≥ 1)
- `selector` (selector)
- `within` (selector)

### `swipe`

Swipe in a `direction` (optionally `within` an element) or `from` one point `to` another.

Argument: map: `direction`?, `from`?, `to`?, `within`?.

- `direction` (one of `up`, `down`, `left`, `right`)
- `from` (map: `x`, `y`)
- `to` (map: `x`, `y`)
- `within` (selector)

### `screenshot`

Save a screenshot to the artifacts folder.

Argument: nothing or string or map: `filename`?.

- `filename` (string)

### `assertScreenshot`

Compare the screen with the baseline `atalanta-baselines/<platform>[-headless]/<W>x<H>/<name>.png`; a missing baseline is written and the step fails.

Argument: string or map: `clusterPx`?, `comparator`?, `crop`?, `mask`?, `name`, `threshold`?, `tolerance`?.

- `clusterPx` (integer ≥ 0)
- `comparator` (one of `strict`, `perceptual`) - strict (default): per-channel tolerance; perceptual: YIQ distance, threshold 0.1 (pixelmatch/Playwright)
- `crop` (map: `height`, `width`, `x`, `y` or selector) - Compare only this part: an element (selector) or a rect in screen coordinates
- `mask` (list of selector)
- `name` (string)
- `threshold` (number) - Share of compared pixels that must match (default 0.98); not a colour tolerance
- `tolerance` (integer ≥ 0) - comparator strict: largest per-channel difference that still matches (default 16)

### `saveState`

Save the app's data under a name (iOS simulator: data container clone; Android: run-as tar on debuggable builds, else an emulator snapshot).

Argument: string or map: `name`.

- `name` (string)

### `restoreState`

Put back app data saved with `saveState`; the app is left stopped.

Argument: string or map: `name`.

- `name` (string)

### `pressKey`

Press a key (`back`, `home`, `enter`, …).

Argument: map: `key`.

- `key` (one of `back`, `home`, `enter`, `volumeUp`, `volumeDown`, `backspace`, `tab`, `escape`, `delete`, `arrowUp`, `arrowDown`, `arrowLeft`, `arrowRight`)

### `hideKeyboard`

Hide the soft keyboard if one is showing (Android only for now: Escape, then Back if it stays up); a no-op when none is.

Argument: nothing or map.

### `wait`

Sleep `ms` milliseconds. Prefer an assertion: it waits only as long as needed.

Argument: map: `ms`.

- `ms` (integer ≥ 0)

### `runFlow`

Run the steps of a flow from `flows:` (by id).

Argument: string or map: `env`?, `flow`?, `flowId`?.

- `env` (map of key to value) - Parameters for this call: `${NAME}` inside the flow (and flows it runs) takes the value. Values are strings, integers or booleans; quote a decimal such as "1.50" so it is typed exactly as written
- `flow` (string)
- `flowId` (string)

### `tapXY`

Tap screen coordinates (no element lookup or actionability checks).

Argument: map: `x`, `y`.

- `x` (integer)
- `y` (integer)

### `inputRaw`

Type text into whatever has focus.

Argument: map: `text`.

- `text` (string)

### `goto`

Open a URL: on the web, navigate the page; on a device, open it as a deep link.

Argument: string or map: `url` or map: `link`.

- `url` (string)
- `link` (string)

### `openLink`

Same as `goto` (Maestro's name).

Argument: string or map: `url` or map: `link`.

- `url` (string)
- `link` (string)

### `goBack`

Web only: the browser's back button (on a device use `pressKey: back`).

Argument: nothing or map.

### `refresh`

Web only: reload the page.

Argument: nothing or map.

### `http`

Send an HTTP request; check `expect` (status, headers, JSONPath values, body, `maxMs`) and `capture` values as `${NAME}` for later steps. Needs no device.

Argument: map (keys below).

- `body` (string) - Text body
- `capture` (map of key to value) - NAME → where its value comes from; used as ${NAME} in later steps
- `expect` (map: `body`?, `headers`?, `json`?, `maxMs`?, `status`?)
- `followRedirects` (boolean)
- `form` (map of key to value)
- `headers` (map of key to value)
- `json` (any value) - JSON body
- `method` (string) - GET (default), POST, PUT, PATCH, DELETE, …
- `query` (map of key to value)
- `retry` (map: `count`, `intervalMs`?)
- `url` (string) - Absolute, or appended to config.http.baseUrl

### `exec`

Run a program on the host (no shell; a string is split on spaces) that `config.exec.allow` lists; check its exit code (`expect.exitCode`, default 0) and output (`expect.contains`), and `capture` its trimmed output as `${NAME}`. Off unless allowed; `ATALANTA_DEVICE_ID` is set for it.

Argument: map: `capture`?, `cwd`?, `expect`?, `run`.

- `capture` (string) - ${NAME} for its trimmed standard output, for later steps
- `cwd` (string) - Its working directory
- `expect` (map: `contains`?, `exitCode`?)
- `run` (string or list of string) - The program and its arguments: a string split on spaces (quotes keep a part whole; no shell) or a list. The program must be in config.exec.allow

### `do`

Natural-language action: a model picks one action and one element from the screen; the selector is built and checked in code.

Argument: string.

### `act`

Natural-language action: a model picks one action and one element from the screen; the selector is built and checked in code.

Argument: string.

### `assert`

Natural-language assertion, checked against the element tree.

Argument: string.

### `expect`

Natural-language assertion, checked against the element tree.

Argument: string.

## Selectors

A selector is a map. `css` or `xpath` (web only) stand alone; otherwise at least one of `id`, `text`, `textContains`, `textMatches`, `idMatches`, `accessibilityId`, `className`, `role` or `name` is needed, and the other keys narrow it. Every key given must match.

- `above` (selector) - Nearest match above this element
- `accessibilityId` (string)
- `below` (selector) - Nearest match below this element
- `checked` (boolean) - State filter: a checkbox / switch / radio is checked (only checkable elements have the state)
- `childOf` (selector) - A match inside this element
- `className` (string) - Full (android.widget.Button) or short (Button)
- `containsChild` (selector) - A match with this element inside it
- `enabled` (boolean) - State filter: the element is enabled
- `focused` (boolean) - State filter: the element has input focus. Never matches where the driver can't report focus (headless)
- `id` (string) - Whole id, or its short form after `:id/`
- `idMatches` (string) - Regex the whole id (or its short form) matches, ignoring case (Maestro's id:)
- `index` (integer ≥ 0) - Nth match in reading order
- `leftOf` (selector) - Nearest match left of this element
- `name` (string) - Accessible name (label, else text, else the content of a button/link/…), compared like text:
- `rightOf` (selector) - Nearest match right of this element
- `role` (one of `alert`, `alertdialog`, `application`, `article`, `banner`, `blockquote`, `button`, `caption`, `cell`, `checkbox`, `code`, `columnheader`, `combobox`, `complementary`, `contentinfo`, `definition`, `deletion`, `dialog`, `directory`, `document`, `emphasis`, `feed`, `figure`, `form`, `generic`, `grid`, `gridcell`, `group`, `heading`, `img`, `insertion`, `link`, `list`, `listbox`, `listitem`, `log`, `main`, `marquee`, `math`, `meter`, `menu`, `menubar`, `menuitem`, `menuitemcheckbox`, `menuitemradio`, `navigation`, `none`, `note`, `option`, `paragraph`, `presentation`, `progressbar`, `radio`, `radiogroup`, `region`, `row`, `rowgroup`, `rowheader`, `scrollbar`, `search`, `searchbox`, `separator`, `slider`, `spinbutton`, `status`, `strong`, `subscript`, `superscript`, `switch`, `tab`, `table`, `tablist`, `tabpanel`, `term`, `textbox`, `time`, `timer`, `toolbar`, `tooltip`, `tree`, `treegrid`, `treeitem` or string) - Role, by Playwright's role names (web: computed ARIA role; devices: the class or element type mapped, see the README's role table)
- `selected` (boolean) - State filter: the element is selected (tab, row, option). Never matches where the driver can't report selection (headless)
- `text` (string) - Exact visible text or label
- `textContains` (string) - Part of the text or label, ignoring case
- `textMatches` (string) - Regex the whole text, label or hint matches (ignoring case, `.` matches newlines; Maestro's text:)
- `css` (string) - Web only; alone in its selector.
- `xpath` (string) - Web only; alone in its selector.

## `config`

- `accessibility` (map: `audit`?, `density`?, `fail`?) - Accessibility audit of the screens each test reads (docs/DSL.md#accessibility-audit)
- `actionability` (one of `strict`, `lenient`) - strict (default): an action fails when its target never holds still, stays disabled or is covered; lenient: act anyway with a warning
- `ai` (map: `apiKeyEnv`?, `apiVersion`?, `baseUrl`?, `exploreModel`?, `images`?, `model`?, `project`?, `provider`?, `region`?) - The model service for natural-language steps and checks; ATALANTA_NL_* environment variables win over it
- `app` (string or map: `android`?, `ios`?) - App build Atalanta installs before the first test (skipped when the device has this exact build): one path, or one per platform
- `artifacts` (map (keys below))
- `batch` (boolean) - Share one launch across consecutive tests
- `dialog` (map)
- `driver` (one of `device`, `headless`)
- `exec` (map: `allow`?)
- `flutter` (map: `hierarchy`?)
- `framework` (one of `native`, `react_native`, `reactnative`, `rn`, `flutter`)
- `headless` (map (keys below))
- `healing` (map: `confidenceThreshold`?, `enabled`?, `mode`?, `persistHealed`?)
- `http` (map: `baseUrl`?, `headers`?, `insecure`?, `timeoutMs`?)
- `nl` (map: `allowCrossOrigin`?, `dataPolicy`?)
- `performance` (map: `enabled`?, `intervalMs`?, `pssGrowthThresholdPct`?) - Opt-in memory profiling and the memory check (Android only: the other drivers report no memory)
- `platform` (one of `ios`, `android`, `web`)
- `reactNative` (map) - Ignored: React Native has no headless renderer (run on a device or simulator)
- `reset` (map: `appPath`?, `snapshot`?, `strategy`?)
- `retry` (map: `count`?, `flakyResult`?)
- `screenshots` (map: `freezeStatusBar`?)
- `settings` (map) - Driver and runtime settings (`atalanta settings` lists them); flat `android.scroll:` or nested keys
- `sync` (map: `intervalMs`?, `maxTimerMs`?, `mode`?, `quietMs`?, `reactionMs`?, `settleCapMs`?, `stabilityCount`?, `timeoutMs`?)
- `verify` (one of `off`, `final`, `checkpoints`, `strict`) - AI checks: off (default; deterministic assertions only), final (the test's description holds at the end), checkpoints (also after assert: and checkpoint: steps; failures reported at the end), strict (the first failed check fails the test)
- `web` (map: `block`?, `device`?, `mocks`?, `parallelMode`?, `settle`?)

### `config.accessibility`

- `audit` (boolean) - Audit each distinct screen once per test and report findings as warnings (default off)
- `density` (number) - Android pixels per dp for the 48 dp target size (default: the screen's short side is 411 dp)
- `fail` (boolean) - Fail a test on an error-severity finding

### `config.ai`

- `apiKeyEnv` (string) - Name of the environment variable holding the key (never the key itself)
- `apiVersion` (string) - Azure OpenAI api-version (deployments URL); unset uses /openai/v1
- `baseUrl` (string) - API base URL, e.g. http://localhost:11434/v1
- `exploreModel` (string) - Model for goal-level planning (atalanta explore)
- `images` (boolean) - Send screenshots to the checker (default: the provider's)
- `model` (string) - Model for single steps and checks; may be provider:model
- `project` (string) - Vertex Google Cloud project
- `provider` (one of `anthropic`, `openai`, `azure-openai`, `gemini`, `openrouter`, `groq`, `mistral`, `deepseek`, `xai`, `together`, `fireworks`, `dashscope`, `perplexity`, `bedrock`, `vertex`, `ollama`, `lmstudio`, `vllm`, `openai-compatible`) - A provider preset: the native APIs (anthropic, openai, azure-openai, gemini, bedrock, vertex), OpenAI-compatible services (openrouter, groq, mistral, deepseek, xai, together, fireworks, dashscope, perplexity), local servers (ollama, lmstudio, vllm) or openai-compatible with baseUrl; docs/AI-PROVIDERS.md
- `region` (string) - Bedrock AWS region or Vertex location

### `config.artifacts`

- `logs` (one of `off`, `on-failure`, `onFailure`, `always`) - Capture device logs per test (logcat, simulator log stream, browser console), kept for failed tests by default; secrets are masked
- `onFailure` (boolean)
- `on_failure` (boolean)
- `outputDir` (string)
- `output_dir` (string)
- `screenshots` (one of `always`, `onFailure`, `on_failure`, `onfailure`, `never`) - screenshot: steps write at once, only for a failing test, or not at all
- `stepScreenshots` (boolean) - Screen after every step, written only for a failing test
- `step_screenshots` (boolean)
- `video` (one of `off`, `on-failure`, `onFailure`, `always`) - Record each test's screen (Android: H.264 ring buffer; iOS simulator: simctl recordVideo; web: screencast frames), kept for failed tests by default, as traces/<test>/video.mp4
- `videoSeconds` (integer ≥ 1) - Android: seconds of video kept in memory per test (default 30)
- `video_seconds` (integer ≥ 1)

### `config.dialog`

- Also read: `enabled`, `maxDismissals`, `customDismissLabels`.

### `config.exec`

- `allow` (list of string) - Programs exec steps may run, exactly as a step names them (adb, ./scripts/seed.sh); exec is off without them

### `config.flutter`

- `hierarchy` (one of `semantics`, `widgets`) - semantics (default) or widgets (deprecated opt-out)
- Also read: `projectPath`, `target`, `goldenDir`, `port`.

### `config.headless`

- `appPath` (string)
- `app_path` (string)
- `density` (number)
- `devicePixelRatio` (number) - Physical px per logical px: native headless density, Flutter test surface ratio
- `device_pixel_ratio` (number)
- `fontScale` (number) - Text size multiplier for native headless renders (default 1)
- `font_scale` (number)
- `height` (integer ≥ 1)
- `initialLayout` (string)
- `initial_layout` (string)
- `width` (integer ≥ 1)

### `config.healing`

- `confidenceThreshold` (number)
- `enabled` (boolean)
- `mode` (one of `auto`, `report`, `off`) - auto: heal and pass; report: fail with the proposed patch (default in CI); off
- `persistHealed` (boolean)

### `config.http`

- `baseUrl` (string) - Prefix of an http step's url without a scheme
- `headers` (map of key to value)
- `insecure` (boolean) - Accept any TLS certificate (a dev server's self-signed one)
- `timeoutMs` (integer ≥ 1) - How long a request may take (default 30000); a step's timeoutMs overrides it

### `config.nl`

- `allowCrossOrigin` (boolean) - A natural-language goto may leave the current page's origin
- `dataPolicy` (one of `verbatim`, `describe`) - describe: the model sees only a description of each data value

### `config.performance`

- `enabled` (boolean) - Sample the app's memory and CPU during the run (default false)
- `intervalMs` (integer ≥ 1) - Sampling interval during long actions, in ms (default 500)
- `pssGrowthThresholdPct` (integer ≥ 0) - Fail a test whose peak total PSS is more than this percent above its saved baseline (default 15). Only clean passes update the baseline. Was heapGrowthThresholdPct, which still works with a warning

### `config.reactNative`

- Also read: `bundlePath`, `component`, `port`, `nativeMocks`.

### `config.reset`

- `appPath` (string)
- `snapshot` (string)
- `strategy` (one of `clear`, `reinstall`, `snapshot`)

### `config.retry`

- `count` (integer ≥ 0)
- `flakyResult` (one of `pass`, `fail`) - fail: a test that passed only on retry exits 1 (as --fail-on-flaky)

### `config.screenshots`

- `freezeStatusBar` (boolean) - Freeze the status bar (Android demo mode, iOS simulator simctl status_bar) before assertScreenshot and compare it instead of masking it
- `freeze_status_bar` (boolean)

### `config.sync`

- `intervalMs` (integer) Deprecated.
- `maxTimerMs` (integer ≥ 0) - Native probe: one-shot timers due further out than this don't keep the app busy (default 1500; iOS probe)
- `mode` (one of `auto`, `polling`, `native`, `none`)
- `quietMs` (integer ≥ 0)
- `reactionMs` (integer ≥ 0)
- `settleCapMs` (integer ≥ 0)
- `stabilityCount` (integer) Deprecated.
- `timeoutMs` (integer ≥ 0)
- Also read: `adaptive`, `nativePortIos`, `nativePortAndroid`, `probeConnectTimeoutMs`.

### `config.web`

- `block` (list of string)
- `device` (string) - A mobile-web profile from Playwright's device list (any name, or "<name> landscape"): viewport, pixel ratio, user agent, mobile layout and touch (Chromium, not Safari)
- `mocks` (list of map: `block`?, `body`?, `delayMs`?, `file`?, `headers`?, `json`?, `method`?, `status`?, `times`?, `url`)
- `parallelMode` (one of `contexts`, `processes`) - What each --parallel browser is: contexts (default: browser contexts in one Chrome, each with its own cookies, storage and mocks) or processes (a Chrome process each)
- `settle` (map: `capMs`?, `quietMs`?, `reactionMs`?) - Web-only settle timings; they win over sync.* on the web (defaults: 30 ms reaction, sync.quietMs, 1 s cap)
- Also read: `startUrl`, `width`, `height`, `chromePath`, `headless`, `extraArgs`.

## Settings

Named settings go under `config: settings:` (flat `android.scroll: semantic` or nested), per run as `atalanta run --set key=value`, per test with `setConfig`, or as the environment variable. `atalanta settings` prints this table with the current values.

| Setting | Values | Default | Variable | What it does |
|---|---|---|---|---|
| `android.text` | auto\|fast\|keys | auto | `ATALANTA_ANDROID_TEXT` | How inputText/clearText put text in: one ACTION_SET_TEXT (fast), key events (keys), or fast for 16+ characters (auto) |
| `android.imeEnter` | 1\|0 | 1 | `ATALANTA_ANDROID_IME_ENTER` | pressKey enter uses the keyboard's own action (Search/Go/Send) first |
| `android.scroll` | drag\|semantic | drag | `ATALANTA_ANDROID_SCROLL` | scrollUntilVisible: touch drags, or the list's own scroll action (~3x faster, no touch) |
| `android.keepAccessibility` | 1\|0 | 0 | `ATALANTA_ANDROID_KEEP_A11Y` | Keep TalkBack and other accessibility services running during the run |
| `android.eventSettle` | 1\|0 | 0 | `ATALANTA_ANDROID_EVENT_SETTLE` | Settle on the device from accessibility events instead of tree reads |
| `android.keepAnimations` | 1\|0 | 0 | `ATALANTA_KEEP_ANIMATIONS` | Leave device animations on (Atalanta turns them off by default) |
| `android.treeServer` | 1\|0 | 1 | `ATALANTA_ANDROID_TREE_SERVER` | Use Atalanta's on-device tree server (0: uiautomator dump, much slower) |
| `android.composeTestTag` | 1\|0 | 1 | `ATALANTA_ANDROID_COMPOSE_TESTTAG` | Read a Compose testTag as the element's id when the app doesn't set testTagsAsResourceId (tree server, API 26+; costs up to 50 ms of main-thread reads per full dump, cached per node) |
| `android.focusCheck` | 1\|0 | 1 | `ATALANTA_ANDROID_FOCUS_CHECK` | inputText fails when its tap left the focus on another element, instead of typing there |
| `android.adbMode` | subprocess\|tcp | subprocess | `ATALANTA_ADB_MODE` | How adb is reached: the adb binary, or the adb server's TCP protocol |
| `ios.runner` | wda\|atalanta | wda | `ATALANTA_IOS_RUNNER` | iOS automation server: WebDriverAgent, or Atalanta's own XCUITest runner |
| `ios.launch` | simctl\|wda | simctl | `ATALANTA_IOS_LAUNCH` | Simulator relaunch in a warm session: simctl (~0.85 s) or WDA (~1.7 s) |
| `ios.systemAlerts` | 1\|0 | 1 | `ATALANTA_IOS_SYSTEM_ALERTS` | Look for SpringBoard alerts (permissions, Open in…) so they can be answered |
| `ios.text` | auto\|paste\|keys | auto | `ATALANTA_IOS_TEXT` | How inputText puts text in: the pasteboard and Cmd+V (paste, ~0.4 s for any length), typed keys (~9 ms a character), or paste into empty fields when quicker: 50+ characters, 100+ for the first after a launch (auto) |
| `ios.typingFrequency` | keys per minute | 180 | `ATALANTA_IOS_TYPING_FREQUENCY` | WDA typing speed |
| `ios.fullSource` | 1\|0 | 0 | `ATALANTA_IOS_FULL_SOURCE` | Read WDA's full source (visible/accessible attributes; ~7x slower) |
| `ios.keepWda` | 1\|0 | 0 | `ATALANTA_KEEP_WDA` | Leave WDA running after the run so the next one reattaches |
| `ios.wdaRef` | a WDA git tag | pinned | `ATALANTA_WDA_REF` | WebDriverAgent version to build |
| `ios.teamId` | Apple team id | - | `ATALANTA_IOS_TEAM_ID` | Team that signs the runner for real devices |
| `ios.wdaBundleId` | bundle id | com.<team>.atalanta.WebDriverAgentRunner | `ATALANTA_WDA_BUNDLE_ID` | Runner bundle id on real devices |
| `ios.swipeMs` | ms | 250 | `ATALANTA_IOS_SWIPE_MS` | Stroke time of an iOS swipe gesture (it flings) |
| `ios.scrollMs` | ms | 350 | `ATALANTA_IOS_SCROLL_MS` | Stroke time of an iOS scroll drag (ends slowly, so it doesn't fling) |
| `ios.tapGuardStatusBar` | 1\|0 | 1 | `ATALANTA_IOS_TAP_GUARD_STATUS_BAR` | Refuse a tap whose only free part is under the status bar (0: tap there; for apps that hide the bar) |
| `ios.allowWifiRunner` | 1\|0 | 0 | `ATALANTA_IOS_ALLOW_WIFI_RUNNER` | Run the runner on an iPhone reached over Wi-Fi only (needs ios.runner: atalanta, whose per-launch token is then required; an ios.runnerToken must be 16+ characters); otherwise such a phone fails with "connect a cable" |
| `ios.runnerToken` | 16+ characters | - | `ATALANTA_IOS_RUNNER_TOKEN` | Fixed bearer token for AtalantaRunner in place of the random one Atalanta makes per launch (a runner at ATALANTA_WDA_URL started by hand needs it as its ATALANTA_RUNNER_TOKEN); every request carries the token (masked; use ${VAR}) |
| `ios.accessibilityDeadlineMs` | ms | 5000 | `ATALANTA_IOS_ACCESSIBILITY_DEADLINE_MS` | Longest WDA spends on one accessibility query; past it WDA aborts the request with an error (WDA's accessibilityDeadline setting, sent in seconds). The 5000 default is sent only to a WebDriverAgent Atalanta built from v16.14.1 or later; any other WDA gets nothing unless this is set. 0 leaves WDA's default |
| `ios.snapshotMaxDepth` | depth | 60 | `ATALANTA_IOS_SNAPSHOT_MAX_DEPTH` | How deep WDA (snapshotMaxDepth) and AtalantaRunner (its deep-tree recovery read) read the element tree; deeper nodes are left out (0 leaves the runners' defaults) |
| `ios.verifyTap` | 1\|0 | 0 | `ATALANTA_IOS_VERIFY_TAP` | Before a tap, ask AtalantaRunner what is drawn at the tap point (a private XCTest hit test, skipped where it is missing) and fail when something else is on top; also lets a tap under the status bar through when the app hides the bar. Needs ios.runner: atalanta |
| `ios.alertRecheckMs` | ms | 1000 | `ATALANTA_IOS_ALERT_RECHECK_MS` | How long a "no system alert" answer is trusted (each check ~80 ms) |
| `web.networkIdle` | 1\|0 | 1 | `ATALANTA_WEB_NETWORK_IDLE` | The settle after an action also waits for the page's requests to finish (not websockets, event streams or long polls) |
| `web.networkIgnore` | URL globs, comma-separated | - | `ATALANTA_WEB_NETWORK_IGNORE` | Requests the network-idle wait doesn't wait for (analytics, polling); also sent to the in-app sync probe as ignore_urls |
| `web.networkIdleMaxMs` | ms | 5000 | `ATALANTA_WEB_NETWORK_IDLE_MAX_MS` | A request open longer than this is a long poll: the settle stops waiting for it |
| `visibility.minArea` | 0..1 | 0 | `ATALANTA_VISIBILITY_MIN_AREA` | Share of an element's box that must be on screen for assertVisible/assertNotVisible to count it as visible (0: any pixel; Maestro uses 0.1) |
| `web.focusCheck` | 1\|0 | 1 | `ATALANTA_WEB_FOCUS_CHECK` | inputText fails when its click left the focus on something that takes no text (a focused cross-origin iframe counts as unknown and passes, unless it already had the focus before the click) |
| `web.frames` | 1\|0 | 1 | `ATALANTA_WEB_FRAMES` | Reach into cross-origin iframes: selectors find elements there, taps and typing land there, and their requests count for the network-idle wait (0: such frames are opaque boxes) |
| `web.visibility` | viewport\|page | viewport | `ATALANTA_WEB_VISIBILITY` | What visible means for web assertions: on screen (in the viewport, not clipped by an overflow ancestor) or anywhere on the page (the old rule) |
| `web.dialogs` | dismiss\|accept | dismiss | `ATALANTA_WEB_DIALOGS` | How a page's confirm() and prompt() are answered, in the page and in its cross-origin frames (alert() and beforeunload are always accepted); accept lets a suite test a confirm-guarded action |
| `web.promptText` | text | - | `ATALANTA_WEB_PROMPT_TEXT` | What an accepted prompt() returns (with web.dialogs: accept) |
| `web.insecureCerts` | 1\|0 | 0 | `ATALANTA_WEB_INSECURE_CERTS` | Accept any TLS certificate in Chrome (a self-signed dev server); off by default, so a network attacker can't serve a page (that a secret is typed into) with a forged certificate |
| `web.downloads` | deny\|a folder | deny | `ATALANTA_WEB_DOWNLOADS` | Where files the page downloads are saved (relative to the working directory); deny refuses them, with a warning naming the file |
| `selectors.strict` | 1\|0 | 0 | `ATALANTA_SELECTORS_STRICT` | Fail a tap, press or text entry whose selector matches several elements on screen, listing them (default: act on the first and warn; while unset the warning says 1.0 makes this an error, 0 keeps today's behaviour) |
| `actionableWaitMs` | ms | 2000 | `ATALANTA_ACTIONABLE_WAIT_MS` | Wait for a tap target to be enabled and still before acting anyway |
| `crash.watchAfterMs` | ms | 1000 | `ATALANTA_CRASH_WATCH_AFTER_MS` | When a slow post-action settle starts checking the crash log |
| `crash.watchEveryMs` | ms | 500 | `ATALANTA_CRASH_WATCH_EVERY_MS` | How often it checks after that |
| `crash.checkEveryMs` | ms | 1000 | `ATALANTA_CRASH_CHECK_EVERY_MS` | How often a waiting element lookup checks for a crash, and how often a physical iPhone's crash watcher asks for the app's state (at least 250 ms) |
| `breaker.threshold` | failures | 5 | `ATALANTA_BREAKER_THRESHOLD` | Device failures in a row before the circuit breaker opens |
| `breaker.resetMs` | ms | 30000 | `ATALANTA_BREAKER_RESET_MS` | How long an open breaker waits before restarting the session and probing |
| `commandTimeoutMs` | ms | 60000 | `ATALANTA_COMMAND_TIMEOUT_MS` | Longest a single adb/simctl/xcrun command may run |
| `secrets` | VAR,VAR,… | - | `ATALANTA_SECRETS` | Extra variables whose values are masked in every report and log |
| `nl.model` | model id | - | `ATALANTA_NL_MODEL` | Model that plans natural-language steps (default: the provider's step model, claude-haiku-5-5 for Anthropic) |
| `nl.effort` | low\|medium\|high | low | `ATALANTA_NL_EFFORT` | Planner reasoning effort |
| `nl.cache` | normal\|read-only\|frozen\|prune | normal | `ATALANTA_NL_CACHE` | How the natural-language plan cache is used |
| `nl.traces` | 1\|0 | 1 | `ATALANTA_NL_TRACES` | Save planner traces for `atalanta nl-eval` |
