#!/usr/bin/env bash
# P5: what reading Compose testTags (android.composeTestTag) costs per full
# tree dump. Runs suites/compose-testtag.yaml ROUNDS times with the setting
# on and off and prints each assertVisible's median time side by side; the
# difference per step is the cost per dump (the first dump reads every
# node's tag, later ones hit the per-node cache).
#
#   APP_ID=com.example.compose MEASURE_TEXT="Home" ./scripts/measure/compose-testtag.sh
#
# Android only. Runs on a real device/emulator; nothing here was run by CI.
source "$(dirname "$0")/lib.sh"
dir="$(out_dir compose-testtag)"
suite="$(suite_copy "$MEASURE_DIR/suites/compose-testtag.yaml" "$dir")"
run_variant tags-off "$suite" "$dir" --set android.composeTestTag=0
run_variant tags-on "$suite" "$dir" --set android.composeTestTag=1
summarize "$dir" tags-off tags-on --steps 'assert'
echo
echo "cost per dump = tags-on minus tags-off on each assert step (first one: uncached)."
