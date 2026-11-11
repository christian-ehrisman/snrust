#!/usr/bin/env bash
# Builds a native macOS .app bundle (dist/snrust.app) with Retina & 120Hz ProMotion enabled.
#
# Usage:
#   ./scripts/bundle-macos.sh [--debug]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

PROFILE="release"
CARGO_FLAGS=("--release")
if [[ "${1:-}" == "--debug" ]]; then
    PROFILE="debug"
    CARGO_FLAGS=()
fi

echo "==> Building snrust (${PROFILE})..."
cargo build -p sn-app "${CARGO_FLAGS[@]}" --manifest-path "${ROOT_DIR}/Cargo.toml"

APP_NAME="snrust"
BUNDLE_DIR="${ROOT_DIR}/dist/${APP_NAME}.app"
CONTENTS_DIR="${BUNDLE_DIR}/Contents"
MACOS_DIR="${CONTENTS_DIR}/MacOS"
RESOURCES_DIR="${CONTENTS_DIR}/Resources"

rm -rf "${BUNDLE_DIR}"
mkdir -p "${MACOS_DIR}" "${RESOURCES_DIR}"

cp "${ROOT_DIR}/target/${PROFILE}/snrust" "${MACOS_DIR}/snrust"
chmod +x "${MACOS_DIR}/snrust"

cat > "${CONTENTS_DIR}/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleDisplayName</key>
    <string>snrust</string>
    <key>CFBundleExecutable</key>
    <string>snrust</string>
    <key>CFBundleIdentifier</key>
    <string>dev.snrust.desktop</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>snrust</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.0</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>LSMinimumSystemVersion</key>
    <string>14.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>CADisableMinimumFrameDurationOnPhone</key>
    <true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key>
    <true/>
</dict>
</plist>
PLIST

echo "==> Bundled macOS app at: ${BUNDLE_DIR}"
echo "    Launch with: open \"${BUNDLE_DIR}\""
