#!/usr/bin/env bash
# Builds dist/CamFast.app (ad-hoc signed, local use only) and optionally installs it.
#
#   packaging/macos/bundle.sh [--debug] [--install] [--bin PATH]
#
#   --debug     build with `cargo build` (dev profile) instead of --release
#   --install   copy the bundle to /Applications/CamFast.app (quits a running instance first)
#   --bin PATH  skip cargo and bundle the given executable (used to validate the script)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PKG="$ROOT/packaging/macos"
DIST="$ROOT/dist"
APP_NAME="CamFast"
EXEC_NAME="camfast"
APP="$DIST/$APP_NAME.app"
INSTALL_DIR="/Applications"

PROFILE="release"
INSTALL=0
BIN=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --debug) PROFILE="debug" ;;
    --install) INSTALL=1 ;;
    --bin) shift; BIN="${1:?--bin requires a path}" ;;
    -h|--help) sed -n '2,9p' "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
  shift
done

# --- version from [workspace.package] in the root Cargo.toml --------------------------------
VERSION="$(awk '/^\[workspace\.package\]/{f=1;next} /^\[/{f=0} f && /^version *=/{gsub(/[" ]/,"",$3); print $3; exit}' "$ROOT/Cargo.toml")"
[[ -n "$VERSION" ]] || { echo "could not read [workspace.package] version from Cargo.toml" >&2; exit 1; }

# --- build ----------------------------------------------------------------------------------
if [[ -z "$BIN" ]]; then
  if [[ "$PROFILE" == "release" ]]; then
    (cd "$ROOT" && cargo build --release -p "$EXEC_NAME")
  else
    (cd "$ROOT" && cargo build -p "$EXEC_NAME")
  fi
  BIN="$ROOT/target/$PROFILE/$EXEC_NAME"
fi
[[ -x "$BIN" ]] || { echo "executable not found: $BIN" >&2; exit 1; }

# --- assemble into a staging dir, then swap into place ----------------------------------------
STAGE="$DIST/.$APP_NAME.app.staging"
rm -rf "$STAGE"
mkdir -p "$STAGE/Contents/MacOS" "$STAGE/Contents/Resources"

cp "$BIN" "$STAGE/Contents/MacOS/$EXEC_NAME"
chmod 755 "$STAGE/Contents/MacOS/$EXEC_NAME"

sed "s/__VERSION__/$VERSION/g" "$PKG/Info.plist" > "$STAGE/Contents/Info.plist"
plutil -lint "$STAGE/Contents/Info.plist" >/dev/null
printf 'APPL????' > "$STAGE/Contents/PkgInfo"

# Icon: icon-1024.png -> AppIcon.iconset -> AppIcon.icns
ICONSET="$DIST/.AppIcon.iconset"
rm -rf "$ICONSET"; mkdir -p "$ICONSET"
for px in 16 32 128 256 512; do
  sips -z "$px" "$px" "$PKG/icon-1024.png" --out "$ICONSET/icon_${px}x${px}.png" >/dev/null
  px2=$((px * 2))
  sips -z "$px2" "$px2" "$PKG/icon-1024.png" --out "$ICONSET/icon_${px}x${px}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$STAGE/Contents/Resources/AppIcon.icns"
rm -rf "$ICONSET"

# --- ad-hoc sign + verify --------------------------------------------------------------------
codesign --force --sign - --timestamp=none --identifier dev.christiancuri.camfast "$STAGE"
codesign --verify --strict --verbose=2 "$STAGE"

rm -rf "$APP"
mv "$STAGE" "$APP"
echo "built $APP (v$VERSION, $PROFILE)"

# --- install ---------------------------------------------------------------------------------
if [[ "$INSTALL" == 1 ]]; then
  TARGET="$INSTALL_DIR/$APP_NAME.app"
  if pgrep -xq "$EXEC_NAME"; then
    echo "quitting running $APP_NAME..."
    osascript -e "quit app \"$APP_NAME\"" >/dev/null 2>&1 || true
    for _ in $(seq 1 50); do pgrep -xq "$EXEC_NAME" || break; sleep 0.2; done
    pgrep -xq "$EXEC_NAME" && pkill -x "$EXEC_NAME" || true
  fi
  NEW="$INSTALL_DIR/.$APP_NAME.app.new"
  OLD="$INSTALL_DIR/.$APP_NAME.app.old"
  rm -rf "$NEW" "$OLD"
  ditto "$APP" "$NEW"
  [[ -e "$TARGET" ]] && mv "$TARGET" "$OLD"
  mv "$NEW" "$TARGET"
  rm -rf "$OLD"
  echo "installed $TARGET"
fi
