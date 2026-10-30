#!/usr/bin/env bash
# P5: what the pre-action checks cost per action. Runs
# suites/strict-actionability.yaml ROUNDS times in three variants and
# prints each tap's median time side by side:
#   default      selectors.strict=0, actionableWaitMs default (2000)
#   strict       selectors.strict=1 (a selector matching several elements fails)
#   no-wait      actionableWaitMs=0 (act without waiting for enabled/still)
# default minus no-wait is the actionability wait's cost; strict minus
# default is the strict-selector check's.
#
#   APP_ID=com.example MEASURE_TAP="Orders" MEASURE_BACK_TO="Home" ./scripts/measure/strict-actionability.sh
#
# Runs on a real device/emulator/simulator; nothing here was run by CI.
source "$(dirname "$0")/lib.sh"
dir="$(out_dir strict-actionability)"
suite="$(suite_copy "$MEASURE_DIR/suites/strict-actionability.yaml" "$dir")"
run_variant default "$suite" "$dir" --set selectors.strict=0
run_variant strict "$suite" "$dir" --set selectors.strict=1
run_variant no-wait "$suite" "$dir" --set selectors.strict=0 --set actionableWaitMs=0
summarize "$dir" default strict no-wait --steps 'tap'
