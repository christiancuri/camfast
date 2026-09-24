#!/usr/bin/env bash
# Builds dist/CamFast-<version>.dmg (drag-to-Applications installer) for another Apple Silicon Mac.
#
#   packaging/macos/dmg.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PKG="$ROOT/packaging/macos"
DIST="$ROOT/dist"

"$PKG/bundle.sh"

VERSION="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$DIST/CamFast.app/Contents/Info.plist")"
DMG="$DIST/CamFast-$VERSION.dmg"
STAGE="$DIST/.dmg-staging"

rm -rf "$STAGE"
mkdir -p "$STAGE"
ditto "$DIST/CamFast.app" "$STAGE/CamFast.app"
ln -s /Applications "$STAGE/Applications"
cp "$PKG/README.txt" "$STAGE/README.txt"

rm -f "$DMG"
hdiutil create -volname "CamFast" -srcfolder "$STAGE" -fs HFS+ -format UDZO -imagekey zlib-level=9 -ov "$DMG" >/dev/null
rm -rf "$STAGE"
hdiutil verify "$DMG" >/dev/null

echo "built $DMG ($(du -h "$DMG" | cut -f1))"
