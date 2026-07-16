#!/bin/sh
# beckon installer: fetch the latest release, install to /Applications,
# clear the quarantine flag (the build is ad-hoc signed, not notarized;
# the source is right there if you would rather build it yourself), and
# launch. Usage:
#   curl -fsSL https://raw.githubusercontent.com/jonathanpopham/beckon/main/scripts/install.sh | sh
set -eu

REPO="jonathanpopham/beckon"
APP="/Applications/Beckon.app"

[ "$(uname -s)" = "Darwin" ] || { echo "beckon runs on macOS only" >&2; exit 1; }

echo "==> finding the latest release"
URL=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
  | grep -o '"browser_download_url": *"[^"]*Beckon[^"]*\.zip"' \
  | head -1 | sed 's/.*"\(https[^"]*\)"/\1/')
[ -n "$URL" ] || { echo "no release asset found; build from source: https://github.com/${REPO}" >&2; exit 1; }

WORK=$(mktemp -d /tmp/beckon-install.XXXXXX)
trap 'rm -rf "$WORK"' EXIT

echo "==> downloading ${URL##*/}"
curl -fsSL -o "$WORK/Beckon.zip" "$URL"

echo "==> installing to ${APP}"
ditto -x -k "$WORK/Beckon.zip" "$WORK/extract"
if [ -d "$APP" ]; then
  osascript -e 'tell application "Beckon" to quit' >/dev/null 2>&1 || true
  rm -rf "$APP"
fi
ditto "$WORK/extract/Beckon.app" "$APP"

# Ad-hoc signature: without this, Gatekeeper blocks the first open with
# an unidentified-developer dialog. Removing quarantine is the standard
# escape for source-available builds; skip it if you prefer the dialog.
xattr -dr com.apple.quarantine "$APP" 2>/dev/null || true

echo "==> launching"
open "$APP"
echo "beckon installed. Press Option+Space. Add ${APP} to Login Items to start at login."
echo "Customize (hotkey, theme, aliases): ~/.beckon/config.json"
