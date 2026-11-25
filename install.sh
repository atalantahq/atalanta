#!/bin/sh
# SPDX-FileCopyrightText: the Atalanta authors
# SPDX-License-Identifier: AGPL-3.0-only

# Atalanta installer: downloads a prebuilt release binary, checks it against
# the release's SHA256SUMS, and puts it in a directory you own (no sudo).
#
#   curl -fsSL https://atalanta.sh/install.sh | sh
#   curl -fsSL https://atalanta.sh/install.sh | sh -s -- --version 0.2.0 --dir ~/bin
#
# The same file is at https://raw.githubusercontent.com/atalantahq/atalanta/main/install.sh.
#
# Options (each also has an environment variable):
#   --version X.Y.Z     ATALANTA_VERSION      release to install (default: latest)
#   --dir DIR           ATALANTA_INSTALL_DIR  where to put `atalanta` (default: ~/.atalanta/bin)
#   --no-modify-path    ATALANTA_NO_MODIFY_PATH=1  don't add DIR to your shell profile
#   --verify-attestation  ATALANTA_VERIFY_ATTESTATION=1  also check the build's
#                       provenance with `gh attestation verify` (needs the GitHub CLI)
#   --sha256 HEX        ATALANTA_SHA256       the archive must also have this sha256
#                       (pins the exact build, e.g. in CI, on top of SHA256SUMS)
#   --keep-quarantine   ATALANTA_KEEP_QUARANTINE=1  on macOS, leave a
#                       com.apple.quarantine flag in place (see below)
#
# macOS: the release binaries are ad-hoc signed, not Developer ID signed or
# notarised. curl does not quarantine what it downloads, so normally there is
# nothing to do. If the installed binary does carry the quarantine flag, this
# script removes it, after the SHA256SUMS check, and says so; Gatekeeper would
# otherwise refuse to run it. --keep-quarantine leaves it for you to approve.
#
# Supported: macOS (Apple silicon, Intel) and Linux (x86_64, arm64; static
# binaries, any distribution). Windows is not supported.
set -eu

REPO="${ATALANTA_REPO:-atalantahq/atalanta}"
# Where release assets live; overridable for mirrors and for testing.
DOWNLOAD_BASE="${ATALANTA_DOWNLOAD_BASE:-https://github.com/${REPO}/releases/download}"
VERSION="${ATALANTA_VERSION:-latest}"
INSTALL_DIR="${ATALANTA_INSTALL_DIR:-$HOME/.atalanta/bin}"
MODIFY_PATH=1
[ "${ATALANTA_NO_MODIFY_PATH:-0}" = 1 ] && MODIFY_PATH=0
VERIFY_ATTESTATION="${ATALANTA_VERIFY_ATTESTATION:-0}"
PINNED_SHA256="${ATALANTA_SHA256:-}"
KEEP_QUARANTINE="${ATALANTA_KEEP_QUARANTINE:-0}"

if [ -t 2 ]; then
    BOLD=$(printf '\033[1m'); RED=$(printf '\033[31m'); GREEN=$(printf '\033[32m'); RESET=$(printf '\033[0m')
else
    BOLD=''; RED=''; GREEN=''; RESET=''
fi

info() { printf '%s\n' "$*" >&2; }
ok() { printf '%s%s%s\n' "$GREEN" "$*" "$RESET" >&2; }
die() {
    printf '%serror:%s %s\n' "$RED$BOLD" "$RESET" "$*" >&2
    exit 1
}

usage() {
    sed -n '2,26p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//' >&2 || true
    exit "${1:-0}"
}

while [ $# -gt 0 ]; do
    case $1 in
        --version) [ $# -ge 2 ] || die "--version needs a value"; VERSION=$2; shift 2 ;;
        --version=*) VERSION=${1#*=}; shift ;;
        --sha256) [ $# -ge 2 ] || die "--sha256 needs a value"; PINNED_SHA256=$2; shift 2 ;;
        --sha256=*) PINNED_SHA256=${1#*=}; shift ;;
        --dir) [ $# -ge 2 ] || die "--dir needs a value"; INSTALL_DIR=$2; shift 2 ;;
        --dir=*) INSTALL_DIR=${1#*=}; shift ;;
        --no-modify-path) MODIFY_PATH=0; shift ;;
        --verify-attestation) VERIFY_ATTESTATION=1; shift ;;
        --keep-quarantine) KEEP_QUARANTINE=1; shift ;;
        -h|--help) usage 0 ;;
        *) die "unknown option: $1 (see --help)" ;;
    esac
