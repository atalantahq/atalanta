# shellcheck shell=bash
# Shared by the scripts in scripts/measure/ (sourced, not run).
#
# These scripts drive a REAL device or emulator/simulator: they are for the
# owner to run, and none of them has been run by CI or by their author.
# Each prints numbers; none of them passes or fails on a threshold.
#
# Environment:
#   ATALANTA   the atalanta binary (default: `atalanta` on PATH, else
#              target/release/atalanta of this repo)
#   DEVICE     device id passed as --device (default: the first one)
#   PLATFORM   android | ios (default: from the suite)
#   APP_ID     app under test: replaces the suite's appId (and launchApp ids)
#   ROUNDS     runs per variant (default 5)
#   OUT        where results go (default: ./atalanta-measure/<script>-<time>)

set -euo pipefail

MEASURE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$MEASURE_DIR/../.." && pwd)"
ROUNDS="${ROUNDS:-5}"

atalanta_bin() {
    if [[ -n "${ATALANTA:-}" ]]; then
        echo "$ATALANTA"
    elif command -v atalanta >/dev/null 2>&1; then
        command -v atalanta
    elif [[ -x "$REPO_ROOT/target/release/atalanta" ]]; then
        echo "$REPO_ROOT/target/release/atalanta"
    else
        echo "atalanta not found: set ATALANTA or build with 'cargo build --release -p atalanta-cli'" >&2
        return 1
    fi
}

# out_dir NAME: a fresh results folder for this script.
out_dir() {
    local dir="${OUT:-./atalanta-measure/$1-$(date +%Y%m%d-%H%M%S)}"
    mkdir -p "$dir"
    (cd "$dir" && pwd)
}

# suite_copy SUITE DIR: the suite, with APP_ID substituted when set.
suite_copy() {
    local src="$1" dir="$2" dst
    dst="$dir/$(basename "$src")"
    if [[ -n "${APP_ID:-}" ]]; then
        # appId: and launchApp's appId: lines only.
        sed -E "s/^(appId:).*/\1 ${APP_ID}/; s/^([[:space:]]+appId:).*/\1 ${APP_ID}/" "$src" > "$dst"
    else
        cp "$src" "$dst"
    fi
    echo "$dst"
}

# run_variant LABEL SUITE DIR [--set k=v ...]: ROUNDS runs, each into
# DIR/LABEL/<n>/results.json. A failed run is kept (its steps still count)
# and noted.
run_variant() {
    local label="$1" suite="$2" dir="$3"
    shift 3
    local bin
    bin="$(atalanta_bin)"
    local args=(run "$suite" --report json --retries 0 --trace off)
    [[ -n "${DEVICE:-}" ]] && args+=(--device "$DEVICE")
    [[ -n "${PLATFORM:-}" ]] && args+=(--platform "$PLATFORM")
    for ((i = 1; i <= ROUNDS; i++)); do
        local out="$dir/$label/$i"
        mkdir -p "$out"
        echo "== $label round $i/$ROUNDS" >&2
        if ! "$bin" "${args[@]}" --artifacts-dir "$out" "$@" >"$out/stdout.log" 2>&1; then
            echo "   (run $label/$i failed; see $out/stdout.log)" >&2
        fi
    done
}

# summarize DIR LABEL...: per-step timings of each variant, side by side.
summarize() {
    local dir="$1"
    shift
    python3 -I "$MEASURE_DIR/summarize.py" "$dir" "$@"
}
