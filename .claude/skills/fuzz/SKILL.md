---
name: fuzz
description: Run Atalanta's cargo-fuzz targets (fuzz/: yaml_suite, android_tree, ios_page_source, tree_server_lines, http_expect, maestro_migrate), reproduce and minimise a crash from fuzz/artifacts or from the weekly Fuzz job in the Security workflow, fix the parser, and keep the input as a regression test. Also covers adding a fuzz target for a new parser of untrusted input. Use when a fuzz job fails, when asked to fuzz something, or when writing or changing a parser for a UI dump, a device or WDA reply, YAML, JSONPath or a Maestro flow.
---

# Fuzzing

The targets cover the parsers that read input Atalanta does not control. A panic in one of them takes down a whole run with exit 5, so a crash is a bug even when the input is nonsense: the parser must return an error.

| Target | Exercises |
|---|---|
| `yaml_suite` | `parse_headers`, `parse_suite_from_str`, `validate_suite` (`atalanta-core`) |
| `android_tree` | `parse_hierarchy`, `parse_hierarchy_v2` (`atalanta-android/src/parser.rs`) |
| `ios_page_source` | `parse_ios_hierarchy` (`atalanta-ios/src/parser.rs`) |
| `tree_server_lines` | the tree server's reply parsers, through `tree_server::fuzzing::parse_reply_lines` |
| `http_expect` | the JSONPath parser and `select`, `HttpRequest` from YAML, `atalanta_api::validate` |
| `maestro_migrate` | `MaestroMigrator::migrate_file` |

`fuzz/` is its own workspace with its own `Cargo.lock`; `cargo build`, clippy and the tests never see it. It needs a nightly toolchain and cargo-fuzz. CI pins both (`nightly-…` and `cargo-fuzz@…` in the `fuzz` job of `.github/workflows/security.yml`); use the same nightly when reproducing a CI crash.

```bash
rustup toolchain install nightly --profile minimal
cargo install --locked cargo-fuzz
```

## Run

From `fuzz/`:

```bash
cargo +nightly fuzz list
cargo +nightly fuzz run yaml_suite -- -max_total_time=60 -timeout=10 -rss_limit_mb=2048
```

`scripts/security-check.sh fuzz` runs every target for `FUZZ_SECONDS` (default 30). The corpus is not committed (`fuzz/corpus/` is ignored), so a fresh run starts from nothing and finds little. Seed it first, the way the "Seed corpus" step of the CI job does: the YAML under `examples/` and `tests/smoke/` into `fuzz/corpus/yaml_suite/`, `tests/fixtures/maestro_compat/*.yaml` into `fuzz/corpus/maestro_migrate/`, and a real dump or reply into the others. Sixty seconds is a smoke test; after changing a parser, give its target several minutes.

Without nightly, this still type-checks every target on stable after an API change:

```bash
RUSTFLAGS="--cfg fuzzing" cargo check --manifest-path fuzz/Cargo.toml
```

## A crash

1. **Get the input.** Locally it is `fuzz/artifacts/<target>/crash-<hash>` (also `timeout-…`, `oom-…`). From CI, download the `fuzz-artifacts-<target>` artifact of the failed run into that folder.
2. **Reproduce**, then shrink:

   ```bash
   cd fuzz
   cargo +nightly fuzz run <target> artifacts/<target>/crash-<hash>
   cargo +nightly fuzz tmin <target> artifacts/<target>/crash-<hash>
   ```

   Read the panic message and the top frames inside `atalanta_*`: that line is the bug. A `timeout-` input is quadratic or unbounded work; an `oom-` input is an allocation sized from the input.
3. **Write the failing test first**, in the parser's own `#[cfg(test)] mod tests`, with the minimised bytes as a literal and a name that says what the input is, not that a fuzzer found it:

   ```rust
   #[test]
   fn a_node_closed_twice_is_an_error_not_a_panic() {
       assert!(parse_hierarchy("<hierarchy><node/></node></hierarchy>").is_err());
   }
   ```

   Assert the outcome the caller needs (an `Err`, or a tree without the bad node), not only "does not panic". Each fuzzed parser also has a mutation test over a real sample (`mangled_dumps_never_panic`, `mangled_sources_never_panic`, `mangled_replies_never_panic`); if the crash needed a shape the sample lacks, extend the sample.
4. **Fix the cause.** Replace the `unwrap`, the slice index or the unchecked arithmetic with a returned error; bound the loop or the allocation. Do not catch the panic, and do not make the fuzz target skip the input.
5. **Confirm**: the test passes, the crash file no longer crashes, and the target runs clean for a few minutes from the old corpus. Do not commit `fuzz/artifacts/` or `fuzz/corpus/`.

Classify the error the parser now returns in `atalanta-common/src/error.rs`: a malformed dump from a device is usually worth one retry (the next dump may be whole), a malformed suite never is.

## A new target

Add one when a new parser reads bytes from a device, a browser, a model, the network or a user's file.

1. `fuzz/fuzz_targets/<name>.rs`, shaped like `android_tree.rs`: `fuzz_target!(|data: &[u8]| { … })`, call the parser, ignore the `Result`. No filesystem or network; `maestro_migrate` shows a private temp dir when the API needs paths.
2. A `[[bin]]` entry in `fuzz/Cargo.toml` (copy an existing one), and the crate as a path dependency.
3. A private parser is exposed through a `#[cfg(fuzzing)] pub mod fuzzing` in its crate, as `atalanta-android/src/tree_server.rs` does. `cfg(fuzzing)` is declared in `[workspace.lints.rust]`, so it does not warn.
4. Add the target to the matrix and the seed step of the `fuzz` job in `.github/workflows/security.yml`.
5. Add a mutation test beside the parser, so the ordinary test run covers the same ground without nightly.

## Report

The target, the minimised input (as a string or hex), the panic and its `file:line`, the fix, the test's name, and how long the target then ran clean.