done

need() { command -v "$1" >/dev/null 2>&1; }

# download URL FILE
download() {
    if need curl; then
        curl --proto '=https,file' --tlsv1.2 -fsSL --retry 3 -o "$2" "$1"
    elif need wget; then
        # GNU wget: HTTPS only, TLS 1.2 or newer (busybox wget has neither flag).
        if wget --help 2>&1 | grep -q -- '--https-only'; then
            wget -q --https-only --secure-protocol=TLSv1_2 -O "$2" "$1"
        else
            wget -q -O "$2" "$1"
        fi
    else
        die "need curl or wget to download Atalanta"
    fi
}

detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case $os in
        Darwin)
            # A shell under Rosetta reports x86_64 on Apple silicon: use the
            # native build.
            if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
                arch=arm64
            fi
            case $arch in
                arm64|aarch64) echo aarch64-apple-darwin ;;
                x86_64) echo x86_64-apple-darwin ;;
                *) die "unsupported macOS architecture: $arch" ;;
            esac
            ;;
        Linux)
            case $arch in
                x86_64|amd64) echo x86_64-unknown-linux-musl ;;
                aarch64|arm64) echo aarch64-unknown-linux-musl ;;
                *) die "unsupported Linux architecture: $arch (prebuilt binaries: x86_64, arm64). Build from source: cargo install atalanta-cli" ;;
            esac
            ;;
        MINGW*|MSYS*|CYGWIN*|Windows_NT)
            die "Windows is not supported. Atalanta runs on macOS and Linux (WSL2 counts as Linux)." ;;
        *)
            die "unsupported OS: $os. Atalanta runs on macOS and Linux." ;;
    esac
}

resolve_version() {
    if [ "$VERSION" = latest ]; then
        need curl || die "resolving the latest release needs curl; set ATALANTA_VERSION instead"
        # The /releases/latest page redirects to /releases/tag/vX.Y.Z; no API
        # call, so no rate limit.
        url=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/${REPO}/releases/latest") ||
            die "could not find the latest release of ${REPO} (none published yet, or GitHub unreachable); set ATALANTA_VERSION=X.Y.Z"
        VERSION=${url##*/}
        case $VERSION in
            v[0-9]*) ;;
            *) die "could not find the latest release of ${REPO} (got '$url')" ;;
        esac
    fi
    VERSION=${VERSION#v}
    case $VERSION in
        [0-9]*.[0-9]*.[0-9]*) ;;
        *) die "invalid version '$VERSION' (expected X.Y.Z)" ;;
    esac
}

sha256_of() {
    if need sha256sum; then
        sha256sum "$1" | awk '{print $1}'
    elif need shasum; then
        shasum -a 256 "$1" | awk '{print $1}'
    elif need openssl; then
        openssl dgst -sha256 "$1" | awk '{print $NF}'
    else
        die "need sha256sum, shasum or openssl to verify the download"
    fi
}

add_to_path() {
    case ":${PATH}:" in
        *":${INSTALL_DIR}:"*) return 0 ;;
    esac
    if [ "$MODIFY_PATH" != 1 ]; then
        info "Add ${INSTALL_DIR} to your PATH to run atalanta."
        return 0
    fi
    case ${SHELL:-} in
        */zsh) profile="${ZDOTDIR:-$HOME}/.zshrc"; line="export PATH=\"${INSTALL_DIR}:\$PATH\"" ;;
        */bash)
            profile="$HOME/.bashrc"
            [ "$(uname -s)" = Darwin ] && profile="$HOME/.bash_profile"
            line="export PATH=\"${INSTALL_DIR}:\$PATH\"" ;;
        */fish) profile="$HOME/.config/fish/config.fish"; line="fish_add_path \"${INSTALL_DIR}\"" ;;
        *) info "Add ${INSTALL_DIR} to your PATH to run atalanta."; return 0 ;;
    esac
    if [ -f "$profile" ] && grep -F "$INSTALL_DIR" "$profile" >/dev/null 2>&1; then
        return 0
    fi
    mkdir -p "$(dirname "$profile")"
    printf '\n# Atalanta\n%s\n' "$line" >>"$profile"
    info "Added ${INSTALL_DIR} to PATH in ${profile} (open a new terminal, or: . ${profile})."
}

