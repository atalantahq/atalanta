---
name: add-mcp-tool
description: Add, rename or change a tool of Atalanta's MCP server (crates/atalanta-mcp) — the definition and JSON input schema in server.rs, the dispatch arm in call_tool, the read-only/destructive/no-device/idempotent lists that drive annotations and --read-only, the core profile (observe/act/run/artifacts) and its schema-size budget, docs/MCP.md and the tool counts in README.md and CONTEXT.md that tests check. Use when working on an MCP tool, when the_mcp_doc_lists_every_tool, the_docs_give_the_real_tool_counts or core_profile_schema_budget fails, or when changing what agents see in tools/list.
---

# Add or change an MCP tool

There is no registry or macro: a tool is a definition, a `match` arm and membership in several hand-kept lists, all in `crates/atalanta-mcp/src/`. A tool missing from a list still works, but with the wrong safety behaviour. `list_tests` is the smallest complete example; grep for `"list_tests"` and follow every hit.

## First: does it need to be a tool?

The default profile serves five tools (`observe`, `act`, `run`, `artifacts`, `verify_change`), and every tool's schema is paid for in each agent's context. Prefer, in this order:

1. A new `kind`/action of a core tool: a row in `ACTIONS`, `OBSERVE` or `ARTIFACTS` in `profile.rs`, expanded by `profile::expand` to a full tool.
2. A new tool listed only under `--profile full`.
3. A new core tool — almost never. `core_profile_schema_budget` caps the core profile at five tools and 3600 bytes of schema.

## Steps

1. **Implement** in `src/tools/<area>.rs` as a function returning `Result<serde_json::Value>`. Keep the result small: an agent reads all of it. Device input and model output never become a selector or a path without the checks the neighbouring tools use.
2. **Define** it in `server.rs`: `extra_tool_definitions()` with `def(name, description, schema)`. The schema is a `serde_json::json!` JSON Schema object. A tool that takes a `selector` property automatically also accepts `ref` (`tool_definitions()` adds it). The description is the agent's only documentation: say what the tool changes, what it returns and when not to call it.
3. **Dispatch** it: an arm in the `match name` of `call_tool`.
4. **Classify** it. Each list is a decision, not boilerplate:

   | List | Put the tool in when | Effect |
   |---|---|---|
   | `READ_ONLY_TOOLS` (`server.rs`) | it changes nothing on the device or host | served under `--read-only`; `readOnlyHint` |
   | `DESTRUCTIVE_TOOLS` | it loses state (clears data, uninstalls) | `destructiveHint` |
   | `IDEMPOTENT_TOOLS` | calling it twice equals calling it once | `idempotentHint`; agents retry these |
   | `NO_DEVICE_TOOLS` | it needs no device | skips the device lease |
   | `SCREEN_CHANGING_TOOLS` (`refs.rs`) | it can change what is on screen | element refs from the last snapshot are invalidated |
   | `RECORDED_TOOLS` (`recording.rs`) | it should appear as a step in an exported test | recorded into the session flow |
   | `IRREVERSIBLE_TOOLS` (`profile.rs`) | it changes the machine itself | kept out of every profile but `full` |
   | `authored::call_refusal` (`authored.rs`) | it reads or writes host files, or runs something | refused for agent-written input outside `--profile full` |

   A tool that changes the screen and is missing from `SCREEN_CHANGING_TOOLS` lets an agent tap a stale ref. A tool wrongly in `READ_ONLY_TOOLS` runs under `--read-only`.
5. **Core access**, if agents on the default profile should reach it: the row in `profile.rs` (step 1 of the list above) and `core_equivalent`, which builds the hint an agent gets when it calls a full tool the profile does not list.
6. **Renaming**: add the old name to `TOOL_ALIASES` so existing agent prompts keep working.
7. **Background work** (anything longer than a call should block): follow `run_task` — a `Child` handed to `TaskManager::spawn`, output in the task folder, progress read from `events.jsonl` (`ATALANTA_EVENTS_FILE`), polled with `task_status`. Do not hold the call open.

## Docs the tests check

- `docs/MCP.md`: a section headed exactly ``### `tool_name` ``. The section contains the words "Read-only." if and only if the tool is in `READ_ONLY_TOOLS` (`the_mcp_doc_lists_every_tool`).
- The count of full-profile tools: "`--profile full` serves all N" in `README.md` and "N with `--profile full`" in `CONTEXT.md` (`the_docs_give_the_real_tool_counts`). The same number in `docs/AGENTS.md` is not checked; update it too.
- A new `atalanta mcp` flag must appear in `docs/CLI.md` or `docs/MCP.md` (`every_long_flag_of_the_main_commands_is_documented` in `atalanta-cli`).
- `crates/atalanta-mcp/assets/skills/mcp-tools.md` (the core tools' table, served by `atalanta skills get mcp-tools`) and the `INSTRUCTIONS`/`CORE_INSTRUCTIONS` texts in `server.rs` name tools; no test compares them with the code.
- `assets/atalanta-agent-rules.md` must stay under 6000 characters and its first `yaml` block must be a valid suite.
- `CHANGELOG.md`: a line under `### Added`, `#### MCP and console`.

## Test

Unit tests live beside the server. `launched(xml)` in `mod t2_tests` gives a server on a headless Android layout, and `call(&mut s, "tool", json!({…}))` runs one tool; `handle_line(raw_json)` covers the JSON-RPC path. Test the refusal paths as well as the result: the tool under `--read-only`, with a stale ref, with a path outside the project.

```bash
cargo test -p atalanta-mcp-server
cargo test -p atalanta-mcp-server --lib core_profile_schema_budget -- --nocapture   # prints the schema sizes
cargo test -p atalanta-cli every_long_flag_of_the_main_commands_is_documented
```

By hand, against a booted device: the agent commands (`atalanta snapshot`, `atalanta tap @e7`, …; `atalanta skills get cli`) each run one tool in-process. For the real transport, pipe newline-delimited JSON-RPC into the server:

```bash
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' \
  | target/debug/atalanta mcp --profile full
```

Logs go to stderr; stdout is the protocol, so nothing in the server may print to it.
