#!/bin/sh
# SPDX-FileCopyrightText: the Atalanta authors
# SPDX-License-Identifier: AGPL-3.0-only

# The GitHub Action's last step (action.yml): Atalanta's exit code, or 3
# (infrastructure) when the device never came up, so a pipeline can retry
# 3 and treat everything else as the tests' verdict. Reads the markers
# scripts/action-run.sh wrote under $ATALANTA_ACTION_STATE.
set -u
state="$ATALANTA_ACTION_STATE"
if [ ! -f "$state/booted" ]; then
    echo "::error title=Atalanta::the device never came up (boot failed or timed out${BOOT_ATTEMPTS:+ after $BOOT_ATTEMPTS attempt(s)}); this is an infrastructure failure, exit 3, not a test result"
    exit 3
fi
if [ ! -f "$state/exit" ]; then
    echo "::error title=Atalanta::the device booted but Atalanta never finished (the emulator died or the step was killed); infrastructure, exit 3"
    exit 3
fi
code=$(cat "$state/exit")
case $code in
    0) echo "Atalanta passed." ;;
    3) echo "::error title=Atalanta::device or infrastructure problem (exit 3)" ;;
    *) echo "Atalanta exited $code (1 test failure, 2 config, 4 suite timeout, 5 internal; docs/CLI.md#exit-codes)" ;;
esac
exit "$code"
