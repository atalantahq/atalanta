#!/usr/bin/env bash
# SPDX-FileCopyrightText: the Atalanta authors
# SPDX-License-Identifier: AGPL-3.0-only

set -euo pipefail

cd "$(dirname "$0")"

if ! command -v xcodegen >/dev/null 2>&1; then
  echo "xcodegen is not installed" >&2
  exit 1
fi

xcodegen generate
xcodebuild -project AtalantaEcho.xcodeproj -scheme AtalantaEcho -sdk iphonesimulator -configuration Debug -derivedDataPath build CODE_SIGNING_ALLOWED=NO -quiet build
echo "build/Build/Products/Debug-iphonesimulator/AtalantaEcho.app"
