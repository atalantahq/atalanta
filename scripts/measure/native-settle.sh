#!/usr/bin/env bash
# SPDX-FileCopyrightText: the Atalanta authors
# SPDX-License-Identifier: AGPL-3.0-only

# P5: settle time per action with the in-app sync probe against tree
# polling and no sync. Runs suites/native-settle.yaml ROUNDS times with
# sync.mode native, polling and none, and prints each step's median
# duration and [settleMs] side by side.
#
#   APP_ID=com.example.app MEASURE_TAP="Orders" MEASURE_BACK_TO="Home" \
#     FRAMEWORK=react_native ./scripts/measure/native-settle.sh
#
# Needs an app built with the sync probe for "native" to mean anything:
# without one the run falls back to polling and its log says so (checked
# below). FRAMEWORK=react_native turns on the probe's React Native trackers.
# Runs on a real device/emulator/simulator; nothing here was run by CI.
source "$(dirname "$0")/lib.sh"
dir="$(out_dir native-settle)"
suite="$(suite_copy "$MEASURE_DIR/suites/native-settle.yaml" "$dir")"
if [[ -n "${FRAMEWORK:-}" ]]; then
    # `framework:` under config: (sent to the probe in its config).
    sed -i.bak -E "s/^config:\$/config:\n  framework: ${FRAMEWORK}/" "$suite" && rm -f "$suite.bak"
fi
[[ "${PLATFORM:-}" == ios ]] && { sed -i.bak -E "s/^  platform: android/  platform: ios/" "$suite" && rm -f "$suite.bak"; }
run_variant native "$suite" "$dir" --set sync.mode=native
run_variant polling "$suite" "$dir" --set sync.mode=polling
run_variant none "$suite" "$dir" --set sync.mode=none
summarize "$dir" native polling none
echo
if grep -l -i "native sync probe\|probe" "$dir"/native/*/stdout.log >/dev/null 2>&1; then
    echo "probe messages from the native runs (an app without the probe falls back to polling):"
    grep -h -i "native sync probe\|sync probe" "$dir"/native/*/stdout.log | sort | uniq -c | head -20
fi
