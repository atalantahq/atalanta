# Atalanta docs

| Page | What's in it |
|---|---|
| [GUIDE.md](GUIDE.md) | The long form of the README: a glossary, how it compares, selectors and roles, natural-language steps, visual checks, healing, sharding and parallel runs, app state and caching, headless, configuration, a command cheat sheet, troubleshooting |
| [ARCHITECTURE.md](ARCHITECTURE.md) | Crates, how a run and a step execute, live events, sync, selectors, dialogs, healing, natural-language steps, exploration and diagnostics, visual assertions, the console |
| [DSL.md](DSL.md) | How to write suites, with examples |
| [DSL-REFERENCE.md](DSL-REFERENCE.md) | Every action, modifier, selector key, config key and setting. Generated from the schema (`atalanta schema --markdown`); a test fails when it is out of date |
| [DRIVERS.md](DRIVERS.md) | Android, iOS (simulators; real iPhones are coded but unverified), web, headless, Flutter, the in-app probes |
| [CLI.md](CLI.md) | Commands, `init`, `doctor`, `run`, `explore` and `inspect` flags, artifacts, failure evidence, exit codes |
| [CI.md](CI.md) | Sharding and shared history, cancelled jobs, secrets, baselines, the opt-in device workflows |
| [AI-PROVIDERS.md](AI-PROVIDERS.md) | Model providers for natural-language steps, `verify:` checks and `atalanta explore`: presets, environment variables, default models, capabilities, fallbacks |
| [EXPLORE.md](EXPLORE.md) | `atalanta explore`: an AI agent reaches a goal once and writes a deterministic test with assertions that replays with no model; the loop, the YAML rules, replay and repair, the library API |
| [BENCHMARKS.md](BENCHMARKS.md) | Public benchmarks Atalanta runs on: AndroidWorld (how the numbers were produced, how to run it, what is measured, the result, caveats) |
| [AGENTS.md](AGENTS.md) | Atalanta from a coding agent: the CLI and skill path next to MCP, what each costs, and what neither ever does |
| [MCP.md](MCP.md) | The MCP server, `mcp install`, background tasks (suite runs and explorations), its five core tools and the full ones (a test checks that every tool is listed) |
| [STATUS.md](STATUS.md) | What has been verified on emulators, simulators, browsers and hardware, and what has not |
| [research/](research/README.md) | Design rationale and the literature behind sync, healing, NL steps and scheduling |

`rust-atalanta.html` is a tutorial that teaches Rust through this codebase. It is not a reference, and its code excerpts may lag behind the code.
