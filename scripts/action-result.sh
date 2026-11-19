#!/bin/sh
# The GitHub Action's last step (action.yml): Velocity's exit code, or 3
# (infrastructure) when the device never came up, so a pipeline can retry
# 3 and treat everything else as the tests' verdict. Reads the markers
# scripts/action-run.sh wrote under $VELOCITY_ACTION_STATE.
set -u
state="$VELOCITY_ACTION_STATE"
if [ ! -f "$state/booted" ]; then
    echo "::error title=Velocity::the device never came up (boot failed or timed out${BOOT_ATTEMPTS:+ after $BOOT_ATTEMPTS attempt(s)}); this is an infrastructure failure, exit 3, not a test result"
    exit 3
fi
if [ ! -f "$state/exit" ]; then
    echo "::error title=Velocity::the device booted but Velocity never finished (the emulator died or the step was killed); infrastructure, exit 3"
    exit 3
fi
code=$(cat "$state/exit")
case $code in
    0) echo "Velocity passed." ;;
    3) echo "::error title=Velocity::device or infrastructure problem (exit 3)" ;;
    *) echo "Velocity exited $code (1 test failure, 2 config, 4 suite timeout, 5 internal; docs/CLI.md#exit-codes)" ;;
esac
exit "$code"
