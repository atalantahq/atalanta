#!/usr/bin/env bash
# SPDX-FileCopyrightText: the Atalanta authors
# SPDX-License-Identifier: AGPL-3.0-only

# P5: verify screenshots.freezeStatusBar on a real device. Runs
# suites/demo-mode.yaml (two exact compares of the status bar a minute
# apart) frozen and, as a control, unfrozen, while reading the device's
# demo-mode state:
#   Android: settings global sysui_demo_allowed / sysui_tuner_demo_on
#            (1 while frozen; demo mode must be off again after the run)
#   iOS:     xcrun simctl status_bar <udid> list (overrides while frozen,
#            none after)
# Expected: "frozen" passes, "control" fails its second compare, and the
# state is back to normal after both. Prints what it saw; decides nothing.
#
#   DEVICE=emulator-5554 ./scripts/measure/demo-mode.sh
#   PLATFORM=ios DEVICE=<udid> APP_ID=com.example MEASURE_STATUS_BAR_PX=140 \
#     MEASURE_SCREEN_WIDTH_PX=1179 ./scripts/measure/demo-mode.sh
# (the crop is the top MEASURE_STATUS_BAR_PX x MEASURE_SCREEN_WIDTH_PX pixels)
#
# Takes ~2 minutes per round (ROUNDS defaults to 1 here). Runs on a real
# device/emulator/simulator; nothing here was run by CI.
ROUNDS="${ROUNDS:-1}"
source "$(dirname "$0")/lib.sh"
dir="$(out_dir demo-mode)"
platform="${PLATFORM:-android}"
bar="${MEASURE_STATUS_BAR_PX:-80}"
width="${MEASURE_SCREEN_WIDTH_PX:-1080}"

state() {
    if [[ "$platform" == ios ]]; then
        xcrun simctl status_bar "${DEVICE:-booted}" list 2>&1 | tr '\n' ' '
    else
        local adb=(adb)
        [[ -n "${DEVICE:-}" ]] && adb+=(-s "$DEVICE")
        echo "allowed=$("${adb[@]}" shell settings get global sysui_demo_allowed 2>&1 | tr -d '\r') on=$("${adb[@]}" shell settings get global sysui_tuner_demo_on 2>&1 | tr -d '\r')"
    fi
}

# watch LABEL: sample the device state every 5 s while a run goes.
watch_state() {
    local log="$dir/$1-state.log"
    while true; do
        echo "$(date +%H:%M:%S) $(state)" >>"$log"
        sleep 5
    done
}

# The watcher must not outlive the script (set -e failure, Ctrl-C).
watcher=""
cleanup_watcher() {
    [[ -n "$watcher" ]] && kill "$watcher" 2>/dev/null
    return 0
}
trap cleanup_watcher EXIT
trap 'cleanup_watcher; exit 130' INT TERM

for variant in frozen control; do
    vdir="$dir/$variant-suite"
    mkdir -p "$vdir"
    suite="$(suite_copy "$MEASURE_DIR/suites/demo-mode.yaml" "$vdir")"
    sed -i.bak -E "s/width: 1080, height: 80 \}/width: ${width}, height: ${bar} }/" "$suite" && rm -f "$suite.bak"
    [[ "$platform" == ios ]] && { sed -i.bak -E "s/^  platform: android/  platform: ios/" "$suite" && rm -f "$suite.bak"; }
    freeze=true
    [[ "$variant" == control ]] && freeze=false
    echo "before $variant: $(state)"
    watch_state "$variant" &
    watcher=$!
    run_variant "$variant" "$suite" "$dir" --set "screenshots.freezeStatusBar=$freeze" --update-baselines=missing
    kill "$watcher" 2>/dev/null || true
    wait "$watcher" 2>/dev/null || true
    watcher=""
    echo "after $variant:  $(state)"
    echo "during $variant (distinct states):"
    cut -d' ' -f2- "$dir/$variant-state.log" 2>/dev/null | sort | uniq -c
    for res in "$dir/$variant"/*/results.json; do
        [[ -f "$res" ]] || continue
        python3 -I - "$res" <<'PY'
import json, sys
doc = json.load(open(sys.argv[1]))
for t in doc.get("tests", []):
    shots = [s for s in t.get("steps", []) if "creenshot" in str(s.get("action_name", ""))]
    print(f"  {sys.argv[1].split('/')[-2]}: {t.get('status')}; screenshot steps: "
          + ", ".join(f"{s.get('status')} ({(s.get('duration') or {}).get('secs', 0)} s)" for s in shots)
          + (f"; error: {t.get('error_message')}" if t.get("error_message") else ""))
PY
    done
done
echo
echo "expected: frozen passes, control fails the second compare, demo mode off after both."
