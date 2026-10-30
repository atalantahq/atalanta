#!/usr/bin/env bash
# Velocity vs Maestro, head to head, on one Android emulator and the Settings
# app. See README.md for the rules. Usage:
#
#   bench/head-to-head/run.sh [-n RUNS] [-d SERIAL] [-o OUT_DIR]
#
# Environment overrides: VELOCITY_BIN, MAESTRO_BIN, MAESTRO_VERSION, API_LEVEL.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"

RUNS=5
DEVICE="${ANDROID_SERIAL:-}"
OUT=""
VELOCITY_BIN="${VELOCITY_BIN:-$REPO/target/release/velocity}"
MAESTRO_BIN="${MAESTRO_BIN:-maestro}"
MAESTRO_VERSION="${MAESTRO_VERSION:-2.3.0}"   # pinned; change only on purpose
API_LEVEL="${API_LEVEL:-34}"
APP_ID="com.android.settings"
VELOCITY_FLOW="$HERE/flows/settings.velocity.yaml"
MAESTRO_FLOW="$HERE/flows/settings.maestro.yaml"

usage() { sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//'; }

while getopts "n:d:o:h" opt; do
  case "$opt" in
    n) RUNS="$OPTARG" ;;
    d) DEVICE="$OPTARG" ;;
    o) OUT="$OPTARG" ;;
    h) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
done

die() { echo "error: $*" >&2; exit 1; }
log() { echo "[head-to-head] $*" >&2; }

# ---------------------------------------------------------------- prerequisites
[[ "$RUNS" =~ ^[1-9][0-9]*$ ]] || die "-n must be a positive integer (got '$RUNS')"
command -v adb >/dev/null || die "adb not on PATH"
command -v python3 >/dev/null || die "python3 not on PATH (summarize.py)"

