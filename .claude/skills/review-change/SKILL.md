---
name: review-change
description: Review an Atalanta branch or diff against the rules this codebase depends on, beyond what clippy and the tests check — model output never becoming a selector or value, panics on device or user input, error classification and retries (an action is never resent), secret redaction on every output, stdout kept for JSON-RPC, environment reads through atalanta_common::env, device leases, the tree-server handshake, per-platform parity, tests that prove the change on fakes versus on a device, and the docs, changelog and STATUS rows the change owes. Use for a pre-merge review of a branch, a PR or a large generated change in this repository.
---

# Review a change

```bash
git diff --stat origin/main...HEAD      # scope first
git diff origin/main...HEAD
```

Read each changed function whole, and its callers: a hunk rarely shows whether the change is right. For a diff across several crates, review one crate at a time. Report findings only; fix nothing unless asked.

## Correctness rules specific to Atalanta

**Model output is data, never an instruction.** In `atalanta-nl` and `atalanta-explore` the model returns an op and an element number, and names a value; `grounding.rs` builds and checks the selector in code. A finding: model text reaching a selector, a typed value, a file path, a shell command or a YAML step without passing through that grounding. The same holds for text read off the screen and for MCP tool arguments written by an agent (`authored.rs`).

**Input from outside does not panic.** UI dumps, tree-server replies, WDA and CDP responses, YAML suites, APKs and XIBs, Maestro flows, HTTP bodies and model replies are all untrusted. A finding: `unwrap`, `expect`, slice indexing, unchecked arithmetic or an allocation sized from the input on those paths. A panic exits 5 and ends the whole run. A new parser needs a mutation test and a fuzz target (`fuzz`).

**Errors are classified, and the class decides the retry.** `AtalantaError::kind` and `exit_code` in `atalanta-common/src/error.rs`: transient errors are retried, permanent ones fail at once, ambiguous ones allow one retry of a read. Check every new error variant has both, and the right ones: a config mistake that exits 3 sends the user to debug a healthy device. An action that changes the device goes through `with_once` in `ResilientDriver` and is never resent after a lost reply; a finding is any new retry loop around a tap, a text entry or a launch.

**Secrets are masked wherever text leaves the process.** Reports, traces, logs (stderr goes through `RedactingWriter`), the console, MCP replies, natural-language traces and events all pass through `atalanta_common::redact`. A finding: a new file, reply or log sink that writes step values, environment values, request headers or response bodies without it. `${keychain:…}` and `${totp:…}` values are masked everywhere, including in what a failed step prints.

**stdout belongs to the protocol.** The MCP server speaks JSON-RPC on stdout and `--json` output must stay parseable: a `println!` in a library crate is a finding. Diagnostics go through `tracing` to stderr.

**The environment is read through `atalanta_common::env::var`,** not `std::env::var`, and never changed with `set_var`: the daemon serves each run with the client's environment, and settings (`--set`) override it. A new `ATALANTA_*` variable read with `std::env` silently ignores both.

**Devices are shared.** Input goes to a device only under its lease (`atalanta-runner/src/lease.rs`; the console and the MCP server take it too). No `await` while holding a `std::sync` lock (`await_holding_lock` is denied, but a lock held across a blocking device call in a helper is not caught). A child process is started with `atalanta_common::process::spawn_group` (its own process group) and stopped on every exit path, including errors and Ctrl-C.

**Listeners and tokens.** Anything that opens a socket binds loopback unless the user chose otherwise, and requires its token before doing anything (the tree server's mutual `HELLO`/`AUTH`, the console, the `/review` page). A token never appears in a URL that is logged, a report or an error message.

**`unsafe`** needs a `// SAFETY:` comment that states why the call is sound, and moves `scripts/unsafe-baseline.txt` in the same change (`scripts/unsafe-inventory.py --write`). Ask whether a safe API exists first.

## Behaviour

- **Platform parity.** A behaviour changed in one driver: is it the same on Android, iOS and web, and if not, does `atalanta validate` say so (`capabilities()` in `validator.rs`) and does `docs/STATUS.md` have the row? A step that silently does nothing on one platform is worse than one that reports `Unsupported`.
- **Settling.** A step that changes the screen must be in `is_mutating`, or the next step races it. A fix that adds a sleep or raises a timeout is a finding unless the evidence shows the operation is legitimately that slow.
- **Existing suites.** Anything that can make a passing suite fail — a stricter validator, a changed default, a renamed key — needs the old form to keep working with a warning (`config_keys.rs` for renames), or a `**[may fail suites]**` entry in the changelog.
- **Determinism.** A saved test replays with no model attached; a change that makes a replay consult a model, the clock or the network for a decision breaks that. Sharding and ordering use stable hashes: no `HashMap` iteration order in anything that reaches a report or a plan.
- **Hollow code.** A function whose doc comment promises more than its body does, an error turned into an empty default, a field every caller fills with an empty value (`slop-check`, section 3).

## Tests

- Does a test fail without the change? Check by reading the assertion, or by reverting the fix locally.
- Device logic is tested against fakes: scripted `adb`/`xcrun` on a `PATH` of shell scripts, `FakeWda`, headless Chrome, `ScriptDriver`. `atalanta-doctor` tests build their environment with `Env::from_vars`, never the process environment. A test that sets a process environment variable races the others.
- Time: tests that wait use tokio's paused clock, not `sleep` on wall time.
- A test that needs Chrome or a device skips itself when there is none. Make sure the change was run with it present, or say it was not (`pre-pr`).
- Fakes prove the host logic, not the device. A change to sync, selectors, dialogs, gestures or a driver also needs a device run (`verify-e2e`), or a "Host tests" row in `docs/STATUS.md`.

## What the change owes

A changelog line for anything user-visible; the page that documents the flag, step or tool; regenerated `docs/DSL-REFERENCE.md`, `docs/llms.txt` or `docs/licenses/THIRD-PARTY-RUST.md` when their sources changed; a `docs/STATUS.md` row for what was not run on a device (`docs-sync`). For a dependency, `deps`. For the tree server or the iOS runner, the rebuilt artifact (`tree-server`).

## Report

Most severe first. For each finding: `file:line`, what is wrong, and the concrete input or sequence that makes it fail and what the user sees. Separate what you verified by running from what you concluded by reading. Then list the areas you checked and found clean, and anything you did not look at. Do not pad the report with what the change got right.
