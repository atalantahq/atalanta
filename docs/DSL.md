# Writing suites

A suite is one YAML file: an `appId`, optional `config`, reusable `flows`, and `tests`. `atalanta validate` checks a suite without running it. For editor completion:

```bash
# first line of a suite:
# yaml-language-server: $schema=https://atalanta.sh/schema/suite.json
# or, to match your installed version: atalanta schema -o atalanta.schema.json
#   and $schema=./atalanta.schema.json
```

## A suite

The example below has one flow and two tests. The second test depends on the first.

```yaml
appId: com.example.shop
config:
  platform: android
  app: build/app-debug.apk      # installed before the first test, unless the device already has this build
  sync:
    timeoutMs: 10000            # how long a lookup or assertion waits
flows:
  - id: login
    steps:
      - tap: { id: "email" }
      - inputText: { selector: { id: "email" }, text: "${TEST_USER}" }
      - inputText: { selector: { id: "password" }, text: "${TEST_PASSWORD}" }
      - tap: { role: button, name: "Sign in" }
tests:
  - name: Log in
    tags: [smoke]
    steps:
      - launchApp: { clearState: true }
      - runFlow: login
      - assertVisible: { text: "Welcome" }
  - name: Add to cart
    dependsOn: ["Log in"]
    steps:
      - tap: { textContains: "running shoes" }
      - tap: { role: button, name: "Add to cart" }
      - assertText: { selector: { id: "cart_count" }, expected: "1" }
```

The test-level keys are:

- `dependsOn`: run the named tests first. A selection pulls them in automatically, and they stay together on one shard.
- `isolated: true`: never shares a launch with the test before it (see `batch`).
- `sideEffects: true`: never skipped by `--cache-results`.

`launchApp: { clearState: true }` wipes the app's own data: `pm clear` on Android, the app's data container on an
iOS simulator. It does not touch data the app keeps outside itself. Examples are files in shared storage (Android
`/sdcard/Documents`) and, on iOS, the system stores behind Reminders, Calendar and Contacts. On a real iPhone it
can't wipe anything. A test that creates such data should delete it, or give it a unique name (`${RUN}` from
`--env RUN=...`).

## Steps

A step is one action or assertion. It may carry modifiers that change how it waits and what a failure means:

```yaml
- tap: { text: "Pay" }
  timeoutMs: 20000       # this step's wait
  settle: none           # act and move on; or { reactionMs, quietMs, capMs }
  optional: true         # a failure is a warning; the test goes on
  force: true            # skip actionability checks (tap, doubleTap, longPress, inputText, clearText, do, act)
```

