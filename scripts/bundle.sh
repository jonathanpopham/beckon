#!/usr/bin/env bash
# Build Beckon.app from the release binary. No Xcode project: the bundle
# is three files and a directory layout, which is all a bundle is.
#
# Usage:
#   scripts/bundle.sh            build target/release + dist/Beckon.app
#   scripts/bundle.sh --sign ID  additionally codesign with identity ID
#
# Signing notes:
#   * With no --sign argument the bundle gets an AD-HOC signature
#     (codesign -s -). That runs fine on the building machine; other
#     machines will show the Gatekeeper unidentified-developer prompt
#     (right-click > Open, once).
#   * For distribution: --sign "Developer ID Application: ..." then
#     notarize: xcrun notarytool submit dist/Beckon.zip --keychain-profile
#     <profile> --wait && xcrun stapler staple dist/Beckon.app
#   * Accessibility and Automation grants attach to the bundle identity,
#     so a signed bundle keeps its permissions across rebuilds; the raw
#     debug binary re-prompts whenever its path or hash changes.
set -euo pipefail
cd "$(dirname "$0")/.."

[ "$(uname -s)" = "Darwin" ] || { echo "bundle.sh: macOS only" >&2; exit 1; }

SIGN_ID="-"
if [ "${1:-}" = "--sign" ]; then
  SIGN_ID="${2:?--sign needs an identity}"
elif security find-identity -v -p codesigning 2>/dev/null | grep -q '"beckon-selfsign"'; then
  # A stable local identity keeps TCC grants (Accessibility etc.) across
  # rebuilds; ad-hoc signatures change every build and orphan the grant,
  # which shows up as "Settings says granted but macOS keeps prompting".
  # Mint one with: openssl req -x509 (codeSigning EKU), import to the
  # login keychain, security add-trusted-cert -p codeSign.
  SIGN_ID="beckon-selfsign"
fi

VERSION=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)"/\1/')

echo "==> building release binary"
cargo build --release -p beckon-macos

APP=dist/Beckon.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp target/release/beckon "$APP/Contents/MacOS/beckon"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key>      <string>com.jonathanpopham.beckon</string>
  <key>CFBundleName</key>            <string>Beckon</string>
  <key>CFBundleDisplayName</key>     <string>Beckon</string>
  <key>CFBundleExecutable</key>      <string>beckon</string>
  <key>CFBundlePackageType</key>     <string>APPL</string>
  <key>CFBundleShortVersionString</key> <string>${VERSION}</string>
  <key>CFBundleVersion</key>         <string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key>  <string>12.0</string>
  <key>LSUIElement</key>             <true/>
  <key>NSHighResolutionCapable</key> <true/>
  <key>NSHumanReadableCopyright</key> <string>MIT License</string>
</dict>
</plist>
PLIST

echo "==> codesigning (identity: ${SIGN_ID})"
codesign --force --options runtime -s "$SIGN_ID" "$APP"

echo "==> verifying"
codesign --verify --deep "$APP"
"$APP/Contents/MacOS/beckon" --version

echo "OK: $APP"
