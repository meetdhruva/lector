#!/usr/bin/env bash
# shellcheck disable=SC2016  # the backticks in the echo lines are literal Markdown
# Prints GitHub release notes for <version>: the CHANGELOG section, a download guide and what is
# signed. The signing statements come from what the build jobs recorded, never from assumptions:
#   scripts/release-notes.sh 0.1.0 [--signing-dir DIR] > notes.md
# DIR holds key=value files written by the packaging scripts (LECTOR_SIGNING_STATUS): macos.txt,
# windows-x64.txt, windows-arm64.txt, linux-x86_64.txt, linux-aarch64.txt, checksums.txt. A missing
# file, or no --signing-dir at all (dry runs), is reported as "not signed".
set -euo pipefail
VERSION="${1:?usage: release-notes.sh <version> [--signing-dir DIR]}"
shift
SIGNING_DIR=""
while [ $# -gt 0 ]; do
  case "$1" in
    --signing-dir) SIGNING_DIR="${2:?--signing-dir needs a directory}"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# CHANGELOG section for a version (from its "## <version>" heading to the next "## ").
section() {
  awk -v v="$1" '
    /^## / { if (found) exit; if (index($0, "## " v " ") == 1 || $0 == "## " v) { found = 1; next } }
    found { print }
  ' "$ROOT/CHANGELOG.md"
}

# status <name> <key>: value of <key> in <SIGNING_DIR>/<name>.txt, or "" when unknown.
status() {
  local file="$SIGNING_DIR/$1.txt"
  if [ -z "$SIGNING_DIR" ] || [ ! -f "$file" ]; then return 0; fi
  sed -n "s/^$2=//p" "$file" | head -n 1
}

NOTES="$(section "$VERSION")"
printf '%s\n' "$NOTES"
# Versions shipped in the same release ("Released together with X.Y.Z").
for other in $(printf '%s\n' "$NOTES" | sed -n 's/.*Released together with \([0-9][0-9.]*[0-9]\).*/\1/p'); do
  printf '\n## Also in this release: %s\n' "$other"
  section "$other"
done

cat <<EOF

## Downloads

| Platform | Installer | Portable |
|---|---|---|
| **Windows** x64 | \`Lector-$VERSION-windows-x64-setup.exe\` · \`.msi\` | \`Lector-$VERSION-windows-x64-portable.zip\` |
| **Windows** ARM64 | \`Lector-$VERSION-windows-arm64-setup.exe\` · \`.msi\` | \`Lector-$VERSION-windows-arm64-portable.zip\` |
| **macOS** (Apple Silicon + Intel) | \`Lector-$VERSION-macos-universal.dmg\` | \`Lector-$VERSION-macos-universal.zip\` |
| **Linux** x86_64 | \`lector_${VERSION}_amd64.deb\` · \`lector-$VERSION-1.x86_64.rpm\` | \`Lector-$VERSION-x86_64.AppImage\` · \`lector-$VERSION-linux-x86_64.tar.gz\` |
| **Linux** ARM64 | \`lector_${VERSION}_arm64.deb\` · \`lector-$VERSION-1.aarch64.rpm\` | \`Lector-$VERSION-aarch64.AppImage\` · \`lector-$VERSION-linux-aarch64.tar.gz\` |

Verify downloads with \`SHA256SUMS.txt\` (\`sha256sum -c SHA256SUMS.txt --ignore-missing\`).
Package-manager files for this release: \`lector.rb\` (Homebrew cask), \`Lector-$VERSION-winget-manifests.zip\`
(winget) and \`io.github.meetdhruva.Lector.yml\` (Flatpak manifest), when attached below.

### Signing

| Platform | Status |
|---|---|
EOF

mac_signed="$(status macos signed)"
mac_notarized="$(status macos notarized)"
if [ "$mac_signed" = true ] && [ "$mac_notarized" = true ]; then
  echo '| **macOS** | Signed with an Apple Developer ID (hardened runtime) and **notarized by Apple**; the ticket is stapled to the app and the disk image. |'
elif [ "$mac_signed" = true ]; then
  echo '| **macOS** | Signed with an Apple Developer ID but **not notarized**: Gatekeeper asks you to confirm the first launch. |'
else
  echo '| **macOS** | **Not signed** (ad-hoc signature only): right-click *Lector.app* and choose *Open* the first time, or run `xattr -dr com.apple.quarantine /Applications/Lector.app`. |'
fi

for arch in x64 arm64; do
  if [ "$arch" = x64 ]; then label="Windows x64"; else label="Windows ARM64"; fi
  if [ "$(status "windows-$arch" signed)" = true ]; then
    case "$(status "windows-$arch" provider)" in
      azure) how="Azure Artifact Signing (formerly Trusted Signing)" ;;
      pfx) how="a code-signing certificate" ;;
      *) how="Authenticode" ;;
    esac
    echo "| **$label** | **Authenticode-signed** with $how and timestamped: \`lector.exe\`, \`lector-cli.exe\`, the installer and the MSI. |"
  else
    echo "| **$label** | **Not signed**: SmartScreen may ask you to confirm (*More info → Run anyway*). |"
  fi
done

for arch in x86_64 aarch64; do
  if [ "$arch" = x86_64 ]; then label="Linux x86_64"; else label="Linux ARM64"; fi
  if [ "$(status "linux-$arch" signed)" = true ]; then
    fpr="$(status "linux-$arch" fingerprint)"
    rpm=""
    if [ "$(status "linux-$arch" rpm_embedded)" = true ]; then rpm=" The \`.rpm\` also carries an embedded signature."; fi
    echo "| **$label** | GPG detached signatures (\`.asc\`) for the tarball, AppImage, \`.deb\` and \`.rpm\`, key \`${fpr:-unknown}\`.$rpm |"
  else
    echo "| **$label** | **Not signed** (checksums only). |"
  fi
done

if [ "$(status checksums signed)" = true ]; then
  echo '| **All files** | `SHA256SUMS.txt` is signed with the Lector update key (`SHA256SUMS.txt.minisig`, [public key](https://github.com/meetdhruva/lector/blob/main/packaging/update-key.pub)); Lector uses it to verify update downloads. |'
else
  echo '| **All files** | `SHA256SUMS.txt` is **not signed** for this release; Lector will not report downloads as verified. |'
fi

cat <<EOF

How to verify each signature: [docs/RELEASING.md](https://github.com/meetdhruva/lector/blob/main/docs/RELEASING.md#verifying-a-signed-release).

### Notes for this release

- **Portable mode:** the Windows portable zip keeps settings in its \`lector-data\` folder. Any build
  becomes portable if a \`lector-data\` folder exists next to the executable.
- **Linux tarball:** run \`./install.sh\` to install for your user (\`~/.local\`), or
  \`sudo ./install.sh /usr/local\` system-wide. Requires Vulkan (Mesa or vendor drivers).
- **Command line:** \`lector [PATH[:LINE[:COL]]]\`. On macOS, link the CLI with
  \`sudo ln -sf /Applications/Lector.app/Contents/MacOS/lector /usr/local/bin/lector\`.
EOF
