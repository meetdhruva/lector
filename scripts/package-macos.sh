#!/usr/bin/env bash
# Packages a built (ideally universal) Lector binary as a macOS app.
#
#   scripts/package-macos.sh <binary> <version> <arch-label: universal|aarch64|x86_64> <out-dir>
#
# Produces in <out-dir>:
#   Lector-<version>-macos-<arch>.dmg   disk image with Lector.app and an Applications link
#   Lector-<version>-macos-<arch>.zip   zipped Lector.app
#
# Signing (docs/RELEASING.md), driven by the environment so local runs stay unsigned:
#   (default)                 ad-hoc signature (required on Apple Silicon), clearly labelled in the .dmg
#   LECTOR_MACOS_SIGN=1       Developer ID signature with hardened runtime for the .app and the .dmg;
#                             needs `scripts/sign-macos.sh setup` to have imported the certificate
#   LECTOR_MACOS_NOTARIZE=1   additionally notarize the .app and the .dmg with Apple and staple the tickets
#   LECTOR_SIGNING_STATUS=f   write what actually happened (signed=, notarized=) to file f
set -euo pipefail

BIN="$1"
VERSION="$2"
ARCH="$3"
OUT="$(mkdir -p "$4" && cd "$4" && pwd)"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SIGN_TOOL="$ROOT/scripts/sign-macos.sh"
SIGN="${LECTOR_MACOS_SIGN:-0}"
NOTARIZE="${LECTOR_MACOS_NOTARIZE:-0}"
if [ "$NOTARIZE" = 1 ] && [ "$SIGN" != 1 ]; then
  echo "LECTOR_MACOS_NOTARIZE=1 needs LECTOR_MACOS_SIGN=1" >&2
  exit 1
fi
SIGNED=false
NOTARIZED=false
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

APP="$WORK/Lector.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources/third-party"
install -m755 "$BIN" "$APP/Contents/MacOS/lector"
sed "s/@VERSION@/$VERSION/g" "$ROOT/packaging/macos/Info.plist" > "$APP/Contents/Info.plist"
printf 'APPL????' > "$APP/Contents/PkgInfo"
cp "$ROOT/assets/app-icon/lector.icns" "$APP/Contents/Resources/lector.icns"
cp "$ROOT/LICENSE-MIT" "$ROOT/LICENSE-APACHE" "$ROOT/README.md" "$ROOT/CHANGELOG.md" "$APP/Contents/Resources/"
cp "$ROOT/assets/THIRD_PARTY.md" "$APP/Contents/Resources/third-party/ASSETS.md"
cp "$ROOT/crates/lector-icons/THIRD_PARTY.md" "$APP/Contents/Resources/third-party/ICONS.md" 2>/dev/null || true
cp "$ROOT/crates/lector-syntax/GRAMMARS.md" "$APP/Contents/Resources/third-party/GRAMMARS.md" 2>/dev/null || true
if [ -f "$ROOT/target/THIRD_PARTY_CRATES.md" ]; then cp "$ROOT/target/THIRD_PARTY_CRATES.md" "$APP/Contents/Resources/third-party/CRATES.md"; fi

plutil -lint "$APP/Contents/Info.plist"
if [ "$SIGN" = 1 ]; then
  "$SIGN_TOOL" sign "$APP"
  SIGNED=true
  if [ "$NOTARIZE" = 1 ]; then
    # Notarize and staple the app itself, so it also passes Gatekeeper offline once copied out of the .dmg.
    "$SIGN_TOOL" notarize "$APP"
    "$SIGN_TOOL" staple "$APP"
    NOTARIZED=true
    "$SIGN_TOOL" verify "$APP" notarized
  else
    "$SIGN_TOOL" verify "$APP"
  fi
else
  codesign --force --deep --sign - --timestamp=none "$APP"
  codesign --verify --deep --strict "$APP"
fi

NAME="Lector-$VERSION-macos-$ARCH"
ditto -c -k --sequesterRsrc --keepParent "$APP" "$OUT/$NAME.zip"
echo "built $OUT/$NAME.zip"

DMG_DIR="$WORK/dmg"
mkdir -p "$DMG_DIR"
ditto "$APP" "$DMG_DIR/Lector.app"
ln -s /Applications "$DMG_DIR/Applications"
if [ "$NOTARIZED" = true ]; then
  cat > "$DMG_DIR/READ ME FIRST.txt" <<TXT
Drag Lector to Applications.

Command-line launcher:
    sudo ln -sf /Applications/Lector.app/Contents/MacOS/lector /usr/local/bin/lector
TXT
else
  cat > "$DMG_DIR/READ ME FIRST.txt" <<TXT
Drag Lector to Applications.

This build is not notarized by Apple (the project has not signed this release). On first
launch, right-click Lector.app and choose "Open", or run:
    xattr -dr com.apple.quarantine /Applications/Lector.app

Command-line launcher:
    sudo ln -sf /Applications/Lector.app/Contents/MacOS/lector /usr/local/bin/lector
TXT
fi
hdiutil create -volname "Lector $VERSION" -srcfolder "$DMG_DIR" -fs HFS+ -format UDZO -ov "$OUT/$NAME.dmg" >/dev/null
if [ "$SIGNED" = true ]; then
  "$SIGN_TOOL" sign "$OUT/$NAME.dmg"
  if [ "$NOTARIZED" = true ]; then
    "$SIGN_TOOL" notarize "$OUT/$NAME.dmg"
    "$SIGN_TOOL" staple "$OUT/$NAME.dmg"
    "$SIGN_TOOL" verify "$OUT/$NAME.dmg" notarized
  else
    "$SIGN_TOOL" verify "$OUT/$NAME.dmg"
  fi
fi
echo "built $OUT/$NAME.dmg"
if [ -n "${LECTOR_SIGNING_STATUS:-}" ]; then
  mkdir -p "$(dirname "$LECTOR_SIGNING_STATUS")"
  {
    echo "platform=macos"
    echo "signed=$SIGNED"
    echo "notarized=$NOTARIZED"
  } > "$LECTOR_SIGNING_STATUS"
fi
