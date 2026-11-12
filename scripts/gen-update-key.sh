#!/usr/bin/env bash
# Creates the project's update-signing key pair (minisign, Ed25519). Run once, by the maintainer.
#
#   scripts/gen-update-key.sh <key-dir> [--no-password] [--install-public-key]
#
# * <key-dir> must be OUTSIDE this repository (the secret key must never be committed).
# * Writes <key-dir>/lector-update.key (SECRET) and <key-dir>/lector-update.pub.
# * By default minisign asks for a password for the secret key; use a strong one and keep it in a
#   password manager. --no-password creates an unencrypted key (only for a CI-only key you can rotate).
# * --install-public-key also copies the public key to packaging/update-key.pub, the file that is
#   compiled into Lector (crates/lector-forge/src/releases.rs). Commit that file.
#
# Then, in GitHub: Settings -> Secrets and variables -> Actions -> New repository secret
#   UPDATE_SIGNING_KEY            the full contents of lector-update.key
#   UPDATE_SIGNING_KEY_PASSWORD   its password (leave the secret out for --no-password keys)
# and back the key up offline (docs/RELEASING.md, "Update signing key"). Until the secret exists the
# release workflow publishes SHA256SUMS.txt without a signature and Lector reports downloads as
# "not verified". Rotating the key means shipping a new Lector with the new public key first.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIR=""
NO_PASSWORD=false
INSTALL=false
for arg in "$@"; do
  case "$arg" in
    --no-password) NO_PASSWORD=true ;;
    --install-public-key) INSTALL=true ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    -*) echo "unknown option: $arg" >&2; exit 2 ;;
    *) DIR="$arg" ;;
  esac
done
[ -n "$DIR" ] || { sed -n '2,6p' "$0" >&2; exit 2; }
command -v minisign >/dev/null 2>&1 || {
  echo "minisign is not installed (macOS: brew install minisign; Debian/Ubuntu: apt install minisign;" >&2
  echo "Windows: scoop install minisign; or https://jedisct1.github.io/minisign/)" >&2
  exit 1
}

DIR="$(realpath -m "$DIR")"
case "$DIR/" in
  "$ROOT"/*) echo "refusing: $DIR is inside the repository; choose a directory outside it" >&2; exit 1 ;;
esac
mkdir -p "$DIR"
KEY="$DIR/lector-update.key"
PUB="$DIR/lector-update.pub"
if [ -e "$KEY" ] || [ -e "$PUB" ]; then
  echo "refusing to overwrite an existing key in $DIR" >&2
  exit 1
fi

if [ "$NO_PASSWORD" = true ]; then
  minisign -G -W -c "Lector update signing key" -p "$PUB" -s "$KEY"
else
  minisign -G -c "Lector update signing key" -p "$PUB" -s "$KEY"
fi
chmod 600 "$KEY"

if [ "$INSTALL" = true ]; then
  cp "$PUB" "$ROOT/packaging/update-key.pub"
  echo "installed the public key into packaging/update-key.pub (review and commit it)"
fi

cat <<TXT

Public key ($PUB):
$(cat "$PUB")

Next steps:
  1. Back up $KEY and its password offline (two places). Losing it means shipping a new Lector
     with a new public key before updates can be verified again.
  2. Add the GitHub Actions secrets UPDATE_SIGNING_KEY (contents of the .key file) and
     UPDATE_SIGNING_KEY_PASSWORD (omit for --no-password keys).
  3. Put the public key into packaging/update-key.pub (re-run with --install-public-key, or copy
     $PUB over it) and commit it. The release workflow refuses to sign while that file is still the
     placeholder.
  4. Delete $KEY from this machine once it is backed up and stored as a secret.
TXT
