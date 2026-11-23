---
name: slop-check
description: Audit Atalanta's Rust for unfinished or lazy code — stubs, functions that claim more than they do, swallowed errors, unwrap/expect in shipped code, #[allow] added to silence a warning, hedging comments ("for now", "simplified"). Runs clippy's pedantic and restriction lints as a report (not a gate), tallies them with clippy_tally.py, and sweeps with ripgrep. Use when asked to check for AI slop, audit code quality, review a large generated change, or before a release.
---

# Slop check

Report only. Do not fix anything unless asked: most hits are fine, and the job is to find the few that are not.

CI's gate is `cargo clippy --workspace --all-targets -- -D warnings` with the lints in `[workspace.lints]` (`Cargo.toml`). This check is wider and is not a gate: pedantic and nursery report thousands of style warnings here, so never pass `-D warnings` with them (it stops at the first crate and hides the rest).

## 1. Clippy, shipped code only

Leave out `--all-targets`: tests are full of `unwrap()` by design and drown the signal.

```bash
cargo clippy --workspace --all-features --message-format=json -- \
  -W clippy::pedantic -W clippy::nursery \
  -W clippy::unwrap_used -W clippy::expect_used -W clippy::panic \
  -W clippy::todo -W clippy::unimplemented -W clippy::dbg_macro \
  -W clippy::print_stdout -W clippy::redundant_clone -W clippy::allow_attributes \
  > /tmp/clippy.json 2>/dev/null

python3 .claude/skills/slop-check/clippy_tally.py /tmp/clippy.json \
  --show unwrap_used,expect_used,allow_attributes,redundant_clone
```

For a branch, count only the files it changed:

```bash
git diff --name-only origin/main... | \
  python3 .claude/skills/slop-check/clippy_tally.py /tmp/clippy.json --files - \
  --show unwrap_used,expect_used,allow_attributes,redundant_clone,print_stdout
```

How to read it:

| Lint | Expect | A finding when |
|---|---|---|
| `todo`, `unimplemented`, `dbg_macro`, `panic` | 0 in shipped code | any hit |
| `unwrap_used` | a few dozen | it is reachable from input Atalanta does not control: a UI dump, a WDA or CDP reply, a YAML file, an APK or XIB, a model reply. An unwrap right after the check that makes it safe (`is_some()`, `contains_key`, a length test) is safe but should be a `let … else` or `if let` |
| `expect_used` | every message states the invariant (`"checked at start"`) | the message is a guess, or the invariant is not established in the same function |
| `allow_attributes` | each has a reason in a comment | it silences `dead_code`, `unused_*` or a clippy correctness lint with no reason. Prefer `#[expect(...)]`, which fails once the warning is gone |
| `redundant_clone` | a handful | any; they are free to remove |
| `print_stdout` | many in `atalanta-cli` (it is a CLI) | a library crate prints to stdout: the MCP server speaks JSON-RPC on stdout, and `--json` output must stay parseable |

The rest of pedantic and nursery (`must_use_candidate`, `missing_errors_doc`, `doc_markdown`, the cast lints) is style. Mention the totals in one line and move on.

## 2. Ripgrep sweep

```bash
rg -n --type rust -g '!target' \
  -e 'todo!|unimplemented!|dbg!' \
  -e '(?i)\bfor now\b|should work|hopefully|simplified|\bdummy\b' \
  -e '\b(TODO|FIXME|HACK|XXX)\b' \
  -e '(?i)not (yet )?implemented|in a real ' \
  crates atalanta-cli

# every unsafe block has a SAFETY comment: the two counts match
rg -c --type rust 'unsafe\s*\{' crates atalanta-cli | awk -F: '{n+=$2} END {print n}'
rg -c --type rust 'SAFETY:'     crates atalanta-cli | awk -F: '{n+=$2} END {print n}'

# tests that cannot fail, and tests switched off
rg -n --type rust -e 'assert!\(true\)' -e '#\[ignore' crates atalanta-cli tests
```

Read each hit before reporting it. Known non-findings: "Android only for now" and the like describe a platform limit; `fake.stub(...)` is the test helper that fakes `adb`/`xcrun`; `# TODO:` in `atalanta-mcp/src/tools/flow.rs` is text written into a generated test; "temporary file" is a temp file.

## 3. Hollow functions

Neither pass above finds a function that compiles, has a doc comment, and does nothing. Look for them by hand in what the change touched:

- The doc comment lists more than the body does. Read both.
- A parser that returns `Self::empty()` or `Default::default()` on every path, or a loop that only advances a cursor.
- `unwrap_or_else(|_| …empty())`, `.ok();`, `let _ = …` on a result whose failure the user would want to know about. Cleanup on a path that is already failing (`kill`, `wait`, removing a temp file) is fine.
- A field or parameter that every caller fills with an empty value.
- A route or subcommand that answers "not implemented".
- A test whose assertions would pass if the function under test returned its input unchanged.

`atalanta-headless` is where this has turned up before (`android/resources.rs`, `ios/inflate.rs`); check it first when it is in scope.

## Report

Lead with the hollow functions and the reachable panics, each as `file:line`, what the code claims, what it does, and what a user would see. Then one table of the slop-lint counts. Say which crates you did not read by hand.