You rarely need `wait:`. Assertions and lookups wait up to the step timeout, and actions settle on their own (see [ARCHITECTURE.md#sync](ARCHITECTURE.md#sync)).

An action needs its target to be enabled and still before it acts. A target that never gets there fails with "not actionable". `force: true` on the step, or `actionability: lenient` in the config, acts anyway.

On Android, `force: true` on `tap`, `doubleTap` or `longPress` also skips the check for a window covering the target. A cover can be a keyboard, a system dialog or an overlay. The tap then touches the element's centre. Use this when the window list is wrong or stale. `inputText` and `clearText` keep the cover check even when forced, because their focus tap through a keyboard would press a key.

`assertNotVisible` fails when another app is in front, because that screen lacks the element too. `appInFront: false` allows it anyway.

## Selectors

A selector names the element a step acts on. It is a map, and every key in it must match. Use the first row that fits: a stable id is the most durable, and position and text are the most fragile.

| Want | Selector |
|---|---|
| A stable id (Android resource-id, iOS accessibilityIdentifier, RN `testID`, web `id`/`data-testid`) | `{ id: "submit" }` |
| Exact visible text or label (tried exactly, then ignoring case) | `{ text: "Sign in" }` |
| Part of the text, ignoring case | `{ textContains: "sign" }` |
| A regex over the whole text | `{ textMatches: "Total: \\$[0-9.]+" }` |
| A role and accessible name | `{ role: button, name: "Pay" }` |
| A state | `{ role: checkbox, name: "Remember me", checked: false }` |
| By position | `{ text: "Edit", below: { text: "Address" } }`, `{ id: "row", index: 2 }` |
| Inside a container | `{ text: "Delete", childOf: { id: "item_3" } }` |
| Web only | `{ css: "form > button.primary" }`, `{ xpath: "//nav//a[2]" }` |

When a selector matches several elements, Atalanta uses the first in tree order and warns. The warning suggests a `childOf:` or `index:` that would pick exactly one. `atalanta lint` flags fragile selectors.

## Variables and secrets

Use variables for anything that differs between environments or must not appear in the repository.

- `${VAR}`, `${VAR:-default}` and `${VAR:?message}` are read from `--env K=V`, then from the environment. `$${` writes a literal `${`.
- Values of variables with secret-looking names are masked in all output and reports. So are the variables listed in `ATALANTA_SECRETS`.
- An `http` step's `capture:` sets variables while the test runs (see [HTTP and API tests](#http-and-api-tests)).
- `${keychain:NAME}` reads the OS keychain, and the value is always masked.
  - It uses service `atalanta` (`ATALANTA_KEYCHAIN_SERVICE`) and account `NAME`. It reads with `security find-generic-password` on macOS and `secret-tool lookup` on Linux.
  - Store one with `security add-generic-password -s atalanta -a NAME -w` or `secret-tool store --label atalanta service atalanta account NAME`.
  - `ATALANTA_KEYCHAIN_CMD` names another program, such as a password-manager CLI or a CI secret store. It is called as `<cmd> <service> <account>` and prints the secret.
  - A command that doesn't answer within `ATALANTA_COMMAND_TIMEOUT_MS` (60 s) is killed, and the secret counts as missing.
- `${totp:NAME}` types the current 6-digit one-time code (RFC 6238: SHA-1, 30 s). `NAME` holds the base32 secret an authenticator app is set up with. It may be a variable or `keychain:NAME`. The code is worked out when its step runs, so it is fresh. The secret and the code are masked.

### Logging in

Keep the login in a suite of its own (`login.yaml`), with every credential as a `${NAME}`. Agents and other suites can then reuse it without handling the password:

```yaml
appId: com.example.app
tests:
  - name: sign in
    steps:
      - launchApp: { clearState: true }
      - inputText: { selector: { id: email }, text: "${LOGIN_EMAIL}" }
      - inputText: { selector: { id: password }, text: "${keychain:LOGIN_PASSWORD}" }
      - inputText: { selector: { id: code }, text: "${totp:LOGIN_TOTP}" }
      - tap: { id: sign_in }
      - assertVisible: { text: Home }
```

- **`atalanta login login.yaml`** runs it (the first test, or `--test NAME`) once, with no retry and no result cache, and leaves the app logged in. An agent can call it from a shell without ever seeing the password.
- **`atalanta explore "<goal>" --after login.yaml`** replays it with no model before the first planning turn. The test explore writes starts with the same steps, by name. Over MCP, `run {goal, app_id, after: "login.yaml"}` does the same.

## Launch arguments

`launchApp` can pass arguments to the app. Use them to point a test build at a staging server or to turn on a test
login. The form is the same as Maestro's:

```yaml
- launchApp:
    clearState: true
    arguments:
      isE2E: true
      server: "https://staging.example.com"
      retries: 3
```

Values are text, `true`/`false` or numbers (quote a number to keep it text: `pin: "0042"`), and `${VAR}`s are filled in
as elsewhere. Names use `[A-Za-z0-9_.-]` and can't start with `-`.

- **Android:** intent extras on the launch (`--es` text, `--ez` true/false, `--ei` or `--el` whole numbers, `--ef`
  decimals), read with `intent.getStringExtra("server")` and friends.
- **iOS:** launch arguments `-server https://staging.example.com`, read with
  `UserDefaults.standard.string(forKey: "server")`, as Xcode scheme arguments are.
- **Web and headless:** a validation error.

On Android a launch whose launcher activity can't be resolved (it would fall back to `monkey`, which can't carry
extras) is a configuration error rather than a launch without the arguments.

## Flows

A flow is a named group of steps that several tests share, such as a login. `runFlow: <id>` inlines a flow's steps. Flows can call flows, but not in a cycle.

### Parameters

`runFlow: { flow: <id>, env: { <NAME>: <value> } }` passes parameters to one call of a flow.

- Inside the flow and the flows it runs, `${NAME}` takes the call's value. Nowhere else does, so a test's own `${USER}` still comes from the environment.
- A nested call passes its caller's parameters on unless it sets its own.
- A value can use the caller's parameters (`env: { GREETING: "hi ${USER}" }`).

```yaml
flows:
  - id: login
    steps:
      - inputText: { selector: { id: email }, text: "${USER}" }
      - inputText: { selector: { id: password }, text: "${PASSWORD}" }
tests:
  - name: admin logs in
    steps:
      - runFlow: { flow: login, env: { USER: admin@example.com, PASSWORD: "${ADMIN_PASSWORD}" } }
```

Parameters are filled in when the suite loads, so each of the flow's steps is still its own step in reports and traces, with its own timeout.

- A parameter the call doesn't pass comes from the environment, as any `${NAME}` does. With no value there either, it is an error, not a literal `${NAME}` typed into the app.
- A name an `http` step captures is left for the run, unless the call passes it.
- A parameter with a secret-like name (`PASSWORD`, `TOKEN`, …) is masked in reports like any secret.
- MCP's `run_flow` takes the same parameters as `env`.

## Natural-language steps

A natural-language step states an intent in plain words, and a model finds the element to act on. Everything else on this page works with no model.

```yaml
tests:
  - name: Checkout
    steps:
      - do: "add the first product to the cart"
      - act: "type the coupon code"
        data: "${COUPON}"
      - assert: "the cart shows one item"
      - "open the cart"                       # a bare string is a `do:`
```

These need a model key: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, or the key for any provider in [AI-PROVIDERS.md](AI-PROVIDERS.md) (`config.ai: { provider: gemini }`). The model only chooses an operation and an element from a numbered list. Atalanta builds and checks the selector itself.

- **Typed values** come from `data:` or from quotes in the step. They are never invented.
- **`waitUntil:`** names a condition to wait for after the action.
- **Caching.** Plans are cached in `.atalanta/nl-cache.json` once the run has verified them, so a stable screen replays without calling the model. Test data is never part of the key. The same step with another `data:` value (or `${VAR}`) replays and types the new value.

### AI verification (`verify:`)

`verify:` asks a model whether the final screen, or a checkpoint along the way, matches what you described. It is off by default, so CI makes no extra model calls unless you turn it on.

```yaml
config:
  verify: off            # default; final | checkpoints | strict
tests:
  - name: Checkout
    description: the order confirmation shows the order number
    verify: final        # this test only; `atalanta run --verify …` wins over both
    steps:
      - do: "place the order"
      - tap: { text: Receipt }
        checkpoint: "the receipt lists two items"
```

| Level | Checks | A failed or unknown check |
|---|---|---|
| `off` (default) | None. Only the compiled, deterministic assertions run, so CI makes no extra model calls | - |
| `final` | After the last step: does the test's `description` (else its name) hold on the final screen? | The test fails |
| `checkpoints` | `final`, plus after every `assert:` step and every step with `checkpoint:` | Recorded, and the test goes on. It fails at the end, listing every failed check |
| `strict` | Like `checkpoints` | The test fails at that step |

`checkpoint: true` on a natural-language step checks its own `waitUntil` (else its intent). On any other step, give the claim as a string.

The checker must cite elements from the screen list. A "holds" that cites none, or cites elements that aren't there, is `unknown`, not a pass. A check whose model call failed is `error`. Every check is in the JSON (`verification`), JUnit (`verify.*` properties) and HTML reports. `atalanta explore` defaults to `final`.

The details are in [ARCHITECTURE.md](ARCHITECTURE.md#natural-language-steps).

## Screenshots

`assertScreenshot` compares the screen with a saved baseline image. Use it for layout and visual regressions that selectors cannot see.

```yaml
- assertScreenshot: home
- assertScreenshot:
    name: checkout
    threshold: 0.99
    mask: [{ id: "clock" }, { id: "ad_banner" }]
    crop: { id: "summary_card" }
```

Baselines live next to the suite in `atalanta-baselines/<platform>[-headless]/<W>x<H>/<name>.png`.

- **New baselines.** A first run writes the baseline and fails, so commit the file after you check it. `--update-baselines=missing` accepts new baselines without failing.
- **Reviewing failures.** Use `atalanta baselines review`, or `atalanta baselines review --serve` for the images side by side with Accept/Reject buttons.
- **Accepting.** `atalanta baselines accept <name|all>` writes the actual screen over the baseline.

## Web

Set `platform: web` to test a site in Chrome or Chromium. Steps and selectors work as on a device, with a few additions listed after the example.

```yaml
appId: web
config:
  platform: web
  web:
    startUrl: "https://shop.example.com"
    device: "Pixel 7"                    # optional mobile-web profile (Chromium, not Safari)
    mocks:
      - { url: "*/api/recommendations", json: [] }
tests:
  - name: Search
    steps:
      - goto: "https://shop.example.com/search"
      - inputText: { selector: { role: searchbox }, text: "shoes" }
      - pressKey: { key: enter }
      - hideKeyboard: {}   # Android only: dismisses the soft keyboard if one is showing; a no-op otherwise.
      - assertVisible: { role: heading, name: "Results" }
      - mockRequest: { url: "*/api/cart", status: 500 }
      - goBack: {}
```

The web adds `goBack`, `refresh`, `mockRequest`/`clearMocks` and `css`/`xpath` selectors. `goto` navigates the page here; on a device it opens the URL as a deep link.

`swipe: up` is the finger moving up, so the page scrolls down, the same as on a device.

## HTTP and API tests

An `http` step sends one request and checks the response. It works in any test: seed data through the API, then check it on screen. A suite whose tests have only `http` steps (with `wait` and `setConfig`) is an API test suite. It runs with no device, may leave out `appId`, and `--parallel N` runs N tests at once.

```yaml
appId: com.example.shop
config:
  http:
    baseUrl: "${API_URL}"                  # a step url without http(s):// goes after it
    headers: { X-Client: atalanta }        # sent with every request
    timeoutMs: 10000                       # per request (default 30000); a step's timeoutMs wins
flows:
  - id: login
    steps:
      - http:
          method: POST
          url: /login
          json: { email: "${USER_EMAIL}", password: "${USER_PASSWORD}" }
          capture: { ACCESS_TOKEN: token } # secret-looking names are masked in reports
tests:
  - name: A new order shows in the app
    steps:
      - runFlow: login
      - http:
          method: POST
          url: /orders
          headers: { Authorization: "Bearer ${ACCESS_TOKEN}" }
          json: { sku: "A1", qty: 2 }
          expect:
            status: 201
            headers: { content-type: { contains: json } }
            json:
              $.id: { type: string }
              items[0].sku: A1             # `$.` may be left out
              total: { gt: 0 }
          capture: { ORDER_ID: id }
      - launchApp
      - tap: { text: "Orders" }
      - assertVisible: { text: "${ORDER_ID}" }
```

A request has `method` (default `GET`), `url`, `headers`, `query`, and at most one body: `json`, `body` (text) or `form`. `followRedirects: false` lets a step check a 3xx itself. A request that carries headers (its own or `config.http.headers`) or a body follows redirects on its own origin only, and `https` never goes to `http`: at any other hop it stops, and the step fails naming both origins unless `expect.status` asks for that 3xx. A bare GET follows any redirect.

`expect:` checks the response. Every check runs, and a failure lists each one that failed with the start of the body.

| Key | Checks |
|---|---|
| `status` | `201`, `[200, 204]` or `2xx`. Without it, any status below 400 passes. |
| `headers` | Header (any case) → matcher on its value, compared as text (`content-length: 0`). |
| `json` | [JSONPath](https://www.rfc-editor.org/rfc/rfc9535) → matcher. A path that names one place (`$.items[0].sku`) gives that value. One with `*`, `..`, a filter (`$.items[?@.sku=='A1']`), a slice or a union gives the list it matched. |
| `body` | Matcher on the whole body as text. |
| `maxMs` | The response arrives within this many milliseconds. |

A matcher is a plain value, which must be equal (numbers by value, `1` = `1.0`), or a map of checks that must all pass:

| Check | Passes when the value… |
|---|---|
| `eq`, `ne` | equals / doesn't equal the given value (`eq: { … }` compares with an object) |
| `gt`, `gte`, `lt`, `lte` | is a number above / at least / below / at most the given one |
| `contains` | has the substring; has the element (each, for a list); or has the given fields, compared the same way (`"$": { contains: { user: { role: admin } } }`) |
| `matches` | is wholly matched by the regex, like `textMatches` (`.*` around it for a part) |
| `exists` | `true`: is there (it may be `null`); `false`: isn't (for a wildcard path: matched nothing) |
| `type` | is a `string`, `number`, `integer`, `boolean`, `object`, `array` or `null` |
| `length` | has a length (string, list or object) that passes the matcher given (`length: { gte: 1 }`) |
| `oneOf` | equals one of the listed values |

`capture:` sets variables for the test's later steps, UI steps included. `NAME: "$.path"` takes the value at a JSONPath, which must match exactly one thing. `{ header: location }` takes a header and `{ regex: "id=(\\d+)" }` the first group in the body. A string is inserted as is; anything else as JSON.

- Captured values and cookies belong to one test. Get a token in each test that needs one, with a flow.
- Using a captured name before the step that captures it fails the step. `${NAME:-default}` gives a fallback.
- A captured name hides an environment variable of the same name.

`retry: { count: 5, intervalMs: 1000 }` sends the request again, up to `count` more times, while its checks fail or no response comes: a backend that catches up after a UI action. It is off by default, because a retried POST runs again.

No response (connection refused, DNS, TLS, timeout) fails the test (exit 1), like a failed check. `optional: true` forgives both. `config.http.insecure: true` accepts any TLS certificate, for a dev server's self-signed one. `RUST_LOG=atalanta_api=debug` logs every request and the start of each response.

## Reordering (`drag`)

`drag` presses one element and drops it on another, the way a person reorders a list.

> Status: verified in headless Chrome only. Not yet run on an emulator, a simulator or a real reorderable list ([STATUS.md](STATUS.md)).

```yaml
appId: com.example.groceries
tests:
  - name: milk moves below eggs
    steps:
      - launchApp: {}
      - drag: { from: { text: Milk }, to: { text: Eggs } }
      - assertVisible: { text: "Eggs, Milk" }
```

A `drag` works in four phases:

1. It presses the centre of `from` and holds it for `holdMs` (default 700, past the platforms' long-press time, so the list picks the row up).
2. It moves onto the centre of `to` over `moveMs` (default 900).
3. It rests 250 ms, so the list drops the row instead of flinging it.
4. It lifts.

Each platform injects the gesture its own way:

- Android uses `input motionevent` (Android 10+; `input draganddrop` before).
- iOS uses W3C pointer actions with a pause, or WebDriverAgent's `dragfromtoforduration`.
- The web uses the mouse with the button held.

A `swipe` is a quick stroke and picks nothing up.

## Host commands (`exec`)

> Status: verified with headless Chrome on macOS. Not yet run with `adb` against a device ([STATUS.md](STATUS.md)).

`exec` runs a program on the machine running Atalanta, such as an `adb` command or a script that seeds test data. It checks how the program ended and keeps its output. It is off unless the suite lists the program in `config.exec.allow`:

```yaml
appId: com.example.app
config:
  exec:
    allow: [adb, ./scripts/seed.sh]
tests:
  - name: a seeded cart shows
    steps:
      - exec: { run: "./scripts/seed.sh cart 3", capture: CART_ID }
      - exec:
          run: [adb, -s, "${ATALANTA_DEVICE_ID}", shell, pm, grant, com.example.app, android.permission.CAMERA]
      - launchApp: {}
      - assertVisible: { text: "Cart ${CART_ID}" }
```

- **No shell.** A string is split on spaces, and quotes keep a part whole, so `;`, `|` and `$(…)` are plain characters. The program, the first part, must be in `config.exec.allow` exactly as written; `atalanta validate` refuses the suite otherwise (exit 2), and the executor checks again.
- **Checks.** `expect: { exitCode: 0 }` (the default) and `contains: "text"`, which the output must include. A failure shows the exit code and the end of the output, masked.
- **Capture.** `capture: NAME` sets `${NAME}` to the trimmed standard output for the test's later steps (masked when the name looks secret).
- **Environment.** `ATALANTA_DEVICE_ID` is the test's device. `timeoutMs` bounds the step (default 60 s), and `cwd` sets its folder.
- **Agents.** An MCP agent can't add `exec` to a suite it writes (`run {yaml}`) outside `--profile full`; see the Rule of Two in [SECURITY.md](SECURITY.md#9-agents-the-rule-of-two).

## Accessibility audit

The audit checks the screens a test already reads against a small set of fixed accessibility rules. It is off by default. Turn it on to catch missing names and small touch targets early, and read the limits below before treating a clean result as a pass.

```yaml
config:
  accessibility:
    audit: true      # off by default
    fail: false      # true: an error finding fails the test
    density: 2.625   # Android px per dp; unset: the screen's short side is 411 dp
```

With `audit: true`, each test applies the rules (`atalanta_common::a11y`) to screens it reads anyway.

- **Input.** The audit uses the trees the run already has: the cached tree after an element lookup on Android and iOS, plus the trees that natural steps, `verify:` checks and failed steps read. It never reads the screen itself, so it adds no device round trip.
- **Frequency.** Each distinct screen is audited once per test. Each problem is reported once per test, however many screens show it.
- **Output.** A finding is a test warning (`accessibility: error name-missing: …`), so every reporter shows it. A test lists at most 20 findings, and one closing note counts the rest.
- **Failing.** With `fail: true`, an error-severity finding fails a test whose steps passed, on its last step.

| Rule | Severity | Flags | Source |
|---|---|---|---|
| `name-missing` | error | a control (button, link, checkbox, switch, radio, tab, slider, menu item, option, combobox) with no accessible name | WCAG 2.2 SC 4.1.2 |
| `image-unlabeled` | error | an icon control (ImageButton, FAB, a button or link whose only content is an image) with no label | WCAG 2.2 SC 1.1.1, 4.1.2 |
| `target-size` | warning | a target under 48×48 dp (Android), 44×44 pt (iOS) or 24×24 CSS px (web) | Android "Make apps more accessible" (48 dp); Apple HIG, Accessibility (44 pt); WCAG 2.2 SC 2.5.8 |
| `name-duplicate` | warning | controls that are siblings in one container and share a name | WCAG 2.2 SC 2.4.6, 2.4.4 |
| `field-unlabeled` | error | an editable field with no label, placeholder or hint | WCAG 2.2 SC 1.3.1, 3.3.2, 4.1.2 |
| `field-placeholder-only` | warning | an iOS or web field named only by its placeholder, which goes once text is typed | WCAG 2.2 SC 3.3.2 |

The rules aim for few false positives, so each one skips what the tree can't show:

- The trees carry no `clickable` flag, so an element counts as a control only when its role says so (`role:`, the table in [README](README.md)). A Compose clickable `View` with no `Role` is never audited.
- Hidden, disabled and off-screen elements are not checked for target size, and the iOS keyboard and status bar are skipped entirely.
- Some controls are sized by the system: a `UISwitch` (51×31 pt) and a native web checkbox or radio (2.5.8's user-agent exception).
- Web links inside a sentence are skipped (2.5.8's inline exception), and so are small web targets with a clear 24 px circle around them (its spacing exception).
- On Android a hint counts as an `EditText`'s label, as Android's accessibility guide recommends.
- A web control inside a `<label>` counts as named.

Limits:

- Without `density`, Android treats the screen's short side as 411 dp. That errs lenient on phones and strict on tablets and unfolded foldables, which should set `density`.
- The web driver reports a node's DOM `id` as its label when it has no `aria-label`, so on the web an `id` hides a missing name.
- The web looks elements up in the page, not on a tree, so a web run is audited only on the trees it reads for other reasons.
- Rules are the cheap, deterministic base, not a verdict. ScreenAudit (Zhong et al., CHI 2025, arXiv 2504.02110) found 69.2% of screen-reader errors, where a widely used rule checker found 31.3%, so a clean audit is no proof of an accessible app.

## Configuration

`config:` holds suite-wide settings. Every key is in [DSL-REFERENCE.md](DSL-REFERENCE.md#config). These are the ones most suites touch:

- `platform`, `app`, `driver` (`device`/`headless`), `framework` (`native`/`react_native`/`flutter`)
- `sync`: timeouts and settle timings. `retry.count`. `actionability`.
- `healing.mode`: `auto` locally, `report` in CI, or `off`.
- `artifacts`: the output folder, failure screenshots, step screenshots.
- `settings`: named driver settings (`atalanta settings` lists them). `atalanta run --set key=value` overrides one for a run, and the `setConfig` step for the rest of a test.
- `batch: true`: consecutive tests share one launch when the previous test passed and left the app in front.

An unknown `config:` key is a warning that names the closest known key. An unknown step key is an error.
