#!/usr/bin/env bash
# Fills the package-manager templates in packaging/ from a built release.
#
#   scripts/gen-package-manifests.sh <version> <dist-dir> <out-dir> [--notarized] [--require-all]
#
# <dist-dir> holds the release files (checksums are computed from them). Writes to <out-dir>:
#   lector.rb                               Homebrew cask (packaging/homebrew/lector.rb.in)
#   Lector-<version>-winget-manifests.zip   winget manifests, version + installer + defaultLocale
#   winget/*.yaml                           the same, loose (packaging/winget/*.in)
#   io.github.meetdhruva.Lector.yml         Flatpak manifest (packaging/flatpak/...yml)
# --notarized   the macOS build is notarized: leave the "not notarized" caveat out of the cask.
# --require-all fail when an input file is missing instead of skipping that manifest.
# RELEASE_DATE (YYYY-MM-DD) overrides today's date in the winget installer manifest.
set -euo pipefail

VERSION="${1:?usage: gen-package-manifests.sh <version> <dist-dir> <out-dir> [--notarized] [--require-all]}"
DIST="${2:?missing <dist-dir>}"
OUT="${3:?missing <out-dir>}"
shift 3
NOTARIZED=false
REQUIRE=false
for arg in "$@"; do
  case "$arg" in
    --notarized) NOTARIZED=true ;;
    --require-all) REQUIRE=true ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DATE="${RELEASE_DATE:-$(date -u +%F)}"
mkdir -p "$OUT/winget"

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

# need <file>...: true when all exist; otherwise warn (or fail with --require-all).
need() {
  for f in "$@"; do
    if [ ! -f "$f" ]; then
      if [ "$REQUIRE" = true ]; then echo "gen-package-manifests: missing $f" >&2; exit 1; fi
      echo "gen-package-manifests: missing $(basename "$f"); skipping" >&2
      return 1
    fi
  done
}

# fill <template> <out> KEY=value...: replaces @KEY@; fails if a placeholder is left over.
fill() {
  local template="$1" out="$2" arg key value
  shift 2
  cp "$template" "$out"
  for arg in "$@"; do
    key="${arg%%=*}"
    value="${arg#*=}"
    sed -i.bak "s|@${key}@|${value}|g" "$out"
  done
  rm -f "$out.bak"
  if grep -n '@[A-Z0-9_]\{3,\}@' "$out" >&2; then
    echo "gen-package-manifests: unresolved placeholder in $out" >&2
    exit 1
  fi
}

# --- Homebrew cask -----------------------------------------------------------------------------
DMG="$DIST/Lector-$VERSION-macos-universal.dmg"
if need "$DMG"; then
  fill "$ROOT/packaging/homebrew/lector.rb.in" "$OUT/lector.rb" "VERSION=$VERSION" "SHA256=$(sha256_of "$DMG")"
  if [ "$NOTARIZED" = true ]; then
    sed -i.bak '/# BEGIN-UNNOTARIZED/,/# END-UNNOTARIZED/d' "$OUT/lector.rb"
    rm -f "$OUT/lector.rb.bak"
  else
    sed -i.bak -e '/# BEGIN-UNNOTARIZED/d' -e '/# END-UNNOTARIZED/d' "$OUT/lector.rb"
    rm -f "$OUT/lector.rb.bak"
  fi
  echo "wrote $OUT/lector.rb"
fi

# --- winget --------------------------------------------------------------------------------------
X64="$DIST/Lector-$VERSION-windows-x64-setup.exe"
ARM64="$DIST/Lector-$VERSION-windows-arm64-setup.exe"
if need "$X64" "$ARM64"; then
  x64="$(sha256_of "$X64" | tr 'a-f' 'A-F')"
  arm64="$(sha256_of "$ARM64" | tr 'a-f' 'A-F')"
  for part in yaml installer.yaml locale.en-US.yaml; do
    fill "$ROOT/packaging/winget/meetdhruva.Lector.$part.in" "$OUT/winget/meetdhruva.Lector.$part" \
      "VERSION=$VERSION" "RELEASE_DATE=$DATE" "X64_SHA256=$x64" "ARM64_SHA256=$arm64"
  done
  ZIP="$OUT/Lector-$VERSION-winget-manifests.zip"
  rm -f "$ZIP"
  (cd "$OUT/winget" && zip -q -X "$ZIP" ./*.yaml)
  echo "wrote $OUT/winget/*.yaml and $ZIP"
fi

# --- Flatpak -------------------------------------------------------------------------------------
TAR_X64="$DIST/lector-$VERSION-linux-x86_64.tar.gz"
TAR_ARM64="$DIST/lector-$VERSION-linux-aarch64.tar.gz"
if need "$TAR_X64" "$TAR_ARM64"; then
  fill "$ROOT/packaging/flatpak/io.github.meetdhruva.Lector.yml" "$OUT/io.github.meetdhruva.Lector.yml" \
    "VERSION=$VERSION" "X86_64_SHA256=$(sha256_of "$TAR_X64")" "AARCH64_SHA256=$(sha256_of "$TAR_ARM64")"
  echo "wrote $OUT/io.github.meetdhruva.Lector.yml"
fi
