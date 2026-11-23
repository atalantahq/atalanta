# Guide

The long form of the [README](../README.md): what each part of Atalanta does and the flags and settings around it. The command reference is [CLI.md](CLI.md), the suite format [DSL.md](DSL.md) and [DSL-REFERENCE.md](DSL-REFERENCE.md), and what has been verified on devices [STATUS.md](STATUS.md).

## Glossary

Terms these docs use, in the order a new user meets them.

| Term | Meaning |
|---|---|
| **suite** | One YAML file: an `appId`, optional `config`, reusable `flows`, and `tests` ([DSL.md](DSL.md#a-suite)). `atalanta run`, `validate` and `-c` all take a suite file. |
| **test** | One entry under `tests:`, a named list of steps. Maestro calls a test file a *flow*; Atalanta does not. |
| **flow** | A reusable group of steps under `flows:`, inlined by `runFlow: <id>` ([DSL.md](DSL.md#flows)). Some MCP tools still say "flow" for an exported test (`export_session_flow`); the console's `--flows-dir` is now `--tests-dir` (the old name still works). |
| **step** | One action or assertion in a test, such as `tap`, `inputText` or `assertVisible`. |
| **selector** | How a step names an element: `id`, `text`, `accessibilityId`, a role, and on the web `css:`/`xpath:` ([DSL.md](DSL.md#selectors)). |
| **ref** | A short handle like `@e12` that `atalanta snapshot` or MCP `observe` gives each element on screen. An agent acts by ref; a ref from an older screen fails with `STALE_REF` instead of hitting something else. |
| **settle** | After an action, Atalanta waits until the screen stops changing (unchanged for 150 ms, capped at 3 s with a warning) before the next step. This is why suites need no `sleep`. Details under [Speed](#speed). |
| **actionable** | A target that is enabled and holding still (two reads agree on its bounds). Atalanta checks this before a tap or text entry. |
| **lease** | A per-device lock (a file lock under `~/.atalanta/leases`) so two Atalanta processes on one host, such as a run, the console and an agent, never drive one device at once. |
| **grounding** | The rule that model output never becomes a selector directly: the model names an element by its number on the screen list, and code builds and checks the selector. Typed text follows the same rule unless you opt in otherwise ([EXPLORE.md](EXPLORE.md)). |
| **trust gates** | The checks a test written by `atalanta explore` passes before it is called *trusted*: it replays 5 times with no model, it fails when its last tap is moved to a neighbouring element, and one assertion names a word of the goal ([EXPLORE.md](EXPLORE.md#trust-gates)). |
| **healing** | When a selector finds nothing, Atalanta scores the elements on screen against snapshots of that element from earlier passing runs, and uses a match only when it is clear-cut. In CI it reports the fix instead of passing ([Flakiness, reports and healing](#flakiness-reports-and-healing)). |
| **baseline** | The saved reference image an `assertScreenshot` step compares against, under `atalanta-baselines/` next to the suite. `atalanta baselines` reviews and accepts new ones. |

## How it compares

Checked on 2026-10-30. Competitor facts come from their docs and issue trackers; nothing was run against a competitor. Atalanta's own figures are from [STATUS.md](STATUS.md).

| | Atalanta | Maestro (CLI 2.12) | Appium (3.x) |
| --- | --- | --- | --- |
| Runtime | One static binary (Rust) | JVM | Node.js server plus a driver per platform |
| Test format | YAML | YAML (plus JavaScript) | Code, in any client language |
| Android | Emulators; real hardware once (one tablet, Settings only) | Emulators and devices | Emulators and devices |
| iOS | Simulators; real iPhones coded but **never run** | Simulators only | Simulators and real devices |
| Model calls when a saved test replays | None | `assertWithAI` calls a model on every run | n/a |
| Local parallel runs | Yes (`--parallel`) | Yes (`--shard-split`, `--shard-all`) | Through the test runner or plugins (appium-device-farm) |
| Sharding | Balanced by past durations (`--shard-timings`) | Round-robin by count | Not built in (the test runner's job) |
| Retry on another device | Yes | Maestro Cloud | Not built in |
| MCP server | Yes (5 core tools, 33 with `--profile full`) | Yes (Maestro MCP; open issues on reliability) | Yes (appium-mcp, a separate project) |
| Reports | JUnit, JSON, HTML, Allure, GitHub annotations | HTML, JUnit | Through the client's test framework |
| Device clouds | None | Maestro Cloud | Every major cloud |
| Migrate from Maestro | Partial: scripts and `copyTextFrom` don't convert; `when:` becomes an optional block, `setLocation` an `exec` step | n/a | n/a |

Measured for Atalanta on 2026-10-25 (one Mac): `atalanta --version` and `validate` start in under 20 ms; peak memory during the Android smoke run was 15.5 MB, without video. No competitor start-up or memory figure was measured here, so none is given.

## Fast Android trees

On Android, Atalanta runs a tiny UI-tree server on the device (pushed automatically,
no APK) instead of shelling out to `uiautomator dump` for every screen read. On an API 34
emulator a dump drops from ~2 s to ~3 ms and a two-test suite from 69 s to 3.7 s. It falls
back to `uiautomator dump` on any problem; set `ATALANTA_ANDROID_TREE_SERVER=0` to turn it off.

## Test dependencies, resets and impact selection

```yaml
config:
  reset:
    strategy: reinstall        # clear (default) | reinstall | snapshot
    appPath: ./app-debug.apk   # for reinstall; snapshot: <name> for emulator snapshots
tests:
  - name: Log in
    steps: [...]
  - name: Checkout
    dependsOn: [Log in]        # runs right after "Log in", same device; skipped if it fails
    steps: [...]
```

`--changed-since <ref>` reads `atalanta-impact.yaml` (`tests:` / `flows:` mapped to path
patterns) and runs the affected tests plus anything that failed or flaked recently, is new,
or hasn't run in a week. Manifest, plist, resource, build-file or suite changes run everything.
The changed files are what the branch changed since it forked from `<ref>` (`<ref>...HEAD`,
the merge base), plus uncommitted edits and untracked files, from the repository root whatever
your `diff.relative` says. A shallow CI clone has no merge base: use `fetch-depth: 0` on
`actions/checkout`. A test nothing maps (no `tests:` entry, no mapped flow) **runs**;
`impact: { unmapped: skip }` in the map leaves it to the history rules instead. An empty
selection passes; `--fail-if-none` makes it exit 2 (it counts only what git and the map select,
not the history picks, so every shard of a sharded run decides alike). Patterns: `*`, `?`, `**`, `{a,b}`, a
directory, or a bare file name; `[` and `]` are literal (`app/[id].tsx`), and a leading `./`
is ignored.

```yaml
# atalanta-impact.yaml
impact:
  unmapped: run          # run (default) | skip
tests:
  Log in: [src/auth/]
flows:
  checkout_flow: ["src/cart/**", "app/[id].tsx"]
runAll: [config/flags.json]
```

## Rust-native performance

Atalanta is a single static binary with no runtime dependencies: no JVM warm-up, no garbage-collection pauses, no `node_modules`. `atalanta --version` and `validate` finish in under 20 ms (measured 2026-10-25). Device start-up is another matter: the first iOS run builds WebDriverAgent (about a minute, once per Xcode), and launching an app takes about a second on an Android emulator.

## Built-in flakiness tolerance

Every interaction waits for its element to appear, and every assertion waits for its condition (an element to go, text to settle). The default timeout is configurable, and `timeoutMs` overrides it per step. A test that passes only on retry is reported as **FLAKY**, not passed, and `atalanta-history.json` tracks each test's recent outcomes and flip rate. Self-healed selectors are always reported with the element they chose; a heal never passes silently, and an ambiguous heal is refused. The driver layer adds retry policies with exponential backoff, and circuit breakers that stop cascading failures when a device becomes unresponsive.


## Parallel-first execution

Run across every booted device, locally. Work goes through one queue, longest tests first; a failed test is retried on a **different** device (so a flaky test and a bad device can be told apart), retries share a run-wide budget, and a device that keeps failing for infrastructure reasons is quarantined and re-probed until it recovers. Devices are leased across processes, so two runs (or a run and an MCP agent) on one host never drive the same device (see "What a lease covers" below).

```bash
atalanta run tests/app.yaml --parallel 4
atalanta run tests/app.yaml --shard 1/3                       # CI shard 1 of 3 (1-based)
atalanta run tests/app.yaml --shard 1/3 --shard-timings junit-reports/   # balanced by durations
atalanta shard-plan tests/app.yaml --total 3                  # every shard's tests, no devices
```

`--shard M/N` is 1-based like Playwright, jest and nextest; `--shard-index I --shard-total N`
(0-based) still works, but not both together (exit 2). `--shard-timings` takes a
`atalanta-history.json` (or a folder holding one), a JUnit XML report, or a folder of JUnit
reports (failed and skipped cases don't count; `time="12,5"` with a decimal comma is 12.5 s, while `1,200` (a lone comma before exactly three digits) is 1200 s, English thousands grouping; a
report over 64 MB is skipped with a warning); every shard must get the same file. It is read
once, and a file that can't be read fails the run (exit 2). Each shard prints
`Shard 2/3: 13 of 40 tests; plan=<fingerprint>`: shards that agree on the partition print the
same fingerprint, and `atalanta shard-plan` prints it too (give it the same `--tags`, `--filter`,
`--env` and `--set` as the run). History-based selection
(`--last-failed`, and the "failed recently / new / stale" part of `--changed-since`) is applied
after sharding, so a test one shard's history picks is run by the shard that owns it.

On the web, `--parallel N` gives N independent browsers, the devices `web-0` … `web-{N-1}`
(one browser is the device `web`), each with its own cookies, storage, cache and mocks. By default
they are N browser contexts in one Chrome process (`web.parallelMode: contexts`), which is far
lighter than N processes; `web.parallelMode: processes` gives each its own Chrome process and
profile instead. A hung page restarts only its own context; if the shared Chrome dies, the next
session restart starts a new one for every device. `atalanta` closes the browsers at the end of
the run, on Ctrl-C too, and a second Ctrl-C or SIGTERM kills Chrome and removes its profiles. Web devices take no cross-process lease: they are
this run's own browsers, so two concurrent `atalanta run --parallel` web runs don't wait on each
other.

`--device <id>` is checked against the devices the driver lists before anything starts (exit 3
if it isn't there). Without `--parallel` it is the device itself: a device's exact id or name,
ignoring case (so a typo like `emulator-555` is caught). With `--parallel` it is the rule the
pool uses to pick devices: ignoring case, a single value matches any device whose name or id
contains it (or a `*` glob), and a comma list names exact ids. If the listing fails or takes over 15 s (adb or devicectl wedged), the check is
skipped with a warning; so is a remote adb id (`host:port/serial`) when `ATALANTA_ADB_SERVERS`
isn't set. In `--parallel` runs a device that drops off mid-run gets ~10 s to come
back; after that it is out of the run and its tests move to other devices. Device leases follow
cargo's rules: a lease dir on NFS runs without cross-process leases (with a warning), so does a
filesystem without lock support; any other lease error exits 3 and says to set
`ATALANTA_LEASE_DIR`. `ATALANTA_LEASES=off` turns cross-process leasing off (one device per
container).

What a lease covers: Atalanta processes on **one host** that use **one lease dir**. The default dir
is per user (`~/.atalanta/leases`), so two users on one farm node don't see each other's leases.
Give them a shared dir instead: `ATALANTA_LEASE_DIR=/var/lib/atalanta/leases` with the dir
group-writable (`chmod 2770` and a shared group, or `1777` like `/tmp`); lock files made there are
group- (or world-) readable and writable, so both users contend for one lock. Leases do **not**
span hosts: two machines that reach one device through a shared adb server (`adb -H`,
`ATALANTA_ADB_SERVERS`) each lock on their own disk and can drive it at once. Give each host its
own devices there.

CI in one command: `atalanta run tests/app.yaml --parallel 4 --provision --app build/app.apk` boots the missing devices (headless, read-only instances of one AVD; or `atalanta-<type>-<n>` simulator clones, reused most recently used first) while it hashes the build, then installs it on every device at once. The install is skipped where that exact build (SHA-256 of the `.apk`, or of the `.app` bundle's files) is already installed and unchanged since. `app:` in the suite config does the same as `--app`. `atalanta devices up --android 4 --ios 2 [--app …]` provisions ahead of time, and `atalanta devices down` stops only what Atalanta started for the same owner (recorded in `~/.atalanta/provisioned.json`; the owner is `--owner`, `$ATALANTA_OWNER` or the CI job id, so one CI job's `down` never stops another job's emulator; `--all` stops every owner's). Nothing boots below ~35% / 3 GB free memory or 3 GB free disk; the refusal says why.

## Speed

- **Settle:** after an action, the screen counts as settled once it has been unchanged for 150 ms (sampled every 25 ms; an action that changes nothing is done after 300 ms; capped at 3 s with a warning). Targets are checked before acting: enabled, and not moving.
- **iOS:** trimmed `/source` reads (556 → 58 ms), WDA's own idle waits off, W3C taps, one tree read per step, one WDA per simulator, and with `ATALANTA_KEEP_WDA=1` the next run reattaches to a running WDA and its session (the iOS Settings smoke suite: 19 s → 8–9 s wall on an iOS 26 simulator, 2026-10-25).
- **Android:** on-device tree server (~6 ms dumps), launches with `am start` on the resolved activity, and system animations off for the run, restored on the device even if Atalanta is killed (`ATALANTA_KEEP_ANIMATIONS=1` to opt out).
- `atalanta run … --profile trace.json` writes a Chrome trace (tests → steps → settles) for https://ui.perfetto.dev.
- **Android input through the tree server:** taps, swipes, keys and text are injected over the server's UiAutomation connection (any Unicode in `inputText`; no `adb shell input` process per action), screenshots come from it too, and taps check the on-screen windows first: a target under the keyboard is tapped where it's visible, or the step says what covers it.
- **iOS runner choice:** `ATALANTA_IOS_RUNNER=atalanta` uses Atalanta's own XCUITest runner (`probes/ios/AtalantaRunner`, a cold build took 25 s against WDA's 47 s on Xcode 26.0.1, and ran the Settings smoke suite in the same time) instead of WebDriverAgent.
- **Warm daemon:** `atalanta daemon start` keeps drivers and device sessions up between runs; `atalanta run … --daemon` uses it.

## App state, batching and caching

```yaml
config:
  batch: true                      # a plain launchApp after a passing test of the app is skipped
tests:
  - name: "Log in once"
    steps:
      - launchApp: { clearState: true }
      - runFlow: login
      - saveState: "logged-in"     # iOS simulator: APFS clone of the data container; Android: run-as tar (debuggable builds) or an emulator snapshot
  - name: "Redeem a voucher"
    dependsOn: ["Log in once"]     # same device, after the save; never cached
    steps:
      - restoreState: "logged-in"
      - launchApp: {}
      - tap: { text: "Vouchers" }
  - name: "Place an order"
    sideEffects: true              # never served from the result cache
    steps:
      - launchApp: {}
```

A saved state lives on the device that saved it, so a test that restores it should `dependsOn` the test that saves it: that keeps the two on one device and in order under `--parallel` and `--shuffle`, and keeps both out of the result cache.

`--cache-results` skips a test that already passed with the same app build, steps, config and device (model, OS and screen size). It is for hermetic UI tests: a backend, feature flag or host state that changes under the same build is not in the key, so tests with an `exec`, `http` or natural-language step always run, as do screenshot tests, `sideEffects: true` tests and `dependsOn` chains. In CI the key includes the branch (`GITHUB_HEAD_REF`/`GITHUB_REF_NAME`, `CI_COMMIT_REF_NAME`, `BUILDKITE_BRANCH`, `CIRCLE_BRANCH`, `BITBUCKET_BRANCH`; `ATALANTA_CACHE_SCOPE` overrides it), so one branch's passes never answer for another's. A pass is reused for 30 days, or `ATALANTA_CACHE_MAX_AGE` (`6h`, `2d`).

Cache keys are salted, because steps hold interpolated values such as PINs. Outside CI the salt is made once per machine (`~/.atalanta/cache-salt`). In CI (`CI` set) there is no salt by default, so a cache file restored between runs still hits; set `ATALANTA_CACHE_SALT` from a CI secret to salt the keys there too.

## Natural-language test steps

Write steps in plain English and let an LLM resolve them to concrete actions at run
time — the [passmark](https://github.com/bug0inc/passmark) model, built into the
runner:

```yaml
appId: "https://demo.vercel.store"
config:
  platform: web
tests:
  - name: Add product to cart
    steps:
      - "Click the Acme Circles T-Shirt"
      - do: "Select the color"
        data: { value: "White" }
      - do: "Add to cart"
        waitUntil: "the cart drawer is visible"
      - assert: "the cart shows the Acme Circles T-Shirt"
```

- **Bare strings** and `do:` are natural-language actions; `assert:` / `expect:` are checks.
- **`data`** supplies a value, and it is always the value used, whatever the model
  writes. **`waitUntil`** names a follow-up condition, and the step fails if that
  condition doesn't appear.
- **The model picks, Atalanta decides.** The planner sees a numbered list of the
  on-screen elements and answers with an operation and an element number. Atalanta
  builds the selector itself (stable id, then accessibility label, then exact text,
  indexed only when needed) and rejects answers that don't fit the screen. If the
  target isn't there, the step fails; the model doesn't guess.
- **Intelligent caching** — each step is planned once and cached to
  `.atalanta/nl-cache.json`, per platform. Reruns replay with no model call and no
  screen capture.
- **Self-healing** — if a cached action stops working (its element is gone), it is
  re-planned against the current screen and retried in the same run, and the re-plan
  is reported. Two cases are not re-planned in the step: a cached action that ran and
  changed nothing, though it changed the screen when it was planned, fails (a second
  action after the first one acted could do anything); and a failed `assert:` fails,
  because re-planning a check on the current screen could pass it on another element.
  Both drop their cache entry, so the next attempt (a retry or the next run) plans them
  afresh.
- **Values come from the test.** A typed value, an expected text, a URL or a scroll
  target must be the step's `data` or be quoted in the step; text on the screen never
  becomes one. A `goto` must be an `http`/`https` URL on the current page's origin
  (`nl.allowCrossOrigin: true` lifts the origin rule), and a key press is one of
  enter, back or home.
- **Mixes freely** with the explicit selector DSL — use whichever is clearer per step.

```bash
export ANTHROPIC_API_KEY=sk-ant-...      # or OPENAI_API_KEY, GEMINI_API_KEY, GROQ_API_KEY, …
atalanta run tests/fixtures/natural-web.yaml --platform web
```

Any of the 19 provider presets works: Anthropic, OpenAI, Azure OpenAI, Gemini, Bedrock, Vertex, OpenRouter, Groq, Mistral, DeepSeek, xAI, Together, Fireworks, Qwen, Ollama, LM Studio, vLLM or any OpenAI-compatible endpoint. Pick one with `ATALANTA_NL_MODEL=provider:model` or `config.ai`, and set only its key's variable ([docs/AI-PROVIDERS.md](AI-PROVIDERS.md)).

```yaml
config:
  nl:
    allowCrossOrigin: false   # a natural-language goto may leave the page's origin
    dataPolicy: verbatim      # describe: the model sees "<value, 5 chars>", never the value
```

Secret data (a registered secret, or a variable named like `*_PASSWORD`/`*_TOKEN`) is never sent to the model. Other `data` is sent as written, because steps like "select the colour" with `data: White` need the model to see it. With `nl.dataPolicy: describe`, every value is described instead (`<value, 5 chars>`) and masked as `<value>` in the screen list, at some cost in accuracy; the action still uses the real value.

Set `ATALANTA_NL_MODEL` to override the planner model (default `claude-haiku-5-5` on
Anthropic, a small model on every provider; provisional until measured with
`atalanta nl-eval --model … --recommend`) and `ATALANTA_NL_EFFORT` to override its effort
(default `low`). A plan is cached only after the run verified it, and test data is never part
of the cache key.

`verify: final|checkpoints|strict` (or `--verify`) adds an AI check that the test's
`description` holds on the final screen (and at `assert:`/`checkpoint:` steps). It is `off` by
default, so CI replays make no model calls. A verdict that cites no element on the screen is
"unknown", never a pass ([docs/DSL.md](DSL.md#ai-verification-verify)).
Natural steps work on every platform (web, iOS, Android); suites without them run
unchanged and need no API key.

## Selectors

```yaml
- tap: { text: "Pay", className: "Button" }          # every field must match
- tap: { text: "Edit", below: { text: "Shipping" } } # also above, leftOf, rightOf
- tap: { text: "Add", childOf: { className: "Cell", containsChild: { text: "Coffee" } } }
- tap: { text: "Add", index: 1 }                     # 2nd match in reading order
- assertVisible: { textMatches: "Order #\\d+" }      # regex, Maestro's text: semantics
- tap: { idMatches: "row_.*", index: 0 }             # regex on the id
```

`textMatches` and `idMatches` take a regular expression with Maestro's semantics: the whole text, label or hint (or the whole id, or its short form after `:id/`) must match, ignoring case, `.` matches newlines and `^`/`$` match at line ends; the pattern also matches as literal text, and with newlines read as spaces. An invalid pattern is a validation error. `atalanta migrate` writes them for Maestro `text:`/`id:` values that are real patterns (a class, quantifier, alternation, anchor or `\d`-style escape); a value that doesn't compile as one (`+44 20 …`, `* Email`, `Done :)`), which Maestro matches as plain text, or that only uses brackets as punctuation (`Price (USD)`) stays `text:`/`id:`. On iOS they resolve from the tree only (no WDA query), on the web through the element tree.

`id:` matches the whole id or its short form (`login` for `com.app:id/login`), the innermost match wins, and a tap whose bare selector matched several elements is reported on every platform, with the count the driver's own lookup saw (counted in that lookup, so a clear selector costs no second one; on the web only matches on screen count) and a ready-made selector for each match (`{ text: "Add", childOf: { id: "row_tea" } }`, or `{ text: "Add", index: 1 }`); it acts on the first in tree order, as Maestro does. `settings: { selectors: { strict: true } }` fails the step instead. While `selectors.strict` is unset the warning adds that this will be an error in 1.0 (strict becomes the default then); `selectors: { strict: false }` keeps today's behaviour and drops that line. Unknown keys are errors (`timeoutMs` belongs next to the action, not inside it).

Text is compared the same way on every platform: both the selector's value and the element's are normalised (zero-width characters dropped, every run of whitespace, newlines included, read as one space, trimmed), so `text: "Sign in"` finds a label drawn as `Sign\nin`. `text:` matches exactly first and, only when nothing matches, ignoring case; `textContains:` always ignores case. A merged label (React Native's `"Points, 120, View rewards"`, Flutter's newline-joined semantics) is matched by one of its parts only at the end of the wait and only for actions: `assertVisible` on such a part fails and names the merged label.

On the web, `accessibilityId:` is the element's accessible name, as a screen reader announces it (the label on Android and iOS): its `aria-label`, or the name computed from `aria-labelledby`, a field's `<label>`, `alt`, a button's or link's text, or `title`. An `aria-label` match wins: the computed names are only tried when no element's `aria-label` matches.

### Roles, accessible names and states

```yaml
- tap: { role: button, name: "Pay" }                 # Playwright's getByRole("button", { name: "Pay" })
- tap: { role: link, text: "Pay" }                   # a role with any other field
- assertVisible: { text: "Remember me", checked: true }
- tap: { text: "Next", enabled: true }               # waits until the Next button is enabled
- assertVisible: { role: tab, name: "Home", selected: true }
- assertVisible: { role: textbox, focused: true }
```

`role:` takes Playwright's role names (the ARIA roles `getByRole` accepts; an unknown one is a validation error naming the nearest). `name:` is the accessible name, compared like `text:` (exactly, then ignoring case; normalised): the label (Android content description, iOS accessibility label, `aria-label`, a field's `<label>`), else for a field its placeholder (Android `hint`, iOS `placeholderValue`; its text is the value, not the name). An iOS field with a placeholder and no label is named by the placeholder, not its accessibility identifier, as on Android, else the text, else for a role named by its content (a button, link, tab, list item …) the text inside it. Playwright compares `name` as a case-insensitive substring by default; Atalanta's is the whole name, as `text:` is.

The state filters `checked:`, `focused:`, `selected:` and `enabled:` (`true`/`false`) narrow another field and never find on their own. Only a checkbox, switch or radio has a checked state, so `checked: false` never matches a plain label. Likewise a state the driver can't read is unknown, and matches neither `true` nor `false`. Android, iOS, Flutter and the web report `focused` and `selected` on every element. The headless driver reports neither, so there `focused:` and `selected:` never match: `selected: false` doesn't match every tab, and `selected: true` matches none. An Android dump node without the attribute is unknown too. A step waits for the state like any lookup, so `assertVisible: { text: "Saved", checked: true }` waits for the box to be ticked. `atalanta migrate` carries Maestro's `enabled`/`checked`/`focused`/`selected` over.

On the web the role is computed in the page, as Playwright does: the first known token of an explicit `role=`, else the tag's implicit role (`<button>`, `<a href>` → `link`, `<input type=checkbox>`, `<input>` → `textbox`, `<input type=search>` → `searchbox`, `<select>` → `combobox`/`listbox`, `<h1>`–`<h6>` → `heading`, `<img alt>` → `img`, `<li>` → `listitem`, `<table>`/`<tr>`/`<td>`/`<th>`, `<nav>`, `<main>`, `<dialog>`, a named `<section>`/`<form>`, …). `selected` is `aria-selected` or an `<option>`'s selectedness.

The devices have no ARIA. Their element type maps to a role conservatively: only a type whose meaning is unambiguous gets one, and everything else (a static `TextView`/`StaticText`, layouts, `View`s) has no role, so `role:` never matches it. On Android a node's role description counts too when it names a role (Compose's `Role.Switch` and `Role.Tab` report a plain `View` with the description "Switch"/"Tab"; the tree server sends it since protocol 11); a localised description names none. `selected` is Android's and iOS's `selected`, Flutter's `isSelected`; a Flutter switch's toggled state is its `checked`. Roles, names and states resolve from the tree on every platform (on iOS never through a WDA query).

| Role | Android (class, also Compose) | iOS (`XCUIElementType…`) | Flutter (semantics) | Headless UIKit |
|---|---|---|---|---|
| `button` | `Button`, `ImageButton`, `AppCompatButton`, `AppCompatImageButton`, `MaterialButton`, `FloatingActionButton`, `ExtendedFloatingActionButton`, `ToggleButton` | `Button` | `Button` | `UIButton` |
| `link` | – | `Link` | `Link` | – |
| `checkbox` | `CheckBox`, `AppCompatCheckBox`, `MaterialCheckBox` | `CheckBox` | `Checkbox` | – |
| `switch` | `Switch`, `SwitchCompat`, `SwitchMaterial`, `MaterialSwitch` | `Switch`, `Toggle` | `Switch` | `UISwitch` |
| `radio` | `RadioButton`, `AppCompatRadioButton`, `MaterialRadioButton` | `RadioButton` | `Radio` | – |
| `radiogroup` | `RadioGroup` | `RadioGroup` | – | – |
| `textbox` | `EditText`, `AppCompatEditText`, `TextInputEditText`, `AutoCompleteTextView`, `MultiAutoCompleteTextView` | `TextField`, `SecureTextField`, `TextView` | `TextField` | `UITextField`, `UITextView` |
| `searchbox` | – | `SearchField` | – | `UISearchBar` |
| `combobox` | `Spinner`, `AppCompatSpinner` | `ComboBox` | – | – |
| `img` | `ImageView`, `AppCompatImageView` | `Image` | `Image` | `UIImageView` |
| `heading` | – | – | `Header` | – |
| `progressbar` | `ProgressBar` | `ProgressIndicator`, `ActivityIndicator` | – | `UIProgressView`, `UIActivityIndicatorView` |
| `slider` | `SeekBar`, `AppCompatSeekBar` | `Slider` | `Slider` | `UISlider` |
| `list` | `ListView`, `RecyclerView` | `Table`, `CollectionView` | – | `UITableView`, `UICollectionView` |
| `listitem` | – | `Cell` | – | `UITableViewCell`, `UICollectionViewCell` |
| `grid` | `GridView` | `Grid` | – | – |
| `tab` | `TabView` | `Tab` | – | – |
| `tablist` | `TabLayout`, `TabWidget` | `TabBar` | – | `UITabBar`, `UISegmentedControl` |
| `menu` | – | `Menu` | – | – |
| `menubar` | – | `MenuBar` | – | – |
| `menuitem` | – | `MenuItem` | – | – |
| `toolbar` | – | `Toolbar` | – | – |
| `alertdialog` | – | `Alert` | – | – |
| `dialog` | – | `Dialog` | – | – |

The inspector, MCP refs and natural-language steps offer `{ role, name }` when an element's text alone is ambiguous (a "Pay" label above a "Pay" button); MCP tool calls accept the flat form (`{"role": "button", "name": "Pay", "enabled": true}`) and `list_elements` shows each element's role and state.

**Visible** means the same on every platform for `assertVisible` and `assertNotVisible`: rendered, a non-empty box, and on screen by at least one pixel. On the web that also means inside the viewport and not clipped away by an `overflow` ancestor (a carousel's hidden slide), and `opacity: 0` counts as hidden. A covered element still counts as present. `settings: { visibility: { minArea: 0.1 } }` asks for a share of the box on screen instead (Maestro uses 10%), and `web: { visibility: page }` brings back the old web rule (anywhere on the page). Actions still scroll their target into view first, so assert below the fold after `scrollUntilVisible`.

**Actionability.** Before a tap, double tap, long press or text entry, the target must hold still (two reads in a row agree on its bounds, within 1 px) and be enabled. When the settle before the step has just seen the screen hold still (two identical trees, or the in-app probe's idle) and the target was found at once, that is the proof and no extra read is made; otherwise (no settle, a capped one, or a target that showed up late) the target is read again. On web the page checks stillness itself (two animation frames), the click must land on the target or on the button, link or label that holds it, not on an overlay or another ancestor, and a control inside an `aria-disabled` subtree counts as disabled. Atalanta waits up to 2 s for that (on top of the element's own lookup timeout), then fails the step as `not actionable`, saying whether the target was disabled or still moving (a pulsing button, marquee or carousel that never holds still for two reads). Some custom and React Native views report `enabled=false` but work: add `force: true` to the step to act without the checks, or set `actionability: lenient` in the suite config to act anyway with a warning (the behaviour before this rule).

```yaml
- tap: { id: "rn_pay_button" }
  force: true
```

**Negative checks need the app in front.** `assertNotVisible` fails when another app is in front (the launcher after a crash-free exit, a browser, a share sheet): that screen lacks the element too, so the check would prove nothing. After a step that leaves the app on purpose (`openLink`/`goto`, `launchApp` of another app, `pressKey: home`) the check runs on whatever is in front, with a note, until the suite's app is launched again. `appInFront: false` on the step allows it anywhere:

```yaml
- assertNotVisible: { text: "Share" }
  appInFront: false
```

## Visual checks

```yaml
- assertScreenshot: "home"
- assertScreenshot: { name: "cart", mask: [ { id: "clock" } ], clusterPx: 20 }
- assertScreenshot: { name: "card", crop: { id: "offer_card" } }          # compare only this element
- assertScreenshot: { name: "hero", crop: { x: 0, y: 120, width: 390, height: 300 } }  # or this rect
- assertScreenshot: { name: "chart", comparator: perceptual }             # or tolerance: 24
```

Compares with `atalanta-baselines/<platform>/<W>x<H>/<name>.png` next to the suite file once the screen holds still (`<platform>-headless` for headless runs; `<W>x<H>` is the device's full capture size, so phones, tablets and orientations each keep their own baseline). Baselines from older versions (`atalanta-baselines/<platform>/<name>.png`, no size folder) are still read, with a warning, when they are the captured size; new and re-recorded baselines are written only to the sized folder.

- **Pass/fail:** it fails on a *concentrated* change (a cluster of `clusterPx`, default 20, changed pixels, like a wrong price) even on a 99.9% identical screen, or when fewer than `threshold` of the compared pixels match. `threshold` is a **share of pixels** (0..1, default 0.98), not a colour tolerance as Playwright's `threshold` is.
- **When a pixel counts as changed:** `comparator: strict` (default) when any channel, alpha included, differs by more than `tolerance` (0..255, default 16); `comparator: perceptual` when the YIQ colour distance is above 0.1 of its maximum, as pixelmatch and Playwright decide (forgiving of shading drift between hosts, strict on visible changes; `tolerance` doesn't apply to it). Anti-aliased edges are ignored either way.
- **Masks and crops:** the status bar is masked, and so is every match of each `mask:` selector. `crop:` (an element's selector, or a rect in screen coordinates: points on iOS, pixels on Android, CSS px on web) compares only that part of the screen; a `crop:` selector that matches several elements crops to the one an action would use (the first in tree order) and warns, as an action does, and fails under `selectors.strict`. Masks may cover at most half of what is compared (the crop, when there is one); there is no opt-out, crop instead.
- **Missing baselines:** a missing baseline is written and the step **fails** ("created; review and commit it"), on a laptop and in CI alike, and a retry (or another device) in the same run doesn't pass on it; the next run, `atalanta daemon` and MCP included, compares with it. `--update-baselines=missing` writes missing baselines and passes, comparing the rest; `--update-baselines` (or `=changed`) rewrites every baseline that is missing or no longer matches. `ATALANTA_UPDATE_BASELINES=missing|changed|1` does the same (`1`/`true` is the bare flag); unset, empty, `0` or `false` is off, and any other value is a configuration error. When the name's baseline exists only at another `<W>x<H>` (a rotated screen, another device), this size's baseline is missing: `--update-baselines=missing` records it (so adding a device, or a first parallel run on two screen sizes, needs no `--update-baselines` that would rewrite the other devices' baselines), and a plain run fails with "size mismatch: baseline is WxH, capture is W'xH': no baseline at this size yet … record it with --update-baselines=missing" without writing anything. A file in the capture's own `<W>x<H>` folder whose pixel size differs from what is compared (a crop whose element changed size) fails as a size mismatch except under `--update-baselines`. Diffs (`-expected`, `-actual`, `-diff`) go to `atalanta-results/diffs/`, with a `-review.json` naming the baseline: `atalanta baselines review` lists them, `atalanta baselines accept <name|all>` writes the actual screen over the baseline (atomically; `--baselines-dir` for artifacts from a CI runner), and `atalanta baselines review --serve` (or `/review` on a running `atalanta inspect`) shows expected, actual and diff side by side with Accept and Reject buttons, behind the inspector's token (`--baselines-dir` there too, for CI artifacts). A record is cleared when a later comparison of the same baseline (same size folder and platform) passes or `--update-baselines` rewrites it, and accepting skips (with a warning) a record whose baseline changed after it was written, judged by content (the record keeps the baseline's SHA-256, or `absent`; records from older versions fall back to modification times with a warning), so CI artifacts unzipped into a fresh checkout accept cleanly and an old run's frame never replaces a newer baseline. Accepting writes only inside the `atalanta-baselines` folder (the recorded one, or `--baselines-dir`), never through a symlinked folder.
- **Frozen status bar** (Android: host tests against a fake adb only, not yet run on a device; [STATUS.md](STATUS.md)): `config: { screenshots: { freezeStatusBar: true } }` freezes the status bar before the first `assertScreenshot` on each device (Android SystemUI demo mode: 12:00, full battery and signal, no notifications; iOS simulators: `simctl status_bar override`, 9:41) and compares it instead of masking it, so pixels under an edge-to-edge bar count. Both are undone when the run ends, `--daemon` runs included; a device where it doesn't take (a real iPhone, or an Android device whose SystemUI doesn't enter demo mode: `sysui_demo_allowed` doesn't read back as 1, or SystemUI's own `sysui_tuner_demo_on` still reads 0 after the broadcasts are sent a second time) keeps the mask, with a warning. Before Android 12 SystemUI doesn't write `sysui_tuner_demo_on`, so there only the allowed setting is checked.

## Headless

`--driver headless` renders Android layouts and iOS XIBs/storyboards on the CPU. It is an **approximate smoke renderer**: layout, colours, text, opacity, corner radius and bold are drawn; attributes it parses but can't draw (images and drawables, elevation and shadows, transforms, custom fonts, italic) print one warning each per run. Use it to catch layout changes, not to match device pixels.

```yaml
config:
  driver: headless
  headless: { width: 1080, height: 1920, devicePixelRatio: 2.625, fontScale: 1.3 }
  flutter: { hierarchy: semantics }   # the default; `widgets` is a deprecated opt-out
```

- `devicePixelRatio`: physical px per logical px. For native headless it is the same as `density` (set one); for Flutter it sizes the test surface as `width` x `height` physical px at that ratio (unset: dpr 1).
- `fontScale`: text size multiplier for native headless renders (default 1), like the system font-size setting.
- `inputText` appends to what the field holds, as on devices; `clearText` empties it.
- Flutter headless matches the **semantics tree** by default, as the device drivers see the app (`text:` a merged label, `id:` a `Semantics(identifier:)`); `hierarchy: widgets` (or `ATALANTA_FLUTTER_HIERARCHY=widgets`) keeps the old widget tree with a deprecation warning. Flutter older than 3.19 is a configuration error in headless mode (an unknown version, such as `0.0.0-unknown` from a shallow SDK clone, skips the check) (see `crates/atalanta-flutter-bridge/README.md`).

## Flakiness, reports and healing

- `--report junit,json,html,github`: several at once. JUnit carries `flakyFailure`/`rerunFailure` per attempt; `report.html` is one self-contained file; `github` (automatic in GitHub Actions) adds annotations and a job summary.
- `atalanta-quarantine.yaml` next to the suite forgives only known failures (test + error signature), and only when *every* failure in the run is a known one. An entry can carry `expires: 2026-12-31` (after that date, in UTC, it warns and forgives nothing) and `flaky-only: true` (it forgives a flaky pass under `--fail-on-flaky`, never a hard failure). A panic in Atalanta itself is never forgiven.
- Exit codes: 0 passed; 1 a test failed (or was flaky, with `--fail-on-flaky`); 2 configuration or usage; 3 device or connection; 4 suite timeout; 5 internal (a panic in Atalanta, or a report it couldn't write; outranks 1). A test that panicked on any attempt stays an internal error even if a retry passes: it is still retried, so the report shows whether the app works, but it fails with exit 5, no quarantine entry forgives it, and `--fail-on-flaky` has no say. Ctrl-C, SIGTERM and SIGHUP stop the run, write its reports and exit 130, 143 and 129; a second signal kills the commands still running (including the Android CPU-stress `adb shell` loops and the tree server's `adb shell`; the device-side loops such a forced exit leaves are killed when the next `--stress` run starts) and exits at once (a second Ctrl-C within 1 s is the same one, as npm and yarn forward it twice).
- History across CI jobs, without a server: each job uploads its `atalanta-history.json`. `atalanta run --history-from 'history/*'` merges the downloaded ones into the run's history (a pattern matching nothing is a warning; an unreadable file exits 2), and `atalanta history merge 'history/*' -o merged` writes one merged file. Per test, the job that ran it last wins, and per-device outcomes are kept from every job. See `docs/CI.md` for the GitHub Actions upload and download steps.
- A run that did not complete (`interrupted:SIGTERM`, `timeout`) gets a warning banner atop its GitHub step summary. Every JUnit `<skipped>` has a `message`. If a stopped run doesn't return within the 20 s grace, the reports are written from the tests that had finished.
- Every line of a multi-line secret (a PEM key) is masked on its own, and GitHub's `::add-mask::` is sent per line.
- Colour: `--color auto|always|never` on any command. Otherwise `NO_COLOR` (off), then `CLICOLOR_FORCE` (on), then `CLICOLOR=0` / `TERM=dumb` (off), then whether the output is a terminal. One decision covers the CLI, the runner's result lines and the logs.
- `--fail-on-flaky` (or `retry: { flakyResult: fail }` in the suite config) makes a test that passed only on retry fail the run (exit 1); reports still show it as flaky.
- `--junit-classname suite` sets every JUnit `classname` to the suite file's stem (default `test`: the constant `atalanta`; changing it resets CI test history once). JUnit reports carry `<properties>`: `atalanta.termination` (`completed`, `timeout`, `interrupted:SIGTERM`), `atalanta.shard`, `atalanta.seed`; `results.json` has `termination`.
- Errors point at the suite: `tests/app.yaml:42: tap { text: "Pay" }: Element not found ...` for parse errors, validation errors and step failures (a step inlined from a flow points at the flow's line). JUnit testcases carry `file` and `line`, and GitHub annotations sit on the failing step's line in the PR diff.
- A failed test keeps its trace in `atalanta-results/traces/<test>/`. The folder is the test name itself only when that is lower-case ASCII letters, digits, `-` and `_`; any other name (a capital, a space, punctuation, a non-ASCII letter) gets a cleaned name plus an 8-hex hash (`Check Out` → `Check_Out-1a2b3c4d`), so `a b` and `a_b`, or `Login` and `login` on a case-insensitive disk, never share a folder. The folder holds each step's action, selector, matched bounds, settle reads and time, heals and warnings, and the screen trees it read (stored once per content hash). In `report.html`, clicking a step shows its screen with the matched (or nearest) element boxed, next to the tree. `--trace always` keeps passing tests' traces too; `--trace off` records nothing. The MCP tool `describe_failure` gives an agent the failing step, its selector and line, and the closest elements on the last screen.
- History flags tests whose flip-or-retry rate is ≥ 15% over 5+ runs and clears them after 10 straight passes; runs where most tests fail are treated as outages and not recorded. Outcomes are also kept per device model and OS major version, so the flaky list says `flaky only on iPhone SE / iOS 17` when that is where it flakes. `--last-failed` re-runs last run's failures; `--detect-new-flakes` re-runs tests new to the history up to 10 times after they pass.
- `--flaky-confidence 0.99` (with `--parallel`) runs copies of tests history flags as flaky on different devices, n = ⌈ln(1 − 0.99) / ln(p_fail)⌉ of them; the test passes if any copy does, and is reported as flaky when one failed.
- Self-healing has three modes, `healing.mode: auto | report | off`. `auto` (the default locally) uses the healed element and passes; `report` (the default when CI is detected) works out the heal but fails the step with the proposed patch and the exact `atalanta heal --accept --config <suite>` command; `off` doesn't heal. A healed test shows as `PASSED (healed n)` on the console and in every report (a JUnit `atalanta.healed` property, HTML, JSON `healed`/`label`, GitHub annotations and summary, an Allure tag).
- Healed selectors from passing tests (and, in `report` mode, the proposed ones) are queued in `.atalanta/healed-selectors.json`; `atalanta heal -c suite.yaml` shows them in your own selector form, `--accept` writes them into the suite, `--discard` drops them, and `--reject <N|selector|all>` drops one (by its number in the listing, or its original selector such as `'text: "Log in"'`) for good: the pair goes into `.atalanta/heal-rejected.json` in the current directory, where runs read it, and is never proposed again for that app (the entry records the app id the heal was made for; another app with the same selector still heals, and an entry from an older Atalanta without one applies to every app); the other heals stay pending. CI is detected from `CI`, `GITHUB_ACTIONS`, `JENKINS_URL`, `TF_BUILD`, `GITLAB_CI`, `BUILDKITE`, `CIRCLECI` and the like; a value of `false`, `0` or empty doesn't count.
- **Memory check** (Android only; host tests only, the profiler has never run on a device: [STATUS.md](STATUS.md)): with `config: { performance: { enabled: true } }` Atalanta samples the app's memory (`dumpsys meminfo`, every `intervalMs`) and fails a test whose peak total PSS is more than `pssGrowthThresholdPct` (default 15) above its saved baseline, with `memory regression: …`. Only a clean pass updates the baseline, so the run after a breach fails too until memory comes back down. The other drivers report no memory.
- `atalanta lint` flags another engine's selector syntax (Playwright's `>>`, `text=` and `:has-text()`, jQuery's `:contains()`): an error in `css:`, which the browser would reject, a warning in a text or id value.
- `atalanta lint` flags fixed `wait:` steps; `atalanta migrate waits suite.yaml --write` removes the ones before a step that already waits for its element.

## Migrate from Maestro

Already using Maestro? Convert your flows and see what didn't carry over. Simple flows convert as they are. Flows that rely on JavaScript (`runScript`, `evalScript`, `${…}` expressions) or `copyTextFrom`/`pasteText` need rewriting by hand: Atalanta has no equivalent yet, and the converter says what to use instead (an `http` or `exec` step's `capture:`). `runFlow when: {visible: …}` / `notVisible:` becomes a flow that checks the condition first and runs as an optional block; other `when:` conditions run as an optional block with a warning. `setLocation` becomes an optional `exec` step per platform (`adb emu geo fix`, `xcrun simctl location`). So do `setPermissions` (`pm grant`/`revoke`, `simctl privacy`), `addMedia` (`adb push` and a media scan, `simctl addmedia`), and the single-platform `clearKeychain` (`simctl keychain reset`), `setAirplaneMode` and `setOrientation` (Android `adb shell` only); `adb` and `xcrun` go into `config.exec.allow`. A flow's header `env:` becomes defaults (`${NAME}` → `${NAME:-value}`, which `--env` still overrides). Run the dry run first:

```bash
atalanta migrate maestro ./maestro-flows/ --dry-run   # what would not convert, nothing written
atalanta migrate maestro ./maestro-flows/
```

`--dry-run` converts every flow without writing and ranks the Maestro commands that don't convert cleanly by how many flows they affect, so you know before switching what you'd rewrite (`--report-json` for the same as JSON). The full run then converts your existing Maestro YAML flows to Atalanta format automatically. `openLink:` opens deep links on every platform. A step's `optional: true` stays `optional: true` (a failure becomes a warning), and `assertWithAI` becomes a natural-language `assert:` with `optional: true` (unless the flow says `optional: false`); it is checked against the element tree rather than a screenshot, so it needs a model key (`ANTHROPIC_API_KEY` or another provider's). `killApp` becomes `stopApp`, and a counted `repeat` is unrolled up to 100 times. A `while:` loop, `runScript` and `evalScript` are skipped with a note; a script that made HTTP calls becomes an [`http` step](DSL.md#http-and-api-tests) with `capture:`. Variables a flow read from a `.env` file come from `atalanta run --env-file .env`. `eraseText` becomes one `pressKey: backspace` per character (50 by default, up to 100), and a `tapOn` point is converted only when it is in absolute pixels and has no selector (a point with a selector is inside that element).

## Command cheat sheet

```text
atalanta doctor [--fix] [--json]             Is this machine ready? (tools, devices, runner caches, keys, suites)
atalanta init [--platform P --app-id ID --yes --run]   A first test from the running app, a CI example
atalanta run <suite.yaml>                    Run a suite (first free booted device)
atalanta run <suite> --device <id>           On a given device
atalanta run <suite> --parallel N            Across up to N booted devices
atalanta run <suite> --shard M/N             One CI shard (1-based; --shard-index/--shard-total: 0-based)
atalanta run <suite> --shard M/N --shard-timings F   Balanced by durations (history JSON or JUnit XML)
atalanta shard-plan <suite> --total N        Print every shard's tests and the plan fingerprint
atalanta run <suite> --history-from 'history/*'   Merge history other CI jobs uploaded (docs/CI.md)
atalanta history merge <files…> [-o dir|file.json]   Merge CI jobs' history files into one
atalanta --color auto|always|never ...       Colour (also NO_COLOR, CLICOLOR, CLICOLOR_FORCE)
atalanta run <suite> --fail-on-flaky         A flaky pass (passed on retry) exits 1
atalanta run <suite> --junit-classname suite JUnit classname = suite file stem
atalanta run <suite> --retries N             Retry failures (flaky = passed on retry)
atalanta run <suite> --report junit,html     Reports: junit, json, html, github, allure
atalanta run <suite> --report allure --clean-allure   Start allure-results afresh (results are kept by default)
atalanta run <suite> --failed-first | --last-failed
atalanta run <suite> --detect-new-flakes     Extra runs for tests new to the history (not with --parallel yet: exit 2)
atalanta run <suite> --changed-since main    Only tests a change can affect (+ risky and unmapped ones)
atalanta run <suite> --changed-since main --fail-if-none   Exit 2 when no test is affected
atalanta run <suite> --shuffle [--seed S]    Shuffled order, to expose order-dependent tests
atalanta run <suite> --hunt 5 [--stress]     Flaky hunting: 5 perturbed rounds, report flips
atalanta run <suite> --hunt 5 --parallel 3   The rounds' copies across 3 devices (device effects shown)
atalanta run <suite> --trace always          Keep every test's trace (default: failed tests only)
atalanta run <suite> --profile trace.json    Chrome/Perfetto trace of the run
atalanta run <suite> --profile-steps         Where the time went, per action (settle share, slowest step)
atalanta run <suite> --set android.scroll=semantic --set sync.quietMs=100   Override settings / config for one run
atalanta settings [filter]                   Every named setting, its default and values
atalanta run <suite> --update-baselines      Rewrite missing and changed assertScreenshot baselines
atalanta run <suite> --update-baselines=missing   Write only missing baselines (a missing one fails otherwise)
atalanta run <suite> --cache-results         Skip tests that passed with nothing changed
atalanta run <suite> --daemon                Run through `atalanta daemon` (warm devices)
atalanta run <suite> --app build/app.apk     Install the build first (skipped if already on the device)
atalanta run <suite> --parallel N --provision   Boot what's missing for N devices, then run
atalanta devices up --android N --ios N [--avd X] [--device-type "iPhone 17 Pro"] [--app path]
atalanta devices down [--delete] [--all]     Stop what `up` started for this owner (--delete: remove its clones; --all: every owner's)
atalanta daemon start | stop | status        Keep drivers and sessions warm between runs
atalanta repl --device <id> [--app <id>]     Run steps one at a time (:tree, :find, :shot, :save)
atalanta explore "<goal>" --app-id <id>      An AI agent reaches the goal and writes a deterministic test (docs/EXPLORE.md)
atalanta explore "<goal>" --app-id <id> --data EMAIL=a@b.c --secret PASSWORD --replay 3
atalanta schema [-o atalanta.schema.json]    JSON Schema for suite YAML (editor completion)
atalanta schema --markdown                   The DSL reference (docs/DSL-REFERENCE.md)
atalanta heal -c <suite> [--accept|--discard|--reject <N>]   Review healed selectors
atalanta baselines review [--serve [--baselines-dir DIR]]  Failed assertScreenshot comparisons (--serve: review page)
atalanta baselines accept <name|all> [--baselines-dir DIR]   Make the actual screen the baseline
atalanta baselines reject <name|all>         Keep the baseline, drop it from the list
atalanta lint -c <suite>                     Fragile selectors and fixed waits
atalanta migrate maestro <dir>               Convert Maestro flows to Atalanta
atalanta migrate waits <suite> [--write]     Remove redundant wait: steps
atalanta nl-eval -k 5                        Planner consistency (pass^k) on saved NL traces
atalanta nl-eval --model anthropic:claude-haiku-5-5 --model openai:gpt-6-luna --recommend
                                             Compare models on the built-in task set
atalanta device list | boot <id> | shutdown <id>
atalanta trace view [atalanta-results]       Open a failed test's trace.html (or an index of all)
atalanta inspect [--open]                    Console: live mirror, inspector, run and explore panels, tasks
atalanta mcp                                 Start the MCP server
atalanta mcp install [client] [--dry-run]    Register it with agent clients, with the agent rules
atalanta validate <suite>                    Validate a suite
```

## Configuration

```yaml
appId: com.example.app
config:
  platform: android          # ios | android | web
  app:                       # installed before the first test, by hash (or one path for both)
    android: build/app.apk
    ios: build/App.app
  sync:
    quietMs: 150             # settled after this long unchanged
    reactionMs: 300          # an action that changes nothing is done after this
    settleCapMs: 3000        # then warn and continue
    timeoutMs: 10000         # element waits
    maxTimerMs: 1500         # in-app probe: one-shot timers / delayed work due later than this don't count
  retry:
    count: 1
  actionability: strict      # strict (default) | lenient: act on a disabled or moving target with a warning
  healing:
    mode: report             # auto (default locally) | report (default in CI: fail with the patch) | off
  artifacts:
    on_failure: true
    output_dir: ./atalanta-results
    screenshots: onFailure   # screenshot: steps — always | onFailure (written only if the test fails) | never
    stepScreenshots: true    # the screen after every step, written only for a failing test
  settings:                  # named driver settings (`atalanta settings`); `--set` overrides
    android: { scroll: semantic }
    ios: { text: auto }      # auto | paste | keys
  web:                       # platform: web
    device: "Pixel 7"        # optional mobile-web profile (see below)
    settle: { reactionMs: 30, capMs: 1000 }   # web-only settle timings; win over sync.* here
    mocks:
      - { url: "*/api/users", json: [{ name: "Ada" }] }
    block: ["*google-analytics*"]
tests:
  - name: "Log in"
    steps:
      - launchApp: { clearState: true }
      - tap: { text: "Log in" }
        timeoutMs: 5000
      - tap: { id: "counter" }
        settle: none         # act and move on (or { quietMs: 50, capMs: 500 } for this step)
      - tap: { text: "Not now" }
        optional: true       # a failure is a warning in the report; the test goes on
      # `runFlow: x` with `optional: true` stops the flow at its first failing step (one
      # warning), skips the rest of that flow and goes on after it (Maestro's optional subflow)
      - setConfig: { sync.timeoutMs: 20000, android.text: fast }   # rest of this test only
      - inputText: { selector: { id: "user" }, text: "${TEST_USER:-ada}" }   # ${VAR}, ${VAR:-default}, ${VAR:?error}; $${ is a literal ${, $$${PRICE} is $ then the value
      # web only (validate refuses it on android/ios); dropped at test end:
      # - mockRequest: { url: "/api/save", method: POST, status: 500, json: { error: down } }
```

Web: the settle after an action waits for the page to stop changing (DOM mutations and finite animations, with `web.settle: { reactionMs, quietMs, capMs }`, else `sync.reactionMs`/`quietMs`/`settleCapMs`; a timing set nowhere gets the web default, a 30 ms reaction window and a 1 s cap, so an action that changes nothing costs about nothing; a value you set, in the suite or a step's `settle:`, is used as written even when it equals the device default; a node a script changes every frame, such as a ticking counter or a JS-driven animation, is ignored; `sync.mode: none` skips the settle and restores the old network-only speed), then for the page's requests (`web.networkIdle`, on; websockets, event streams and polls older than `web.networkIdleMaxMs` don't count; `web.networkIgnore` lists more). `mockRequest` takes `url` (a glob with `*`, else a substring; `?` is the query string), `method`, `status`, one of `json`/`body`/`file`, `headers`, `delayMs`, `block` and `times`; `clearMocks` drops the test's mocks.

Keys: `pressKey` takes `back`, `home`, `enter`, `volumeUp`, `volumeDown`, `backspace`, `tab`, `escape`, `delete` (forward delete), `arrowUp`, `arrowDown`, `arrowLeft` and `arrowRight`. `back` (and `pressBack`) is navigation on every platform: Android's back button, iOS's navigation-bar back, and history back on the web; it is never "delete a character" (use `backspace`). Android sends every key as a keyevent; iOS types `backspace` and `tab` through WebDriverAgent's keyboard but has no `escape`, `delete` or arrow keys, and a browser has no volume buttons: a key the platform can't send is a config error at validation (exit 2), not a silent pass. `atalanta migrate` flags a Maestro `back` straight after `inputText` (there it usually closed the keyboard), and `validate`/`run` warn on a web suite's `back` straight after `inputText`. The headless driver has no keyboard: `back`, `home`, `enter` and the volume keys do nothing there, and the editing keys (`backspace`, `tab`, `escape`, `delete`, the arrows) are a config error. Over MCP, `press_key escape` on iOS presses back, with a note in the result.

Web `<select>`: `inputText` on a `<select>` (or on its `<label>`) chooses the option whose value is the text, else whose visible label is (whitespace normalised), and fires `input` and `change`. A disabled option, an option in a disabled `<optgroup>` or a disabled select is refused (`not actionable`); a text no option has fails with the list of options.

Mobile web: `web.device` emulates a phone or tablet in Chrome with a Chromium device descriptor: its viewport, device pixel ratio, user agent, mobile layout (the meta viewport applies) and touch (`(pointer: coarse)`, touch events; taps, double taps and long presses are sent as touches, and `swipe` is a finger stroke of `touchmove`s that comes to rest before it lifts, so it scrolls by its length and drags touch carousels). Every name in Playwright's device list works (about 200: `iPhone 15`, `iPhone 17 Pro Max`, `Pixel 9 Pro XL`, `Galaxy S24`, `iPad Pro 11`, `Desktop Chrome`, …, and each as `<name> landscape`; the list ships as data under Apache-2.0, see `NOTICE`); `iPad` is kept as an alias of `iPad (gen 7)`. Names match ignoring case and spaces; an unknown name is a config error that names the nearest ones. `web.width`/`web.height` override the profile's viewport. This is the responsive layout in Chromium, not Safari. Without a profile pages are desktop pages, with no touch.

Web swipes: `swipe: up` is the finger moving up, as on Android and iOS, so the page scrolls down and shows what is below (the mouse wheel at the viewport centre, so the scroller under it moves). Before, a web `swipe: up` scrolled the page up: web suites written against that flip their directions; `validate` and `run` print a note on each web `swipe: <direction>`, and on a web `swipe` with no direction (down), for one release. Maestro flows migrate unchanged, since Maestro's directions are the finger's too.

In-app sync probe: after its `hello`, Atalanta sends the probe the suite's `web.networkIgnore` globs (as regexes: a pattern with `*` covers the whole URL, any other is a substring; case-sensitive, as on the web; runs of `*` count as one, and the regex can't backtrack badly on a long URL), so requests to those URLs never keep a mobile app busy either, and `sync.maxTimerMs` when set. The ignore list joins the probe's own list for the app's lifetime; `maxTimerMs` holds for this run's connection only. What counts as a timer: on iOS one-shot `NSTimer`s scheduled with `+scheduledTimer...` and `performSelector:withObject:afterDelay:` (cancelled ones drop out), plus work posted with `AtalantaSyncProbe.asyncAfter`; plain `dispatch_after` / `DispatchQueue.asyncAfter` can't be observed with public API and is not tracked. On Android (probe protocol 4) delayed main-looper messages (`Handler.postDelayed` / `postAtTime`), read by walking the main `MessageQueue` (else parsed from `Looper.dump`, else reported as `unknown`), except barriers and the cursor blink and other framework re-posters (matched on the callback class; `View.postDelayed` through the view root's handler counts). A callback instance re-posted as it fires two times in a row (a poller; a coroutine `delay()` loop is matched by class and period, three times over 2 s) stops counting; a debounce never does. A timer counts only when it was first seen due within the horizon.

Perpetual work never holds an app busy for good: a self-rescheduling `performSelector` (iOS), a main-looper poller, a JS timer re-armed as it fires (`setTimeout` polling, `requestAnimationFrame` loops), a looping native-driver animation (after 2 s) and an RN `fetch` open over 10 s (a stream) stop counting and are listed in the probe's `pending` as `ignored: perpetual <tracker>` (idle replies included). One-shot work keeps counting. Trackers can be opted out per app with the probe's `config` `ignore_trackers` (e.g. `["rn_animation"]`; process-wide) or in-app with `AtalantaSyncProbe.ignoreTrackers(...)`; Atalanta itself doesn't send it yet. `maxTimerMs` goes only to a probe of protocol 3 or later (its `hello` says so); an older probe gets none, with a warning to update it.

React Native: `framework: react_native` (`--framework rn`, or detected from the app) runs on a device or simulator, like a native app: RN's `testID` is the Android resource-id and the iOS accessibilityIdentifier, so `id:` selectors work, and the in-app sync probe settles as for a native app. There is no headless React Native renderer: `driver: headless` with React Native is a configuration error (exit 2), and a leftover `reactNative:` block is ignored with a warning. With `framework: react_native` Atalanta also sends `framework` in the probe's `config` (protocol 4), and the probe tracks React Native's own work by reflection / guarded swizzling, with no build dependency on RN: on Android JS timers (`JavaTimerManager.timers`, else `hasActiveTimersInRange`, also its Kotlin-internal mangled name, so bridgeless RN 0.76+ works; horizon `maxTimerMs`), Fabric's `MountItemDispatcher` queue (old architecture: `UIViewOperationQueue`), native-driver animations (`mNodesManager`, RN 0.81 `nodesManagerRef`) and RN `fetch` calls on its OkHttp client, plus `rn_loading` while the React context isn't up (at most 30 s from when it went missing, so reloads are waited for); the probe only reads RN objects that exist (it never creates the `ReactHost` or a native module); on iOS JS timers (`RCTTiming`, hooked when the probe starts, so timers made before `config` count) and Fabric transactions scheduled but not yet mounted (`RCTMountingManager`). A hook that doesn't fit the app's RN version is reported as `unknown` with one warning, never as idle; an older probe gets a warning to update it. Verified on the host against fake classes with RN's names only, not on a device yet. Android apps report their own OkHttp requests with `AtalantaOkHttpInterceptor` (in the probe; OkHttp is `compileOnly`): a request counts until its response body is read to the end, closed or fails, not just until the headers arrive.

Measurement scripts for real devices (Compose testTag cost, strict-actionability cost, native settle times, demo-mode verification): `scripts/measure/` (see its README).

Editors: `atalanta schema -o atalanta.schema.json`, then `# yaml-language-server: $schema=./atalanta.schema.json` as a suite's first line.

Environment: `ATALANTA_ADB_SERVERS` (remote adb servers `host:port,…`; their devices join as `host:port/serial`), `ATALANTA_IOS_RUNNER=atalanta`, `ATALANTA_ANDROID_EVENT_SETTLE=1` (settle from accessibility events; measured slower than polling on an emulator, lighter on the device), `ATALANTA_COMMAND_TIMEOUT_MS` (device command deadline, default 60000), `ATALANTA_KEEP_WDA`, `ATALANTA_KEEP_ANIMATIONS`, `ATALANTA_IOS_TYPING_FREQUENCY` (keys/min, default 180), `ATALANTA_WDA_REF`, `ATALANTA_WDA_URL` (use your own WDA), `ATALANTA_NL_CACHE` (`read-only`, `frozen` or `prune` for natural-language steps in CI), `ATALANTA_MIN_FREE_MEMORY_PCT` / `ATALANTA_MIN_FREE_MEMORY_GB` / `ATALANTA_MIN_FREE_DISK_GB` (provisioning guards, default 35 / 3 / 3), `ATALANTA_AVD`, `ATALANTA_DEVICE_TYPE` and `ATALANTA_EMULATOR_PORT` (what `run --provision` boots).

Element state: every driver that knows it reports `focused`, `selected`, `obscured` (a password field), `checked` (checkboxes, switches, radios) and `visible_bounds` (the on-screen part) on each element, in traces and MCP output alike. `focused` and `selected` are left out of an element's JSON when the driver can't tell (headless), and written as `true` or `false` when it can.

Cross-origin iframes (a Stripe or Adyen card field, an OAuth button, an embedded checkout) are part of the page on web: Atalanta attaches to each out-of-process frame over its own DevTools session (`Target.setAutoAttach`, flat sessions), runs its in-page library there, and a selector that finds nothing on the page itself (or only hidden matches) looks in those frames. Found elements have their boxes in the page's coordinates, so `tapOn`, `inputText`, `assertVisible` and `assertText` work on them, and the frames' requests count for the network-idle wait. Not covered: the element tree (`get_hierarchy`, layout relations like `below:`) stays the page's own, and `web.mocks`/`mockRequest` don't apply to requests made inside such frames. `web.frames: 0` (`ATALANTA_WEB_FRAMES=0`) leaves them opaque, as before.

A tap on an element in such a frame is checked on the page too: when something on the top page covers the frame at the click point (a cookie banner, a sticky footer) the step fails "obscured by <div>", as it does for an element of the page, and a click the frame never received is retried and then fails. Lookups that fall back to the frames take at most about 2 s for all of them together. A frame still loading is waited for once, then skipped until its document is ready; one slow answer (a long task) is not a hang: a frame is left out of lookups, with one warning, only after 3 lookups in a row it didn't answer, and then for 3 s at a time, after which a ping decides whether it is asked again. While any frame is skipped, a lookup that finds nothing is unknown, not "not found": the error names the skipped frames, and `assertNotVisible` keeps waiting until its timeout instead of passing. A tap on an `<iframe>` inside a cross-origin frame works (its click arrives in the inner frame, which the outer one can't see). A cross-origin frame removed while its document is still loading no longer keeps the network-idle wait busy. Also not covered: an `<iframe>` rotated or scaled with a CSS transform (its elements' boxes are placed by its top-left corner only).

Behaviour changes with the cross-origin frames on, any of which can turn a suite red **[FAILS]**:
- A selector that matches only hidden elements on the page now resolves to a visible match in a cross-origin frame: `assertNotVisible: "Submit"` with a hidden "Submit" on the page and a visible one in an embedded frame used to pass and now fails.
- Lookups of every element (`findAll`) include frame matches: screenshot `mask:` selectors also mask matching elements inside cross-origin frames (a baseline taken before now diffs), the strict-selector ambiguity list can name frame matches, and an `index:` past the page's own count resolves into a frame instead of failing.
- Negative lookups (`assertNotVisible`, `waitUntilNotVisible`, `extendedWaitUntil` waiting for an element) on pages with cross-origin frames make a round trip per frame per poll (bounded as above), so they are slower.
- A tap on a frame element covered by something on the page now fails instead of passing (the click went to the cover).
- With `web.parallelMode: contexts` (the default) one Chrome process is one failure domain: a browser crash or hang takes all N parallel devices down at once (a hung one is detected by a browser-level ping over its own DevTools connection, so another device's slow context or page creation doesn't count; after 2 failed pings of 2 s it is killed and replaced on the next restart).

The browser's own `/favicon.ico` request (type other, started by the browser, at the top page's origin root) is not waited for by the network-idle wait; a page's own request for some `favicon.ico` (an `<img>`, a `fetch`) is.

Typing: `inputText` fails when its tap left the focus on another element, so keys aimed at a label never go into the previous field. On web it fails whenever the focus is not on something that takes text (an `<input>`, `<textarea>` or contenteditable), looking through shadow roots and same-origin frames; focus inside a cross-origin iframe (a Stripe, Braintree or Adyen card field) can't be read, so it passes, unless that frame already had the focus before the tap (the tap didn't move it: the text would go into the card field) and isn't the target's own (the target, a card widget's container, holds that `<iframe>`, or the tap landed in it: typing is allowed then). A field inside an attached cross-origin frame has its focus checked in that frame, which must hold the focus itself; and `web.focusCheck: off` (`ATALANTA_WEB_FOCUS_CHECK=off`) turns the check off. On Android and iOS it fails only when the device reports focus elsewhere (the Android tree server always does; AtalantaRunner reports it): on something that takes no text, on the field that already had the focus before the tap, or on a different, identified field that was already on screen. A field that keeps the focus but moves (a collapsing header) is still the field that had it. On Android a `TextView` takes no text (it does on iOS). A field that moves when it takes focus (a search bar sliding into the navigation bar, a form scrolling the field above the keyboard, a fake search bar opening a screen with the real field) passes; `android.focusCheck: 0` (`ATALANTA_ANDROID_FOCUS_CHECK=0`) and `ATALANTA_IOS_FOCUS_CHECK=off` turn it off. Without the tree server, the `adb shell input text` fallback refuses non-ASCII text, a password field's text and secrets.

Compose: `id:` also matches a Compose `testTag` when the app doesn't set `testTagsAsResourceId` (read through the accessibility extra data, API 26+); `android.composeTestTag: 0` (`ATALANTA_ANDROID_COMPOSE_TESTTAG=0`) turns it off. Cost: each tag read is one synchronous call the app's main thread serves, so only Compose nodes without a resource-id are read, at most 50 ms of reads (and 200 nodes) per full dump, and each node's tag is cached for 2 s across dumps (a tag the app changes is seen within 2 s); on a screen with many tagged nodes the first dump pays up to 50 ms and later ones about nothing, and tags past the budget fill in over the next polls.

iOS taps: `ATALANTA_IOS_TAP_GUARD=off` taps the centre of the frame as before the tap guard; `ios.tapGuardStatusBar: 0` (`ATALANTA_IOS_TAP_GUARD_STATUS_BAR=off`) drops only the status-bar rule, for apps that hide the status bar.

iOS tap check (opt-in): `ios.verifyTap: true` (`ATALANTA_IOS_VERIFY_TAP=1`) asks AtalantaRunner (`ios.runner: atalanta`) what is drawn at each tap point before tapping, through XCTest's private hit test (`_XCT_requestElementAtPoint:reply:`, every private selector checked with `respondsToSelector:` first) The element XCTest's hit test returns is found in the app's snapshot (by its accessibility element) and sent with its snapshot path, so the host checks it is the target or inside it by tree position, not by frame; only when XCTest's element is a container of the target (or can't be placed) does the deepest node at the point decide, skipping only nodes the snapshot reports with user interaction off (not being an accessibility element or not "hittable" to XCTest does not count: a plain `UIView` dimming overlay is both and still swallows the touch). Each verified tap costs one snapshot of the app. A tap that would land on something else (an overlay, a system alert, the status bar) fails naming it; a row under the status bar is tapped when the hit test says the app draws there (an app that hides its bar). Where the hit test is missing (WebDriverAgent, an iOS without the private API) the frame-only rule stands and a warning says so once. Typechecked against the iOS 26 simulator SDK; not yet run on a device or simulator.

WebDriverAgent session settings: after each session starts Atalanta sends `accessibilityDeadline` (`ios.accessibilityDeadlineMs`, sent in seconds; 0 leaves WDA's default). Past the deadline WDA aborts the request with an error rather than waiting, so an app whose main thread is busy longer than that fails its source and find reads instead of waiting them out. The 5000 ms default applies only to a WebDriverAgent Atalanta built itself from v16.14.1 or later (recorded per device, also when a kept runner is reattached with `ATALANTA_KEEP_WDA`); older WDA crashed WKWebView content with it (#1290), so an older or externally started WDA gets no deadline unless you set one. Atalanta also sends `snapshotMaxDepth` (`ios.snapshotMaxDepth`, default 60; AtalantaRunner uses the same depth for its deep-tree recovery read) through `POST /session/:id/appium/settings`.

### Other environment variables

Most settings are in the table `atalanta settings` prints (each with its `ATALANTA_*` variable; [DSL-REFERENCE.md](DSL-REFERENCE.md)). These are read directly:

| Variable | Default | What it does |
|---|---|---|
| `ATALANTA_ADB_HOST`, `ATALANTA_ADB_PORT` | `127.0.0.1`, `5037` | The adb server Atalanta talks to |
| `ATALANTA_EMULATOR_PATH` | the SDK's `emulator`, else `emulator` on `PATH` | The emulator binary `atalanta devices up` starts |
| `ATALANTA_EMULATOR_COLD_BOOT` | off | `1` cold-boots emulators instead of loading the AVD's snapshot (the snapshot is never saved over either way) |
| `ATALANTA_XCRUN_PATH` | `xcrun` | The `xcrun` used for `simctl` and `devicectl` |
| `ATALANTA_IOS_STOP_TIMEOUT_MS` | `10000` | How long stopping the iOS runner app may take |
| `ATALANTA_USBMUX_TIMEOUT_MS` | `5000` | Deadline of each request to usbmuxd (the USB forward to a cabled iPhone) |
| `ATALANTA_SCREENSHOT_STABLE_MS` | `5000` | How long `assertScreenshot` waits for the screen to hold still when the step sets no `timeoutMs` |
| `ATALANTA_WEB_FLUTTER_SEMANTICS` | on | `off` stops Atalanta turning on a Flutter web app's semantics tree |
| `ATALANTA_WEB_SW` | service workers bypassed | `allow` keeps a page's service worker in the request path (mocks may then be bypassed) |
| `ATALANTA_FLUTTER_HIT_VISIBILITY` | off | `1`: in the headless Flutter bridge, `assertVisible` also needs a tap at the element's centre to reach it, so a widget under an overlay is not visible (the bridge config's `hitTestVisibility` wins) |
| `ATALANTA_STATE_DIR` | `~/.atalanta/state` | Where `saveState` keeps app state |
| `ATALANTA_INSTALLS_DIR` | `~/.atalanta/installs` | Where the markers of hash-checked app installs live |
| `ATALANTA_DAEMON_SOCKET` | `~/.atalanta/daemon.sock` | The socket of `atalanta daemon` |
| `ATALANTA_EXPLORE_SHOTS_DIR` | unset | `atalanta explore` writes each screenshot the model saw there as `turn-NN.png` (for an outside judge, such as a benchmark harness) |

## Troubleshooting

The first-run failures people hit, with what to do next. `atalanta doctor` is the first step for anything about the machine or a device: it names each fix as a command, and `--fix` applies the safe ones.

| You see | Do this |
|---|---|
| `Failed to execute adb` | adb is not installed or not on `PATH`. `atalanta doctor -p android` says where the SDK is and prints the `export PATH=…` line; `ATALANTA_ADB_PATH` points at another adb. |
| `ADB connection lost`, or `Circuit breaker is open for device …` (the driver stopped answering, so Atalanta stopped sending it commands) | `atalanta doctor --fix` restarts a wedged adb server and reconnects offline devices. Then run again. |
| `WDA … failed to build with Xcode …` | Usually not the WebDriverAgent tag: an unaccepted Xcode licence, `xcode-select` pointing at the command line tools only, or no simulator runtime. `atalanta doctor -p ios` checks all three and prints the fix. |
| `Failed to clone WebDriverAgent` (a CI runner without access to GitHub) | Point `ATALANTA_WDA_URL` at a WebDriverAgent you run yourself, or let the runner reach github.com once so the build is cached. |
| The first iOS run sits for a minute in `xcodebuild` | Expected once per Xcode version: Atalanta builds WebDriverAgent on the first iOS run, then reuses the build. |
| `Element not found: … after …ms` | `atalanta trace view atalanta-results` shows the screen at the failure with the nearest elements; `atalanta inspect --open` lets you hover the element and copy its selector. |
| `Element … is not actionable: …` | The target never held still, stayed disabled or was covered. Wait for it first (`assertVisible`), or put `force: true` on the step to act anyway; `config.actionability: lenient` makes the whole suite act with a warning. |
| `Sync timeout: UI did not stabilize …` (the screen kept changing: a spinner, an animation, a ticking clock) | `settle: none` on that step, or a larger `config.sync.settleCapMs`. |
| `Missing required environment variable(s): X` | `--env X=value`, `--env-file .env`, or export it in the shell. |
| A test passes locally and fails in CI on the first step | The device is not booted yet, or another job holds it: `atalanta devices up --android 1` in the job, and see the exit codes in [CI.md](CI.md) (3 is a device problem; retry it). |
| `atalantahq/atalanta@v0.2.0` not found in a workflow `atalanta init --ci github` wrote | Atalanta 0.2.0 has not been released yet, so the Action tag does not exist. Until it does, build from a checkout ([README](../README.md#install)). |

## Platform requirements

Device support is tested on the host against fake adb, WebDriverAgent, devicectl and usbmuxd. On 2026-10-25 the drivers, WebDriverAgent, AtalantaRunner, the MCP server and the inspector API were also run on an Android 14 emulator and iOS 26 simulators. Since then one real Android device has been used (a SUNMI tablet on 2026-11-08, the Settings app only). Real iPhones, the Wi-Fi runner, device crash logs, Android demo mode and Compose `testTag` reads are still **unverified on hardware**. The web driver is tested against real headless Chrome on macOS. See [docs/STATUS.md](STATUS.md).

### iOS

- macOS with Xcode installed
- Xcode Command Line Tools
- WebDriverAgent (auto-bootstrapped on first run: a pinned tag, built once per Xcode: v16.14.1 where the simulator platform ships `lib_TestingInterop.dylib`, v16.12.10 otherwise; a clone of a pinned tag must check out the commit Atalanta pins for it, or the bootstrap stops; a cached clone at another commit is cloned again, and a cached build is reused only when it records the pinned commit. `ATALANTA_WDA_REF` picks another tag, unchecked)
- **Real iPhones/iPads** (connected by USB, Developer Mode on): set `ATALANTA_IOS_TEAM_ID` to your Apple team so WebDriverAgent can be signed (`ATALANTA_WDA_BUNDLE_ID` overrides the runner's bundle id). Atalanta forwards WDA's port over usbmuxd and uses `devicectl` to install and launch. `clearState` can't wipe app data on a device; use `reset: { strategy: reinstall, appPath }`.
- **Wi-Fi-only iPhones** fail with "connect a cable": the runner would listen on the phone's Wi-Fi address. To allow it anyway set `ios.allowWifiRunner: true` with `ios.runner: atalanta`. AtalantaRunner always requires a bearer token: Atalanta makes a random one for every launch (simulators and cabled iPhones too) and sends it on every request, `/status` readiness and reattach probes included; `ios.runnerToken: ${RUNNER_TOKEN}` (16+ characters) sets a fixed one instead. A cabled iPhone's runner binds the phone's loopback, which the USB forward reaches; `ATALANTA_IOS_DEVICE_LOOPBACK=0` binds every interface (with WebDriverAgent, which has no authentication, a run then logs a warning). A runner kept running (`ATALANTA_KEEP_WDA`) is reused on a Wi-Fi-only iPhone only under the same Wi-Fi rules and only when it refuses a request without the token.
- **Crashes on a device**: when the app dies, Atalanta copies the device's crash reports with `devicectl device copy from --domain-type systemCrashLogs`, names the crash in the failure, and saves this launch's reports under `<output_dir>/crashlogs/<test>/` (simulators: from `~/Library/Logs/DiagnosticReports`). The crash sticks: from each launch a watcher asks the runner for the app's state every `crash.checkEveryMs` (default 1000 ms, at least 250); once it says "not running", every later step in the test fails at once with the crash instead of waiting out its timeout or tapping the home screen, until the next `launchApp` (`stopApp` ends the watcher). Verified against a fake runner only, not yet on a device.

### Android

- Android SDK with platform-tools
- `adb` available in PATH
- An emulator or connected device
