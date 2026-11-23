# Atalanta tests in YAML

A suite is one YAML file: an `appId`, optional `config`, reusable `flows`, and `tests`. `atalanta validate <file>` checks one without running it; `atalanta schema --markdown` prints every key; `atalanta lint <file>` flags fragile selectors and fixed waits.

```yaml
appId: com.example.shop
config:
  platform: android            # android | ios | web; detected when omitted
  sync:
    timeoutMs: 10000           # how long a lookup or assertion waits
flows:                         # reusable step groups, called with runFlow
  - id: login
    steps:
      - inputText: { selector: { id: email }, text: "${TEST_USER}" }
      - inputText: { selector: { id: password }, text: "${TEST_PASSWORD}" }
      - tap: { role: button, name: "Sign in" }
tests:
  - name: Log in
    tags: [smoke]
    steps:
      - launchApp: { clearState: true }
      - runFlow: login
      - assertVisible: { text: "Welcome" }
```

## Steps

One action per step, plus optional modifiers:

```yaml
- tap: { text: "Pay" }
  timeoutMs: 20000     # this step's wait
  optional: true       # a failure is a warning; the test goes on
  force: true          # skip the actionability checks
```

Common actions: `launchApp`, `stopApp`, `tap`, `doubleTap`, `longPress`, `inputText: { selector, text }`, `clearText`, `pressKey: { key: back }`, `swipe: { direction: up }`, `scrollUntilVisible: { selector, direction }`, `assertVisible`, `assertNotVisible`, `assertText: { selector, expected }`, `runFlow: <id>`, `http` (API calls). You rarely need `wait:`: lookups and assertions wait, and actions wait for the screen to settle.

## Selectors

A selector is a map; every key in it must match.

| Want | Selector |
|---|---|
| A stable id (resource-id, accessibilityIdentifier, `testID`, web `id`) | `{ id: "submit" }` |
| Exact visible text or label | `{ text: "Sign in" }` |
| Part of the text, ignoring case | `{ textContains: "sign" }` |
| A regex over the whole text | `{ textMatches: "Total: \\$[0-9.]+" }` |
| A role and accessible name | `{ role: button, name: "Pay" }` |
| A state (only beside another key) | `{ role: checkbox, name: "Remember me", checked: false }` |
| By position | `{ text: "Edit", below: { text: "Address" } }`, `{ id: "row", index: 2 }` |
| Inside a container | `{ text: "Delete", childOf: { id: "item_3" } }` |

Prefer `id`, then `role` + `name`, then `text`. A selector that matches several elements uses the first and warns.

## Variables and secrets

- `${VAR}`, `${VAR:-default}`, `${VAR:?message}` come from `--env K=V`, then the environment. `$${` writes a literal `${`.
- Never write a secret into a test. Use `${NAME}` (or `${keychain:NAME}`); its value is masked in every report and log.
- An `http` step's `capture:` sets variables while the test runs.

`docs/DSL.md` explains every feature; `docs/DSL-REFERENCE.md` lists every key.
