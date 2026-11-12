#!/usr/bin/env bash
# Signs SHA256SUMS.txt with the project's minisign key and verifies the result against the public
# key committed in packaging/update-key.pub (the one compiled into Lector).
#
#   UPDATE_SIGNING_KEY=<secret key file contents> [UPDATE_SIGNING_KEY_PASSWORD=...] \
#     scripts/sign-checksums.sh <tag> <dir containing SHA256SUMS.txt>
#
# Produces <dir>/SHA256SUMS.txt.minisig with the trusted comment "lector <tag>", which Lector
# requires (it binds the signature to the release and defeats replaying an older signed file).
# The signature must be a prehashed (`ED`) one; minisign 0.10+ makes those by default and the
# script checks. Exit codes: 0 signed and verified; 1 failure; 3 refused because
# packaging/update-key.pub is still the placeholder.
#   LECTOR_SIGNING_STATUS=f   write signed=true to file f
set -euo pipefail

TAG="${1:?usage: sign-checksums.sh <tag> <dir>}"
DIR="${2:?usage: sign-checksums.sh <tag> <dir>}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SUMS="$DIR/SHA256SUMS.txt"
SIG="$DIR/SHA256SUMS.txt.minisig"
: "${UPDATE_SIGNING_KEY:?UPDATE_SIGNING_KEY is not set}"
[ -f "$SUMS" ] || { echo "sign-checksums: $SUMS not found" >&2; exit 1; }

# The first line that is not blank, a '#' comment or an "untrusted comment:" line (the same rule as
# the Rust side, crates/lector-forge/src/releases.rs).
PUBLIC_KEY="$(grep -v -e '^[[:space:]]*$' -e '^[[:space:]]*#' -e '^untrusted comment:' "$ROOT/packaging/update-key.pub" | head -n 1 | tr -d '[:space:]' || true)"
if [ -z "$PUBLIC_KEY" ]; then
  echo "sign-checksums: packaging/update-key.pub is still the placeholder; Lector could not verify a signature." >&2
  echo "Run scripts/gen-update-key.sh and commit the public key first." >&2
  exit 3
fi

umask 077
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
printf '%s\n' "$UPDATE_SIGNING_KEY" > "$WORK/secret.key"

rm -f "$SIG"
printf '%s\n' "${UPDATE_SIGNING_KEY_PASSWORD:-}" |
  minisign -S -s "$WORK/secret.key" -m "$SUMS" -x "$SIG" -t "lector $TAG" >/dev/null

# Line 2 of the .minisig is base64 of "ED"/"Ed" + key id + signature: prehashed "ED" starts with
# "RU", legacy "Ed" with "RW" (the third character depends on the key id).
case "$(sed -n 2p "$SIG")" in
  RU*) ;;
  *) echo "sign-checksums: minisign produced a legacy (non-prehashed) signature; need minisign >= 0.10" >&2; exit 1 ;;
esac
minisign -V -P "$PUBLIC_KEY" -m "$SUMS" -x "$SIG" >/dev/null || {
  echo "sign-checksums: the signature does not verify against packaging/update-key.pub (wrong key pair?)" >&2
  exit 1
}
echo "signed $(basename "$SUMS") (trusted comment: lector $TAG)"
if [ -n "${LECTOR_SIGNING_STATUS:-}" ]; then
  mkdir -p "$(dirname "$LECTOR_SIGNING_STATUS")"
  printf 'platform=checksums\nsigned=true\n' > "$LECTOR_SIGNING_STATUS"
fi
