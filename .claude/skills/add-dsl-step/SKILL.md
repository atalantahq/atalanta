---
name: add-dsl-step
description: Add a new step (action) to Atalanta's YAML DSL, or change an existing step's arguments — the Action enum, PlatformDriver trait method, ResilientDriver and RoutingDriver forwarding, parser (ACTION_KEYS, parse_action), JSON schema, reference line, validator, env interpolation, executor, each platform driver, the generated docs/DSL-REFERENCE.md, DSL.md, STATUS.md and the changelog. Lists the places the compiler will not catch. Use when asked to add, rename or extend a DSL step such as tap, swipe, drag or hideKeyboard.
---

# Add a DSL step

A step touches about twenty places. The compiler finds the exhaustive `match`es; the places marked **silent** below compile and pass tests when forgotten, and the step then does the wrong thing at run time. Do them in this order, and take an existing step of the same shape as the template: `hideKeyboard` for a step with no arguments, `drag` for one with selectors and options. `git log --all --oneline --grep hideKeyboard` finds the commit that added it.

Naming: the YAML key is a lowerCamelCase verb (`hideKeyboard`, `scrollUntilVisible`), the variant is PascalCase, and argument keys are camelCase in YAML (`holdMs`) and snake_case in Rust. Durations are `…Ms`. Optional numbers are `Option<u64>` whose default is a constant in `executor.rs` applied by the executor, and the schema description says "(default N)".

## 1. Type and trait: `atalanta-common`

- `types.rs`, `enum Action`: add the variant.
- `traits.rs`, `PlatformDriver`: add the method with a default body that returns `AtalantaError::Unsupported("<step> isn't supported on this platform …")`. Drivers that support it override it; the rest inherit the error (exit 2, kind `PlatformNotSupported`).
- `resilience.rs`, `impl PlatformDriver for ResilientDriver`: forward the method. **Silent**: without this every real run gets the trait default, `Unsupported`, even though the driver implements it. Use `with_once` for anything that changes the device, so it is never retried; add a forwarding test like `keyboard_frame_and_hide_keyboard_are_forwarded`.
- `natural.rs`, `cacheable()`: only if a natural-language step may resolve to it.

## 2. Parser: `atalanta-core/src/parser.rs`

- `ACTION_KEYS`: add the key. This list is the source for the schema, the reference, the REPL's typo hints and the unknown-key error.
- `parse_action`: add the arm. Arguments are a struct with `#[serde(rename_all = "camelCase", deny_unknown_fields)]`; a step with no arguments uses `no_args`.
- No arguments: also add the bare form to `visit_str`, or `- shake` is rejected with "needs arguments" while `shake: {}` works.
- A selector plus options accepts both `step: { selector: {…}, opt }` and the inline `step: { text: …, opt }`; see `LongPressDsl`. Several selectors get named keys (`from`, `to`).
- `force: true` is accepted only for the steps in `schema.rs`'s `FORCE` and the matching parser check.
- `maestro_hint`: if Maestro has the same thing under another name.
- Tests: the short and long forms, an unknown argument, a wrong type.

## 3. Schema, reference, validation: `atalanta-core`

- `schema.rs`, `action_arg`: add the arm. **Silent**: a missing arm falls back to `any_value()`, so the editor schema accepts anything for the step.
- `schema.rs`, test `every_schema_action_parses`: add an example for the step.
- `reference.rs`, `action_doc`: one line, which is also the schema's description and the row in the generated reference. Say which platforms support it.
- `validator.rs`, `validate_step`: validate every selector the step carries. `capabilities()`: return `Can::Never` or `Can::Maybe` for a platform that cannot do it, so `atalanta validate` says so before a run does.
- `env.rs`, `interpolate_action`: expand `${VAR}` in every selector and string argument.
- `selector_lint.rs`, `waits_for_its_target` and `extract_selector`: **silent**; add the step if it has a target element.
- `lib.rs`, `steps_need_no_device`: only for a step that needs no device.

## 4. Executor: `atalanta-core/src/executor.rs` and `executor/actions.rs`

- `action_name` and the `match` in `execute_action` are exhaustive.
- **Silent**, each with a catch-all arm: `is_mutating` (decides whether the screen is settled after the step — a step that changes the screen and is missing here races the next step), `is_read_only`, `action_selector`, `crash_sticks_to`, `describe`, and `skip_sync` in `run_action`.
- `executor/tests.rs`: record the call in `ScriptDriver`, forward it in the wrapping fakes that forward the others (`HangingTap`, `CrashingTap`, `CoveredUnlessForced`, `StateKeeping`), and add a step test that asserts the driver call and its arguments.

## 5. Drivers

Override the trait method where the platform can do it: `atalanta-android/src/driver.rs`, `atalanta-ios/src/driver.rs` (through `wda.rs`), `atalanta-web/src/driver.rs` (through `protocol.rs` and `cdp.rs`; the fake page in the driver's tests needs it too). `atalanta-headless`, `atalanta-flutter-bridge` and `NoDeviceDriver` inherit the default unless they can do it.

`atalanta-inspector/src/console/routing.rs`, `RoutingDriver`, must forward the method: `every_trait_method_is_forwarded` fails otherwise.

A new error is classified in `atalanta-common/src/error.rs` (`AtalantaError::kind`): transient errors are retried, permanent ones fail at once, and an action is never retried after `CommandFailed`.

## 6. Only if agents or the converter should produce it

- `atalanta-nl/src/goal.rs`: `GoalOp`, `NAMES` (a fixed-length array), `needs_element`, the prompt text; `grounding.rs` builds the `Action`. The model returns an op and an element number, never a selector or a value.
- `atalanta-explore/src/session.rs` and `yaml.rs` (`to_step`). **Silent**: a missing `to_step` arm is a run-time "explore can't write … as a step".
- `atalanta-migrate/src/maestro.rs`, `convert_step`, when Maestro has the command.

## 7. Docs

- `docs/DSL-REFERENCE.md` is generated: `ATALANTA_BLESS=1 cargo test -p atalanta-core --test dsl_reference`. Never edit it by hand.
- `docs/DSL.md`: a section or an example. Every whole suite in a `yaml` block there is parsed and validated by `doc_examples`.
- `docs/STATUS.md`: a row saying where the step was run. "Host tests" until it has run on an emulator, simulator or Chrome; name the platforms not yet run in the last column.
- `CHANGELOG.md`: one line under `## [Unreleased]`, `### Added`, `#### Suite DSL`.

## Verify

```bash
cargo test -p atalanta-common -p atalanta-core
cargo test -p atalanta-inspector every_trait_method_is_forwarded
cargo test -p atalanta-core --test doc_examples --test dsl_reference
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Then run the step for real: write a two-step suite that uses it and run it on each platform that claims support (`verify-e2e`). Unit tests use fakes that record the call; only a device shows whether the gesture does anything. Report which platforms ran it and put the rest in `docs/STATUS.md`.
