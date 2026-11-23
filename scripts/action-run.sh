#!/bin/sh
# SPDX-FileCopyrightText: the Atalanta authors
# SPDX-License-Identifier: AGPL-3.0-only

# Runs Atalanta for the GitHub Action (action.yml). Inside the Android
# emulator step it proves the emulator booted, so the next step can tell a
# boot failure (infrastructure, exit 3) from a failing test.
#
# Environment: ATALANTA_ACTION_STATE (a folder for the markers), SUITE,
# SHARD (M/N, optional), RESULTS_DIR, EXTRA_ARGS (split on spaces).
# Writes $ATALANTA_ACTION_STATE/booted, then $ATALANTA_ACTION_STATE/exit.
# Exits 0 itself: the outcome is in the exit file.
set -u
mkdir -p "$ATALANTA_ACTION_STATE"
: > "$ATALANTA_ACTION_STATE/booted"
set -- run "$SUITE" --report junit,json,github --artifacts-dir "${RESULTS_DIR:-atalanta-results}"
[ -n "${SHARD:-}" ] && set -- "$@" --shard "$SHARD"
# shellcheck disable=SC2086 # EXTRA_ARGS is a list of flags on purpose
atalanta "$@" ${EXTRA_ARGS:-}
echo $? > "$ATALANTA_ACTION_STATE/exit"
exit 0