[[ -x "$VELOCITY_BIN" ]] || die "no Velocity binary at $VELOCITY_BIN; build it: cargo build --release -p velocity-cli"
case "$VELOCITY_BIN" in
  */target/release/*) ;;
  *) log "warning: $VELOCITY_BIN is not under target/release; make sure it is a release build" ;;
esac
VELOCITY_VERSION="$("$VELOCITY_BIN" --version 2>/dev/null | head -1 || true)"
[[ -n "$VELOCITY_VERSION" ]] || die "$VELOCITY_BIN --version failed"

command -v "$MAESTRO_BIN" >/dev/null || die "maestro not on PATH (pinned version: $MAESTRO_VERSION)"
export MAESTRO_CLI_NO_ANALYTICS=1 MAESTRO_CLI_ANALYSIS_NOTIFICATION_DISABLED=true
got_maestro="$("$MAESTRO_BIN" --version 2>/dev/null | tail -1 | tr -d '[:space:]')"
[[ "$got_maestro" == "$MAESTRO_VERSION" ]] \
  || die "Maestro $got_maestro installed, the benchmark pins $MAESTRO_VERSION (or set MAESTRO_VERSION to compare on purpose)"

if [[ -z "$DEVICE" ]]; then
  online="$(adb devices | awk 'NR>1 && $2=="device" {print $1}')"
  count="$(printf '%s\n' "$online" | grep -c . || true)"
  [[ "$count" == 1 ]] || die "need exactly one online device or -d SERIAL (found $count)"
  DEVICE="$online"
fi
ADB=(adb -s "$DEVICE")
[[ "$("${ADB[@]}" get-state 2>/dev/null)" == "device" ]] || die "device $DEVICE is not online"
dsh() { "${ADB[@]}" shell "$@" | tr -d '\r'; }
[[ "$(dsh getprop sys.boot_completed)" == 1 ]] || die "device $DEVICE has not finished booting"
[[ "$(dsh getprop ro.kernel.qemu)" == 1 ]] || log "warning: $DEVICE does not look like an emulator"
api="$(dsh getprop ro.build.version.sdk)"
[[ "$api" == "$API_LEVEL" ]] || die "device is API $api, the benchmark wants API $API_LEVEL"
dsh pm path "$APP_ID" | grep -q '^package:' || die "$APP_ID is not installed on $DEVICE"
model="$(dsh getprop ro.product.model)"
abi="$(dsh getprop ro.product.cpu.abi)"

# -------------------------------------------------------------------- output
OUT="${OUT:-$HERE/results/$(date +%Y%m%d-%H%M%S)}"
mkdir -p "$OUT/logs"
CSV="$OUT/results.csv"
echo "phase,iteration,order,tool,wall_ms,exit_code,device,api,started_at" > "$CSV"
{
  echo "date=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "host=$(uname -sm)"
  echo "velocity=$VELOCITY_VERSION ($VELOCITY_BIN)"
  echo "maestro=$got_maestro"
  echo "device=$DEVICE model=$model abi=$abi api=$api"
  echo "runs=$RUNS"
} > "$OUT/env.txt"
log "writing to $OUT"

# ------------------------------------------------------------------- helpers
# Milliseconds since the epoch: bash 5's EPOCHREALTIME, else perl (macOS bash 3.2).
now_ms() {
  if [[ -n "${EPOCHREALTIME:-}" ]]; then
    local t="${EPOCHREALTIME/[.,]/}"; echo $(( t / 1000 ))
  else
    perl -MTime::HiRes=time -e 'printf "%d\n", time()*1000'
  fi
}

# Same starting point for every run: animations at the device default (1.0),
# so neither tool inherits the other's setting, the app stopped, home screen.
reset_device() {
  local k
  for k in window_animation_scale transition_animation_scale animator_duration_scale; do
    dsh settings put global "$k" 1.0 >/dev/null
  done
  dsh am force-stop "$APP_ID" >/dev/null
  dsh input keyevent KEYCODE_HOME >/dev/null
  sleep 2
}

run_velocity() {   # $1 = log/artifact tag
  VELOCITY_KEEP_ANIMATIONS='' "$VELOCITY_BIN" run "$VELOCITY_FLOW" \
    --device "$DEVICE" --platform android --retries 0 \
    --report junit --artifacts-dir "$OUT/artifacts/$1"
}

run_maestro() {    # $1 = log/artifact tag
  "$MAESTRO_BIN" --device "$DEVICE" test "$MAESTRO_FLOW" \
    --format junit --output "$OUT/artifacts/$1.xml"
}

# Time one run and append it to the CSV. $1 phase, $2 iteration, $3 order, $4 tool
timed() {
  local phase="$1" iter="$2" order="$3" tool="$4" tag start end rc
  tag="$phase-$iter-$tool"
  mkdir -p "$OUT/artifacts"
  reset_device
  log "$phase #$iter: $tool"
  start="$(now_ms)"
  set +e
  "run_$tool" "$tag" > "$OUT/logs/$tag.log" 2>&1
  rc=$?
  set -e
  end="$(now_ms)"
  echo "$phase,$iter,$order,$tool,$((end - start)),$rc,$DEVICE,$api,$(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "$CSV"
  (( rc == 0 )) || log "  $tool exited $rc (see $OUT/logs/$tag.log)"
}

# --------------------------------------------------------------------- runs
# Warm-up: one run of each tool, recorded as phase=warmup and ignored by
# summarize.py (Maestro installs its driver APKs, Velocity its tree server).
timed warmup 0 1 velocity
timed warmup 0 2 maestro

# Measured: N iterations, the order flipping every iteration (V,M then M,V)
# so neither tool always runs second on a warmer device.
for (( i = 1; i <= RUNS; i++ )); do
  if (( i % 2 == 1 )); then first=velocity second=maestro; else first=maestro second=velocity; fi
  timed measured "$i" 1 "$first"
  timed measured "$i" 2 "$second"
done

reset_device
log "done: $CSV"
python3 "$HERE/summarize.py" "$CSV" | tee "$OUT/summary.md"
