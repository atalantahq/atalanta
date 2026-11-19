# Velocity from a coding agent

A coding agent (Claude Code, Codex, Cursor, …) can use Velocity in two ways. Both drive the same devices and write the same YAML.

| | CLI + skill | MCP server |
|---|---|---|
| How the agent calls it | Shell commands (`velocity run`, `velocity explore`, …) | Tool calls to `velocity mcp` |
| What it costs the context | Only the commands it runs and their output | The tool definitions, every turn: about 850 tokens for the default five tools |
| Driving a screen step by step | Yes: `velocity snapshot`, then `velocity tap @e7` | Yes: `observe`, then `act` |
| Allow-listing | One rule, such as `Bash(velocity *)` | Per tool, or the whole server |
| Set up with | `velocity mcp install` (installs the skill too) or `velocity mcp rules` | `velocity mcp install` |

Teams that already allow-list one shell command per tool tend to prefer the CLI; it costs nothing in context until a command runs. MCP suits clients without a shell. The CLI's agent commands drive Android, iOS and the web (web commands run in `velocity daemon`, which they start).

## The CLI path

The skill `velocity mcp install` puts in `~/.claude/skills/velocity/` (or `~/.agents/skills/velocity/` for Codex) teaches the workflow; `velocity mcp rules` prints it for other clients. The commands an agent needs:

```bash
velocity doctor --json                        # is the machine ready? (adb, Xcode, devices, keys)
velocity run tests/ --report json             # run; results in velocity-results/results.json
velocity explore "open Settings > Network" \
  --app-id com.android.settings --json        # an agent writes a YAML test and replays it with no model
velocity trace view velocity-results          # the failed test's steps, screenshots, video and logs
velocity snapshot                             # drive by hand: one line per element, each with a ref
velocity tap @e7                              # act; prints what changed on screen
velocity export --out velocity/flow.yaml      # the session as a test that replays with no model
```

`velocity skills get cli` prints the guide for the agent commands, matched to the installed binary ([CLI.md](CLI.md#agent-commands)). `velocity skills get yaml` is a short guide to writing tests, and `velocity skills get mcp-tools` says what each MCP core tool changes, returns and whether a call is safe to repeat. Failures carry a code and a retry rule (`STALE_REF`, `retry: no`, `next: take a new snapshot`) so an agent branches on fields, not prose.

- **Exit codes** say what happened without reading the output: 0 passed, 1 a test failed, 2 a usage or config error, 3 the device or infrastructure, 4 the suite timeout, 5 an internal error ([CLI.md](CLI.md#exit-codes)).
- **`results.json`** has every test, its status, its error and its failed step. `--report github` adds annotations on a PR.
- **Secrets** stay out of the agent's context: write `${LOGIN_PASSWORD}` in YAML and set the variable in the environment. They are masked in every report.
- **`velocity explore`** is the agent loop on top of the executor. The model picks an operation and an element number; code builds the selector and checks every value. The test it writes replays with no model ([EXPLORE.md](EXPLORE.md)).

## The MCP path

`velocity mcp` serves five tools by default ([MCP.md](MCP.md#core-tools)):

- **`observe`** reads the screen as one line per element, each with a ref like `@e12`.
- **`act`** does one thing (tap, type, swipe, scroll, press a key, launch) and returns what changed on screen.
- **`run`** runs a suite or explores a goal in the background.
- **`artifacts`** explains the last failure, shows a trace, or exports the session as a YAML suite.
- **`verify_change`** checks that a change does what its description says and leaves PR evidence (`velocity verify-change`).

`--profile full` lists all 32 individual tools instead ([MCP.md](MCP.md#full-tools)), and `--read-only` keeps only the tools that look.

## What either path never does

- **Model output never becomes a selector or a typed value directly.** The model names an element by number, and code builds the selector. A value is typed only when it is named (`${NAME}`), or, with `explore --values-from-goal` / `--values-from-screen`, when code finds the text in the goal or on a screen it saw ([EXPLORE.md](EXPLORE.md#--values-from-goal-and---values-from-screen-off-by-default)).
- **Secrets never reach the model.** A secret is typed from the environment by name. It is redacted from every report, event and MCP reply.
- **A path the model chooses stays in the project.** MCP refuses an absolute path, `..` or a symlink out of the working directory.
