#!/bin/sh
# Removes a Lector installation made by install.sh (same PREFIX argument).
set -eu
PREFIX="${1:-$HOME/.local}"
APP_ID=io.github.meetdhruva.Lector
rm -f "$PREFIX/bin/lector" "$PREFIX/share/applications/$APP_ID.desktop" "$PREFIX/share/metainfo/$APP_ID.metainfo.xml"
find "$PREFIX/share/icons/hicolor" -name "$APP_ID.*" -delete 2>/dev/null || true
echo "Lector removed from $PREFIX"
