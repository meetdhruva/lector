#!/usr/bin/env bash
# Packages a built Lector binary for Linux.
#
#   scripts/package-linux.sh <binary> <version> <arch: x86_64|aarch64> <out-dir>
#
# Produces in <out-dir>:
#   lector-<version>-linux-<arch>.tar.gz   portable archive (+ install.sh)
#   lector_<version>_<amd64|arm64>.deb     Debian/Ubuntu package
#   lector-<version>-1.<arch>.rpm          Fedora/RHEL/openSUSE package (needs rpmbuild)
#   Lector-<version>-<arch>.AppImage       AppImage (needs appimagetool; set APPIMAGETOOL)
set -euo pipefail

BIN="$1"
VERSION="$2"
ARCH="$3"
OUT="$(mkdir -p "$4" && cd "$4" && pwd)"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP_ID=io.github.meetdhruva.Lector
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

case "$ARCH" in
  x86_64) DEB_ARCH=amd64 ;;
  aarch64) DEB_ARCH=arm64 ;;
  *) echo "unsupported arch: $ARCH" >&2; exit 1 ;;
esac

# --- shared FHS-style tree ------------------------------------------------------
stage_tree() { # <dest-prefix>
  local dest="$1"
  install -Dm755 "$BIN" "$dest/bin/lector"
  install -Dm644 "$ROOT/packaging/linux/$APP_ID.desktop" "$dest/share/applications/$APP_ID.desktop"
  install -Dm644 "$ROOT/packaging/linux/$APP_ID.metainfo.xml" "$dest/share/metainfo/$APP_ID.metainfo.xml"
  install -Dm644 "$ROOT/assets/app-icon/lector.svg" "$dest/share/icons/hicolor/scalable/apps/$APP_ID.svg"
  for size in 16 24 32 48 64 128 256 512; do
    install -Dm644 "$ROOT/assets/app-icon/png/lector-$size.png" \
      "$dest/share/icons/hicolor/${size}x${size}/apps/$APP_ID.png"
  done
}

stage_docs() { # <dest-dir>
  local dest="$1"
  mkdir -p "$dest/third-party"
  cp "$ROOT/README.md" "$ROOT/CHANGELOG.md" "$ROOT/LICENSE-MIT" "$ROOT/LICENSE-APACHE" "$dest/"
  cp "$ROOT/assets/THIRD_PARTY.md" "$dest/third-party/ASSETS.md"
  cp "$ROOT/crates/lector-icons/THIRD_PARTY.md" "$dest/third-party/ICONS.md" 2>/dev/null || true
  cp "$ROOT/crates/lector-syntax/GRAMMARS.md" "$dest/third-party/GRAMMARS.md" 2>/dev/null || true
  find "$ROOT/assets/fonts" -name 'OFL*' -exec sh -c 'cp "$1" "$2/third-party/$(basename "$(dirname "$1")")-OFL.txt"' _ {} "$dest" \;
  find "$ROOT/crates/lector-icons/assets" -maxdepth 2 -name 'LICENSE*' -exec sh -c 'cp "$1" "$2/third-party/$(basename "$(dirname "$1")")-LICENSE.txt"' _ {} "$dest" \;
  if [ -f "$ROOT/target/THIRD_PARTY_CRATES.md" ]; then cp "$ROOT/target/THIRD_PARTY_CRATES.md" "$dest/third-party/CRATES.md"; fi
}

# --- portable tarball -------------------------------------------------------------
NAME="lector-$VERSION-linux-$ARCH"
TAR_DIR="$WORK/$NAME"
stage_tree "$TAR_DIR"
stage_docs "$TAR_DIR"
install -m755 "$ROOT/packaging/linux/install.sh" "$ROOT/packaging/linux/uninstall.sh" "$TAR_DIR/"
tar -C "$WORK" -czf "$OUT/$NAME.tar.gz" "$NAME"
echo "built $OUT/$NAME.tar.gz"

