#!/usr/bin/env bash
# Prints GitHub release notes for <version>: the CHANGELOG section plus a download guide.
#   scripts/release-notes.sh 0.1.0 > notes.md
set -euo pipefail
VERSION="$1"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# CHANGELOG section for a version (from its "## <version>" heading to the next "## ").
section() {
  awk -v v="$1" '
    /^## / { if (found) exit; if (index($0, "## " v " ") == 1 || $0 == "## " v) { found = 1; next } }
    found { print }
  ' "$ROOT/CHANGELOG.md"
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

### Notes for this release

- **Builds are not code-signed yet.** On macOS, right-click Lector.app and choose *Open* the first
  time (or run \`xattr -dr com.apple.quarantine /Applications/Lector.app\`). On Windows, SmartScreen may
  ask you to confirm: choose *More info → Run anyway*.
- **Portable mode:** the Windows portable zip keeps settings in its \`lector-data\` folder. Any build
  becomes portable if a \`lector-data\` folder exists next to the executable.
- **Linux tarball:** run \`./install.sh\` to install for your user (\`~/.local\`), or
  \`sudo ./install.sh /usr/local\` system-wide. Requires Vulkan (Mesa or vendor drivers).
- **Command line:** \`lector [PATH[:LINE[:COL]]]\`. On macOS, link the CLI with
  \`sudo ln -sf /Applications/Lector.app/Contents/MacOS/lector /usr/local/bin/lector\`.
EOF
