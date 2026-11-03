#!/bin/sh
# Installs Lector from this archive for the current user (default) or system-wide.
#   ./install.sh            -> ~/.local
#   sudo ./install.sh /usr/local
set -eu
PREFIX="${1:-$HOME/.local}"
HERE="$(cd "$(dirname "$0")" && pwd)"
APP_ID=io.github.meetdhruva.Lector

install -Dm755 "$HERE/bin/lector" "$PREFIX/bin/lector"
install -Dm644 "$HERE/share/applications/$APP_ID.desktop" "$PREFIX/share/applications/$APP_ID.desktop"
install -Dm644 "$HERE/share/metainfo/$APP_ID.metainfo.xml" "$PREFIX/share/metainfo/$APP_ID.metainfo.xml"
for dir in "$HERE"/share/icons/hicolor/*/apps; do
  size="$(basename "$(dirname "$dir")")"
  for icon in "$dir"/*; do
    install -Dm644 "$icon" "$PREFIX/share/icons/hicolor/$size/apps/$(basename "$icon")"
  done
done
if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database -q "$PREFIX/share/applications" || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
  gtk-update-icon-cache -q -t "$PREFIX/share/icons/hicolor" || true
fi
echo "Lector installed to $PREFIX/bin/lector"
case ":$PATH:" in *":$PREFIX/bin:"*) ;; *) echo "Note: add $PREFIX/bin to your PATH." ;; esac
