---
name: docs-sync
description: Keep Atalanta's documentation in step with a code change, and add or move a docs page — which doc files tests compare with the code (DSL-REFERENCE.md, llms.txt, MCP.md, CLI.md flags, EXPLORE.md flags, the tool counts in README.md and CONTEXT.md, every YAML example), how to regenerate the generated ones with ATALANTA_BLESS, the site registry in site/src/lib/docs-config.ts, the docs/README.md table, the changelog line and the docs/STATUS.md row. Use after any user-visible change (a flag, a step, a tool, a default, an exit code), when adding a docs/*.md page, or when the Docs workflow, dsl_reference, llms_txt, doc_examples or the site build fails.
---

# Docs in step with the code

The Markdown under `docs/` and at the repository root is the source: GitHub renders it and `site/` (Astro) builds the website from the same files, so a page has to read correctly in both. Much of it is checked against the code by tests. The rest is not, and drifts unless the change that alters behaviour also alters the page.

## What a change owes

| The change | Update |
|---|---|
| any user-visible behaviour | one line in `CHANGELOG.md` under `## [Unreleased]` |
| a DSL step, selector key or setting | `docs/DSL.md`; regenerate `docs/DSL-REFERENCE.md` (see `add-dsl-step`) |
| a CLI flag or command | `docs/CLI.md` (an `mcp` flag may live in `docs/MCP.md` instead; an `explore` flag goes in `docs/EXPLORE.md` as well) |
| an MCP tool | `docs/MCP.md` and the counts in `README.md`, `CONTEXT.md`, `docs/AGENTS.md` (see `add-mcp-tool`) |
| an exit code or an artifact file | the tables at the end of `docs/CLI.md`, and `docs/CI.md` |
| a driver capability | `docs/DRIVERS.md` and a row in `docs/STATUS.md` |
| crate layout, a new module with a rule attached | `CLAUDE.md` and `CONTEXT.md` (architecture), `docs/ARCHITECTURE.md` |
| an `ATALANTA_*` variable | where the feature is documented; `atalanta settings` lists the named ones |

## Guarded by tests

| File | Test | When it fails |
|---|---|---|
| `docs/DSL-REFERENCE.md` | `atalanta-core`, `--test dsl_reference` | **Generated.** `ATALANTA_BLESS=1 cargo test -p atalanta-core --test dsl_reference` |
| `docs/llms.txt` | `atalanta-core`, `--test llms_txt` | **Generated** from the table in `docs/README.md`. `ATALANTA_BLESS=1 cargo test -p atalanta-core --test llms_txt` |
| YAML in `README.md` and `docs/*.md` | `atalanta-core`, `--test doc_examples` | Every `yaml` block that is a whole suite (`appId:` and `tests:`) must parse and validate. Fix the example, or the parser if the example is right |
| `docs/MCP.md` | `the_mcp_doc_lists_every_tool` (`atalanta-mcp-server`) | A ``### `tool` `` section per tool; "Read-only." exactly for read-only tools |
| `README.md`, `CONTEXT.md` | `the_docs_give_the_real_tool_counts` | The number of full-profile tools |
| `docs/CLI.md`, `docs/MCP.md` | `every_long_flag_of_the_main_commands_is_documented` (`atalanta-cli`) | Every long flag of `run`, `explore`, `gate`, `init`, `doctor`, `mcp` appears as `--flag` |
| `docs/EXPLORE.md` | `every_explore_flag_is_on_the_explore_page` (`atalanta-explore`, `--test docs`) | Every `explore` flag |
| `CLAUDE.md` | `claude_md_names_this_protocol` (`atalanta-android`) | "(protocol N:" matches the tree server |
| `docs/licenses/THIRD-PARTY-RUST.md` | `python3 -I scripts/third-party-rust.py --check` | **Generated.** `--write` (see `deps`) |

Never hand-edit a generated file, and never bless one without reading the diff: a bless that changes lines you did not expect means the code changed something you did not intend.

Not guarded, so check by reading: the exit-code tables, the command table at the top of `docs/CLI.md`, `README.md`'s command list, `docs/CI.md`, `docs/DRIVERS.md`, `docs/GUIDE.md`, the guides under `crates/atalanta-mcp/assets/skills/`. `grep -rn '<old name>' README.md docs crates/atalanta-mcp/assets` after a rename.

## A new page

1. Write `docs/NAME.md`. The first paragraph says who the page is for and what it covers. Link other pages with relative links (`[CLI](CLI.md#run)`); they work on GitHub and the site rewrites them.
2. Register it in `site/src/lib/docs-config.ts`: an entry `{ label, slug, source: 'docs/NAME.md' }` in the right group of `GROUPS`, or the path in `IGNORED` if it should not be on the site. An unregistered `docs/*.md` fails the site build.
3. Add a row `| [NAME.md](NAME.md) | What's in it |` to the table in `docs/README.md`, then regenerate `docs/llms.txt` (above).
4. Build the site.

## Build the site

```bash
cd site && npm ci && npm run build      # astro build, Pagefind index, then anchor warnings
npm run dev                             # live preview (search needs a built index: npm run preview)
```

Node 24, as in CI. The build fails on a relative link to a file that does not exist, a link that leaves the repository, and an image missing from `site/public`. A link to a missing `#anchor` only warns, at the end of the build: read the warnings and fix them, since a renamed heading breaks every link to it silently.

Before the site build, the Docs workflow runs:

```bash
cargo test -p atalanta-core --locked --test doc_examples --test dsl_reference --test llms_txt
cargo test -p atalanta-mcp-server --locked --lib the_mcp_doc_lists_every_tool
```

## Changelog

One bullet per user-visible change under `## [Unreleased]`, in the matching `###` section (`Added` has `####` groups: Commands, Suite DSL, Running and CI, Failure evidence, Platforms, MCP and console). Present tense, stating the fact; name flags and keys in backticks and link the page that documents them. The reasoning belongs in the commit message.

A change that can make an existing passing suite fail goes under `### Behaviour changes that can fail existing suites` and starts `**[may fail suites]** **Bold summary.**`, followed by what changed, how to tell, and the fix or opt-out.

Do not rename `## [Unreleased]` or touch the version headings and the link definitions at the bottom: `scripts/bump-version.sh` and `scripts/changelog-section.sh` depend on their exact form.

## Status

`docs/STATUS.md` says where each feature has actually run. The states: **Hardware** (a real phone or tablet), **Verified** (an Android emulator, an iOS simulator or real headless Chrome), **Host tests** (fakes or mock servers on the development machine), **Not run**. Rows are `| Feature | State | Not yet verified |`, with `—` in the last column when nothing is left.

A feature tested only against fakes is "Host tests", whatever the unit tests say. Move a row to "Verified" only after running it, and name the platform in parentheses. Update the "last updated" date at the top.

## Writing

Say what the thing does and what the reader types. Lead with the common case; put limits where the reader will hit them. No "simply", no "powerful", no future plans presented as features: if it has not run on a device, the sentence says so and `docs/STATUS.md` has the row.
