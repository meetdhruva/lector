#!/usr/bin/env bash
# Packages a built (ideally universal) Lector binary as a macOS app.
#
#   scripts/package-macos.sh <binary> <version> <arch-label: universal|aarch64|x86_64> <out-dir>
#
# Produces in <out-dir>:
#   Lector-<version>-macos-<arch>.dmg   disk image with Lector.app and an Applications link
#   Lector-<version>-macos-<arch>.zip   zipped Lector.app
# The app is ad-hoc signed (required on Apple Silicon); Developer ID signing and
# notarization come later.
set -euo pipefail

BIN="$1"
VERSION="$2"
ARCH="$3"
OUT="$(mkdir -p "$4" && cd "$4" && pwd)"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
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
codesign --force --deep --sign - --timestamp=none "$APP"
codesign --verify --deep --strict "$APP"

NAME="Lector-$VERSION-macos-$ARCH"
ditto -c -k --sequesterRsrc --keepParent "$APP" "$OUT/$NAME.zip"
echo "built $OUT/$NAME.zip"

DMG_DIR="$WORK/dmg"
mkdir -p "$DMG_DIR"
cp -R "$APP" "$DMG_DIR/"
ln -s /Applications "$DMG_DIR/Applications"
cat > "$DMG_DIR/READ ME FIRST.txt" <<TXT
Drag Lector to Applications.

This build is not yet signed with an Apple Developer ID. On first launch,
right-click Lector.app and choose "Open", or run:
    xattr -dr com.apple.quarantine /Applications/Lector.app

Command-line launcher:
    sudo ln -sf /Applications/Lector.app/Contents/MacOS/lector /usr/local/bin/lector
TXT
hdiutil create -volname "Lector $VERSION" -srcfolder "$DMG_DIR" -fs HFS+ -format UDZO -ov "$OUT/$NAME.dmg" >/dev/null
echo "built $OUT/$NAME.dmg"
