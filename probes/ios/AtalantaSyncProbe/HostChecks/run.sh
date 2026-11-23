#!/usr/bin/env bash
# SPDX-FileCopyrightText: the Atalanta authors
# SPDX-License-Identifier: AGPL-3.0-only

# Host (macOS) checks for AtalantaSyncProbe: compiles the UIKit-free sources
# with swiftc and runs them. No simulator or Xcode project needed.
#   ./run.sh                 run the checks
#   ./run.sh --regression    only the timer regression (works on old sources)
set -euo pipefail
cd "$(dirname "$0")"
OUT="${OUT:-$(mktemp -d)}"
SRC=../Sources
SDK="$(xcrun --show-sdk-path)"
# The UIKit-free sources AppStateTracker needs (older trees lack some).
DEPS=()
for f in DelayedWorkTracker ReactNativeTracker; do
    [[ -f "$SRC/$f.swift" ]] && DEPS+=("$SRC/$f.swift")
done
if [[ "${1:-}" == "--regression" ]]; then
    printf 'import Foundation\nexit(timerRegression() ? 0 : 1)\n' > "$OUT/main.swift"
    swiftc -sdk "$SDK" -swift-version 5 -suppress-warnings -o "$OUT/regression" \
        "$SRC/AppStateTracker.swift" "$SRC/TimerTracker.swift" ${DEPS[@]+"${DEPS[@]}"} TimerRegression.swift "$OUT/main.swift"
    exec "$OUT/regression"
fi
swiftc -sdk "$SDK" -swift-version 5 -suppress-warnings -o "$OUT/hostchecks" \
    "$SRC/AppStateTracker.swift" "$SRC/TimerTracker.swift" "$SRC/IdleWait.swift" \
    "$SRC/URLBlocklist.swift" "$SRC/NetworkTracker.swift" "$SRC/SyncServer.swift" \
    ${DEPS[@]+"${DEPS[@]}"} TimerRegression.swift ProbeHookChecks.swift main.swift
exec "$OUT/hostchecks"
