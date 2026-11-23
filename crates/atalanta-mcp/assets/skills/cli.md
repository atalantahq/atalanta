# Atalanta from the shell

Drive an Android emulator or device, an iOS simulator, or Chrome, one command at a time, then save what you did as a test that CI replays with no model. This guide matches the installed `atalanta` binary.

## The loop

```bash
atalanta snapshot                  # what is on screen, one line per element, each with a ref
atalanta tap @e7                   # act on a ref; prints what changed on screen
atalanta type @e3 "qa@example.com" # type into a field
atalanta assert "Welcome"          # check something is visible
atalanta export --out atalanta/login.yaml   # save the session as a test, with assertions
atalanta gate atalanta/login.yaml --goal "log in"   # trust gates: replays, wrong path, goal text
atalanta run atalanta/login.yaml   # replay it: no model involved
```

- Every action waits for the screen to settle and then prints a **delta**: `+` added, `-` removed, `~` changed. Use the refs in it directly; snapshot again only when you need the whole screen.
- A ref (`@e7`) keeps naming the same element for as long as that element stays on screen. When it is gone you get `STALE_REF`: take a new snapshot. Atalanta never taps a different element in its place.
- Targets other than refs: `"Log in"` (exact text or accessibility label), `id=login_button`, `textContains=network`, or any selector as JSON: `'{"role":"button","name":"Pay"}'`. A ref is the safest: it is checked against the element you saw.

## Reading the screen cheaply

- `atalanta snapshot --delta`: only what changed since the last snapshot.
- `atalanta snapshot --find wi-fi`: only lines that contain the text (any case).
- `atalanta snapshot --within @e12`: only what lies inside one element (a dialog, a list).
- `atalanta snapshot --limit 40`: at most 40 lines. The header then says `incomplete` and how many more there are.
- `atalanta screenshot`: saves a PNG and prints its path. Look at it only when the tree is not enough.

## Other actions

`press back|home|enter|…` · `swipe up|down|left|right` (the finger's direction) · `scroll down --until "Privacy"` · `launch com.example.app [--clear]` · `stop com.example.app` · `assert "Text" --not` · `wait "Loaded" --timeout 10000`

Secrets: `atalanta type @e4 --secret LOGIN_PASSWORD` types the value of that environment variable. It must be listed in `ATALANTA_SECRETS`. The value is masked everywhere and exported as `${LOGIN_PASSWORD}`. Never put a password on the command line.

## When something fails

Add `--json` to get one JSON object: `{"v":1,"ok":false,"error":{"code","message","retry","delivered","next"}}`. Branch on `code` and `retry`, not on the message text.

| retry | meaning |
|---|---|
| `safe` | nothing happened that repeating could double; repeat it |
| `observe_first` | the effect is unknown; `atalanta snapshot` before repeating |
| `no` | it fails the same way again; follow `next` (often: take a new snapshot) |

`delivered` says whether the call reached the device: `no`, `unknown`, or `yes` (it changed the screen before failing, e.g. a `scroll --until` that never found its target: snapshot before deciding).

Common codes: `STALE_REF`, `NOT_FOUND`, `NOT_ACTIONABLE`, `ASSERTION_FAILED`, `APP_CRASHED`, `INVALID_ARGS`, `DEVICE_NOT_FOUND`, `DEVICE_BUSY` (another agent holds the device: repeat when it is done), `DEVICE_LOST`, `TIMEOUT`. Exit status is 0 on success, 1 on any failure the reply describes, and 2 for a malformed command line.

## Sessions

Refs and the recording belong to a session. By default that is this git worktree (else this directory) plus its device. Pass `--session NAME` (or set `ATALANTA_SESSION`) to every command when several agents share a checkout. Use `atalanta session` to see the current session, and `atalanta session reset` to start again.

The first command picks the first booted device (`--device ID` to choose one), and the session keeps it. Each command holds the device until it has printed its delta; another agent's command waits for it.

## The web

`atalanta launch https://example.com -p web` and later commands of that session drive Chrome. They run inside `atalanta daemon` (started for you on the first command) so the page stays open between commands; `atalanta daemon stop` closes it. Web sessions share one page.

## Proving a change and leaving a test behind

- `atalanta verify-change "the paywall shows a monthly plan" --app-id com.example.app`: explores, replays the result 5 times, checks that it fails on a wrong path, and writes a PR evidence bundle (verdict, video, `comment.md`). A second run of an unchanged flow makes no model calls.
- `atalanta explore "<goal>" --app-id …`: an agent writes the test for you.
- Tests are plain YAML (`atalanta skills get yaml`). Never write a secret into one; use `${NAME}`.
