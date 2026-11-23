#!/usr/bin/env bash
# SPDX-FileCopyrightText: the Atalanta authors
# SPDX-License-Identifier: AGPL-3.0-only

# Opt in to Atalanta's git hooks (scripts/hooks/): pre-commit runs gitleaks
# on the staged changes plus the fast format/lint/unsafe checks; pre-push
# runs gitleaks on the commits being pushed and, when a lockfile changed,
# the dependency checks. Tools that are not installed are skipped.
#
#   scripts/install-hooks.sh            # use scripts/hooks for this clone
#   scripts/install-hooks.sh --remove   # back to .git/hooks
#
# It sets this clone's core.hooksPath, so hooks you keep in .git/hooks stop
# running while it is set.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
if [ "${1:-}" = "--remove" ]; then
    git config --local --unset core.hooksPath || true
    echo "core.hooksPath unset: git uses .git/hooks again"
    exit 0
fi
chmod +x scripts/hooks/pre-commit scripts/hooks/pre-push
git config --local core.hooksPath scripts/hooks
echo "git hooks enabled from scripts/hooks (pre-commit, pre-push); undo with $0 --remove"
