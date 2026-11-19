#!/bin/sh
# Runs Velocity for the GitHub Action (action.yml). Inside the Android
# emulator step it proves the emulator booted, so the next step can tell a
# boot failure (infrastructure, exit 3) from a failing test.
#
# Environment: VELOCITY_ACTION_STATE (a folder for the markers), SUITE,
# SHARD (M/N, optional), RESULTS_DIR, EXTRA_ARGS (split on spaces).
# Writes $VELOCITY_ACTION_STATE/booted, then $VELOCITY_ACTION_STATE/exit.
# Exits 0 itself: the outcome is in the exit file.
set -u
mkdir -p "$VELOCITY_ACTION_STATE"
: > "$VELOCITY_ACTION_STATE/booted"
set -- run "$SUITE" --report junit,json,github --artifacts-dir "${RESULTS_DIR:-velocity-results}"
[ -n "${SHARD:-}" ] && set -- "$@" --shard "$SHARD"
# shellcheck disable=SC2086 # EXTRA_ARGS is a list of flags on purpose
velocity "$@" ${EXTRA_ARGS:-}
echo $? > "$VELOCITY_ACTION_STATE/exit"
exit 0