main() {
    target=$(detect_target)
    resolve_version
    archive="atalanta-${VERSION}-${target}.tar.gz"
    base="${DOWNLOAD_BASE}/v${VERSION}"

    info "${BOLD}Installing Atalanta ${VERSION} (${target})${RESET} into ${INSTALL_DIR}"

    tmp=$(mktemp -d 2>/dev/null || mktemp -d -t atalanta)
    trap 'rm -rf "$tmp"' EXIT INT TERM

    download "${base}/${archive}" "${tmp}/${archive}" ||
        die "download failed: ${base}/${archive} (is ${VERSION} a release with a build for ${target}?)"
    download "${base}/SHA256SUMS" "${tmp}/SHA256SUMS" ||
        die "could not download SHA256SUMS for ${VERSION}; refusing to install an unverified binary"

    expected=$(awk -v f="$archive" '$2 == f || $2 == "*" f { print $1 }' "${tmp}/SHA256SUMS")
    [ -n "$expected" ] || die "SHA256SUMS has no entry for ${archive}"
    actual=$(sha256_of "${tmp}/${archive}")
    [ "$expected" = "$actual" ] ||
        die "checksum mismatch for ${archive}: expected ${expected}, got ${actual}"
    info "Checksum verified (sha256 ${actual})."
    if [ -n "$PINNED_SHA256" ]; then
        pinned=$(printf '%s' "$PINNED_SHA256" | tr 'A-F' 'a-f')
        [ "$pinned" = "$actual" ] ||
            die "the archive's sha256 ${actual} is not the pinned ${pinned} (--sha256 / ATALANTA_SHA256)"
        info "Pinned sha256 matches."
    fi

    if [ "$VERIFY_ATTESTATION" = 1 ]; then
        need gh || die "--verify-attestation needs the GitHub CLI (gh)"
        gh attestation verify "${tmp}/${archive}" --repo "$REPO" >/dev/null ||
            die "build provenance attestation did not verify for ${archive}"
        info "Build provenance attestation verified."
    fi

    tar -xzf "${tmp}/${archive}" -C "$tmp" || die "could not unpack ${archive}"
    bin="${tmp}/atalanta-${VERSION}-${target}/atalanta"
    [ -f "$bin" ] || die "${archive} has no atalanta binary"

    if ! mkdir -p "$INSTALL_DIR" 2>/dev/null || [ ! -w "$INSTALL_DIR" ]; then
        die "${INSTALL_DIR} is not writable. Pick a directory you own (--dir ~/bin), or run this script with sudo yourself if you really want a system directory."
    fi
    # Write next to the target and rename, so a running atalanta is never
    # half-overwritten.
    cp "$bin" "${INSTALL_DIR}/.atalanta.tmp.$$"
    chmod 755 "${INSTALL_DIR}/.atalanta.tmp.$$"
    mv -f "${INSTALL_DIR}/.atalanta.tmp.$$" "${INSTALL_DIR}/atalanta"

    # The binary is ad-hoc signed, not notarised (see the header). Only act
    # when the flag is actually there, and say what was done.
    quarantined=0
    if [ "$(uname -s)" = Darwin ] &&
        xattr -p com.apple.quarantine "${INSTALL_DIR}/atalanta" >/dev/null 2>&1; then
        if [ "$KEEP_QUARANTINE" = 1 ]; then
            info "Left the macOS quarantine flag on ${INSTALL_DIR}/atalanta (--keep-quarantine); approve it in System Settings > Privacy & Security on first run."
            quarantined=1
        else
            xattr -d com.apple.quarantine "${INSTALL_DIR}/atalanta" ||
                die "could not remove the quarantine flag from ${INSTALL_DIR}/atalanta"
            info "Removed the macOS quarantine flag from ${INSTALL_DIR}/atalanta (its sha256 matched SHA256SUMS; the binary is not notarised). --keep-quarantine skips this."
        fi
    fi

    # A quarantined binary would be stopped by Gatekeeper here: skip the run check.
    if [ "$quarantined" = 0 ]; then
        "${INSTALL_DIR}/atalanta" --version >/dev/null 2>&1 ||
            die "installed ${INSTALL_DIR}/atalanta but it does not run on this machine"
    fi

    add_to_path
    ok "Atalanta ${VERSION} installed: ${INSTALL_DIR}/atalanta"
    info "Next: atalanta doctor   (checks adb, Xcode, devices)"
    info "      atalanta init     (writes a starter suite)"
}

main
