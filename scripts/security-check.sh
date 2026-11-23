#!/usr/bin/env bash
# Run Atalanta's security checks locally, the same ones
# .github/workflows/security.yml runs (it calls this script, one check per
# job, so the flags and allowlists live in one place).
#
# Usage: scripts/security-check.sh [check ...]
#
#   deny       cargo-deny: RustSec advisories, licences, bans, sources (deny.toml)
#   osv        OSV-Scanner over every lockfile in the tree (osv-scanner.toml)
#   npm-audit  npm audit --audit-level=high for each package-lock.json,
#              production and dev dependencies reported separately
#   gitleaks   secrets in the whole git history and in uncommitted changes
#              (.gitleaks.toml)
#   zizmor     GitHub Actions workflow and action.yml audit
#   semgrep    SAST, Semgrep OSS rule packs (.semgrepignore, `nosemgrep:`)
#   unsafe     `unsafe` count per crate against scripts/unsafe-baseline.txt
#
# With no check named, all of the above run. Opt-in extras:
#   sbom       CycloneDX SBOMs into $SECURITY_REPORT_DIR/sbom (scripts/sbom.sh)
#   fuzz       every cargo-fuzz target for $FUZZ_SECONDS (default 30) each;
#              needs a nightly toolchain and cargo-fuzz
#
# A check whose tool is not installed is skipped with the command that
# installs it. Exit status: 0 when every check that ran passed, 1 when any
# found something, 2 for a usage error. SECURITY_STRICT=1 (CI) turns a
# skipped check into a failure.
#
# Env: SECURITY_REPORT_DIR  where JSON/SARIF reports go (default target/security)
#      DENY_CHECKS          cargo-deny checks (default "advisories bans licenses sources")
#      SEMGREP              the semgrep command (default "semgrep"; CI runs the
#                           digest-pinned container through it)
#      GH_TOKEN             lets zizmor run its online audits (impostor commits,
#                           known-vulnerable actions); offline without it
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

REPORTS=${SECURITY_REPORT_DIR:-$ROOT/target/security}
STRICT=${SECURITY_STRICT:-0}
mkdir -p "$REPORTS"

ALL_CHECKS="deny osv npm-audit gitleaks zizmor semgrep unsafe"
failed=()
skipped=()
passed=()

say() { printf '\n==> %s\n' "$*"; }

# One line per check in the GitHub job summary, when there is one.
summary() {
    if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
        printf '%s\n' "$*" >> "$GITHUB_STEP_SUMMARY"
    fi
}

# have TOOL INSTALL-HINT: true when TOOL is on PATH; otherwise record a skip.
have() {
    if command -v "$1" > /dev/null 2>&1; then
        return 0
    fi
    echo "skipped: $1 is not installed. Install it with: $2"
    skipped+=("$CHECK ($1 missing)")
    summary "- :warning: **$CHECK**: skipped, \`$1\` not installed"
    return 1
}

pass() { passed+=("$CHECK"); summary "- :white_check_mark: **$CHECK**: passed"; }
fail() { failed+=("$CHECK"); summary "- :x: **$CHECK**: $*"; }

check_deny() {
    have cargo-deny "cargo install --locked cargo-deny  (or: brew install cargo-deny)" || return 0
    # shellcheck disable=SC2086 # a list of check names
    if cargo deny check ${DENY_CHECKS:-advisories bans licenses sources}; then
        pass
    else
        fail "cargo deny reported problems (see the log)"
    fi
}

check_osv() {
    have osv-scanner "brew install osv-scanner  (or: go install github.com/google/osv-scanner/v2/cmd/osv-scanner@latest)" || return 0
    # Every lockfile under the tree (Cargo.lock, fuzz/Cargo.lock,
    # package-lock.json, ...); .gitignore'd folders are skipped.
    osv-scanner scan source --config osv-scanner.toml --format sarif \
        --output "$REPORTS/osv.sarif" -r . > /dev/null 2>&1 || true
    if osv-scanner scan source --config osv-scanner.toml -r .; then
        pass
    else
        fail "known vulnerabilities in a lockfile (see the log, osv.sarif)"
    fi
}

check_npm_audit() {
    have npm "brew install node" || return 0
    local lock dir bad=0
    while IFS= read -r lock; do
        dir=$(dirname "$lock")
        echo "--- $dir: production dependencies"
        if ! (cd "$dir" && npm audit --omit=dev --audit-level=high); then
            bad=1
        fi
        echo "--- $dir: all dependencies, dev included"
        (cd "$dir" && npm audit --json > "$REPORTS/npm-audit-$(echo "$dir" | tr '/' '_').json") || true
        if ! (cd "$dir" && npm audit --include=dev --audit-level=high); then
            bad=1
        fi
    done < <(git ls-files '*package-lock.json')
    # npm/cli has no package-lock.json: its only dependencies are
    # Atalanta's own per-platform packages, built by the release job.
    if [ "$bad" -eq 0 ]; then
        pass
    else
        fail "high or critical npm advisories (see the log)"
    fi
}

