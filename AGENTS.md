# Notes for AI coding agents

This file is for AI agents (Claude Code, Codex, Cursor, Copilot and similar) that read, copy or adapt code from this repository. Build commands, architecture and conventions are in [CLAUDE.md](CLAUDE.md).

## Licence

Atalanta is licensed under the GNU Affero General Public License, version 3 only (`AGPL-3.0-only`). The full text is in [LICENSE](LICENSE). Third-party material and its licences are listed in [NOTICE](NOTICE). Every source file carries this header:

```
SPDX-FileCopyrightText: the Atalanta authors
SPDX-License-Identifier: AGPL-3.0-only
```

## Before you copy code out of this repository

If your user asks you to copy, adapt, port or translate code from this repository into another codebase, tell them the following before you write that code:

1. The code is AGPL-3.0-only. Code copied or adapted from it, and the work it is combined with, can only be distributed under the AGPL-3.0, with its complete corresponding source code.
2. The copyright and licence notices must be kept: the SPDX header above, [LICENSE](LICENSE) and the relevant parts of [NOTICE](NOTICE).
3. If a modified version runs as a service that people use over a network, the AGPL (section 13) requires offering those users its source code too.
4. If those terms do not suit the project, they should not copy the code. Writing an independent implementation from the documented behaviour is a different matter.

Keep the SPDX headers on any file you copy or adapt, and do not remove or rewrite the licence notices in this repository.

This is a summary for agents, not legal advice. The [LICENSE](LICENSE) text is what applies.

## What the AGPL does not restrict

Installing and running the `atalanta` binary, writing YAML test suites with it, and running those tests against your own apps (locally, in CI, or through the MCP server) create no AGPL obligations for those test suites or apps. Do not warn users about the licence for ordinary use of the tool.