# --- .deb ---------------------------------------------------------------------------
DEB_DIR="$WORK/deb"
stage_tree "$DEB_DIR/usr"
stage_docs "$DEB_DIR/usr/share/doc/lector"
INSTALLED_KB=$(du -sk "$DEB_DIR/usr" | cut -f1)
mkdir -p "$DEB_DIR/DEBIAN"
cat > "$DEB_DIR/DEBIAN/control" <<EOF
Package: lector
Version: $VERSION
Section: devel
Priority: optional
Architecture: $DEB_ARCH
Maintainer: Lector contributors <268780292+meetdhruva@users.noreply.github.com>
Installed-Size: $INSTALLED_KB
Depends: libc6 (>= 2.35), libgcc-s1, libxcb1, libxcb-xkb1, libxkbcommon0, libxkbcommon-x11-0, libvulkan1, libfontconfig1
Recommends: git, libwayland-client0, mesa-vulkan-drivers
Homepage: https://github.com/meetdhruva/lector
Description: Fast, reading-first code viewer and git review tool
 Lector is built for reading and reviewing code rather than typing it:
 tree-sitter highlighting, symbol navigation, project search, side-by-side
 diffs, review checklists, history graph and blame. It never modifies your
 files or repositories.
EOF
dpkg-deb --root-owner-group --build -Zxz "$DEB_DIR" "$OUT/lector_${VERSION}_${DEB_ARCH}.deb" >/dev/null
echo "built $OUT/lector_${VERSION}_${DEB_ARCH}.deb"

# --- .rpm ---------------------------------------------------------------------------
if command -v rpmbuild >/dev/null 2>&1; then
  RPM_TOP="$WORK/rpm"
  RPM_ROOT="$WORK/rpmroot"
  stage_tree "$RPM_ROOT/usr"
  stage_docs "$RPM_ROOT/usr/share/doc/lector"
  mkdir -p "$RPM_TOP"/{BUILD,RPMS,SOURCES,SPECS,SRPMS}
  cat > "$RPM_TOP/SPECS/lector.spec" <<EOF
Name:           lector
Version:        $VERSION
Release:        1
Summary:        Fast, reading-first code viewer and git review tool
License:        MIT OR Apache-2.0
URL:            https://github.com/meetdhruva/lector
BuildArch:      $ARCH
Requires:       libxcb, libxkbcommon, libxkbcommon-x11, vulkan-loader, fontconfig
Recommends:     git, mesa-vulkan-drivers
AutoReqProv:    no

%description
Lector is built for reading and reviewing code rather than typing it:
tree-sitter highlighting, symbol navigation, project search, side-by-side
diffs, review checklists, history graph and blame. It never modifies your
files or repositories.

%install
cp -a $RPM_ROOT/. %{buildroot}/

%files
/usr/bin/lector
/usr/share/applications/$APP_ID.desktop
/usr/share/metainfo/$APP_ID.metainfo.xml
/usr/share/icons/hicolor/*/apps/$APP_ID.*
/usr/share/doc/lector
EOF
  rpmbuild --define "_topdir $RPM_TOP" --define "_build_id_links none" --target "$ARCH" \
    -bb "$RPM_TOP/SPECS/lector.spec" >/dev/null
  find "$RPM_TOP/RPMS" -name '*.rpm' -exec cp {} "$OUT/lector-$VERSION-1.$ARCH.rpm" \;
  echo "built $OUT/lector-$VERSION-1.$ARCH.rpm"
else
  echo "rpmbuild not found; skipping .rpm" >&2
fi

# --- AppImage -----------------------------------------------------------------------
APPIMAGETOOL="${APPIMAGETOOL:-$(command -v appimagetool || true)}"
if [ -n "$APPIMAGETOOL" ]; then
  APPDIR="$WORK/Lector.AppDir"
  stage_tree "$APPDIR/usr"
  stage_docs "$APPDIR/usr/share/doc/lector"
  cp "$ROOT/packaging/linux/$APP_ID.desktop" "$APPDIR/$APP_ID.desktop"
  cp "$ROOT/assets/app-icon/png/lector-256.png" "$APPDIR/$APP_ID.png"
  cp "$ROOT/assets/app-icon/png/lector-256.png" "$APPDIR/.DirIcon"
  cat > "$APPDIR/AppRun" <<'EOF'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/lector" "$@"
EOF
  chmod +x "$APPDIR/AppRun"
  export ARCH APPIMAGE_EXTRACT_AND_RUN=1   # appimagetool reads ARCH for the runtime
  "$APPIMAGETOOL" --no-appstream "$APPDIR" \
    "$OUT/Lector-$VERSION-$ARCH.AppImage" >/dev/null
  echo "built $OUT/Lector-$VERSION-$ARCH.AppImage"
else
  echo "appimagetool not found; skipping AppImage" >&2
fi