check_gitleaks() {
    have gitleaks "brew install gitleaks" || return 0
    local bad=0
    echo "--- every commit"
    gitleaks git --config .gitleaks.toml --redact --no-banner \
        --report-format sarif --report-path "$REPORTS/gitleaks.sarif" . || bad=1
    if [ -z "${CI:-}" ]; then
        # Local only: what is not committed yet (CI has a clean checkout).
        echo "--- uncommitted changes"
        gitleaks git --config .gitleaks.toml --redact --no-banner --pre-commit . || bad=1
    fi
    if [ "$bad" -eq 0 ]; then
        pass
    else
        fail "possible secrets found; rotate any real one first (values are redacted in the log)"
    fi
}

check_zizmor() {
    have zizmor "brew install zizmor  (or: pipx install zizmor)" || return 0
    local mode=--offline rc=0
    if [ -n "${GH_TOKEN:-}" ]; then
        mode=--no-progress
    fi
    # The repository root: .github/workflows/*.yml, action.yml and
    # .github/dependabot.yml. Accepted findings carry an inline
    # `# zizmor: ignore[rule]` comment saying why.
    zizmor "$mode" --format sarif . > "$REPORTS/zizmor.sarif" 2> /dev/null || true
    zizmor "$mode" . || rc=$?
    # Exit 11 means only informational findings (shown above, not failed
    # on); 12-14 mean low, medium or high ones.
    case "$rc" in
        0 | 11) pass ;;
        1[2-4]) fail "workflow findings (see the log, zizmor.sarif)" ;;
        *) fail "zizmor could not run (exit $rc)" ;;
    esac
}

check_semgrep() {
    local cmd
    read -r -a cmd <<< "${SEMGREP:-semgrep}"
    if [ "${cmd[0]}" = semgrep ]; then
        have semgrep "brew install semgrep  (or: pipx install semgrep)" || return 0
    fi
    # Rule packs from the open Semgrep registry; nothing is uploaded
    # (--metrics=off, no login). Secrets are gitleaks' job, unsafe Rust the
    # unsafe check's, so p/secrets and the INFO-level audit rules are left
    # out. The excluded rule:
    #   unencrypted-socket: every socket Atalanta opens is on loopback (or an
    #   abstract socket over adb), authenticated by the HMAC handshake where
    #   it carries commands (docs/SECURITY.md); TLS is not the control there.
    # Triaged findings are `// nosemgrep: <rule-id>` comments with a reason.
    local args=(
        scan --metrics=off --error --disable-version-check
        --severity ERROR --severity WARNING
        --config p/rust --config p/javascript --config p/typescript
        --config p/react --config p/python --config p/java --config r/bash
        --config p/github-actions
        --exclude-rule java.lang.security.audit.crypto.unencrypted-socket.unencrypted-socket
    )
    "${cmd[@]}" "${args[@]}" --sarif --output "$REPORTS/semgrep.sarif" --quiet . > /dev/null 2>&1 || true
    if "${cmd[@]}" "${args[@]}" .; then
        pass
    else
        fail "SAST findings (see the log, semgrep.sarif)"
    fi
}

check_unsafe() {
    have python3 "brew install python" || return 0
    if python3 -I scripts/unsafe-inventory.py --check; then
        pass
    else
        fail "a crate has more unsafe code than scripts/unsafe-baseline.txt allows"
    fi
}

check_sbom() {
    have cargo-cyclonedx "cargo install --locked cargo-cyclonedx" || return 0
    have npm "brew install node" || return 0
    if scripts/sbom.sh "$REPORTS/sbom"; then
        pass
    else
        fail "SBOM generation failed"
    fi
}

check_fuzz() {
    have cargo-fuzz "cargo install --locked cargo-fuzz  (and: rustup toolchain install nightly --profile minimal)" || return 0
    local t secs=${FUZZ_SECONDS:-30} bad=0
    for t in $(cd fuzz && cargo +nightly fuzz list); do
        echo "--- $t (${secs}s)"
        (cd fuzz && cargo +nightly fuzz run "$t" -- -max_total_time="$secs" -timeout=10 -rss_limit_mb=2048) || bad=1
    done
    if [ "$bad" -eq 0 ]; then
        pass
    else
        fail "a fuzz target crashed (inputs in fuzz/artifacts/)"
    fi
}

checks=("$@")
if [ ${#checks[@]} -eq 0 ]; then
    read -r -a checks <<< "$ALL_CHECKS"
fi
for CHECK in "${checks[@]}"; do
    case "$CHECK" in
        deny | osv | npm-audit | gitleaks | zizmor | semgrep | unsafe | sbom | fuzz) ;;
        -h | --help) sed -n '2,35p' "$0"; exit 0 ;;
        *) echo "unknown check '$CHECK' (one of: $ALL_CHECKS sbom fuzz)" >&2; exit 2 ;;
    esac
done

for CHECK in "${checks[@]}"; do
    say "$CHECK"
    "check_${CHECK//-/_}"
done

say "summary"
echo "passed:  ${passed[*]:-none}"
echo "skipped: ${skipped[*]:-none}"
echo "failed:  ${failed[*]:-none}"
echo "reports: $REPORTS"
if [ ${#failed[@]} -gt 0 ]; then
    exit 1
fi
if [ "$STRICT" = 1 ] && [ ${#skipped[@]} -gt 0 ]; then
    echo "SECURITY_STRICT=1: a check was skipped" >&2
    exit 1
fi
