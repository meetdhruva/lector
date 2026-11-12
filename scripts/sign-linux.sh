#!/usr/bin/env bash
# GPG-signs the Linux release files (detached, ASCII-armored `.asc` next to each file) and, when
# possible, embeds a signature in the .rpm.
#
#   LINUX_GPG_PRIVATE_KEY_BASE64=... [LINUX_GPG_PASSPHRASE=...] scripts/sign-linux.sh <dist-dir>
#
# Signs *.tar.gz, *.AppImage, *.deb, *.rpm in <dist-dir>. The key is imported into a throw-away
# GNUPGHOME that is deleted on exit. Each signature is verified straight away.
#   LINUX_RPM_SIGN=0          skip the embedded .rpm signature (rpmsign is best effort anyway)
#   LECTOR_SIGNING_STATUS=f   write what happened (signed=, rpm_embedded=, fingerprint=) to file f
# Nothing secret is printed.
set -euo pipefail

DIST="${1:?usage: sign-linux.sh <dist-dir>}"
: "${LINUX_GPG_PRIVATE_KEY_BASE64:?LINUX_GPG_PRIVATE_KEY_BASE64 is not set}"

umask 077
GNUPGHOME="$(mktemp -d)"
export GNUPGHOME
cleanup() {
  gpgconf --kill gpg-agent >/dev/null 2>&1 || true
  rm -rf "$GNUPGHOME"
}
trap cleanup EXIT

printf '%s' "$LINUX_GPG_PRIVATE_KEY_BASE64" | base64 --decode | gpg --batch --quiet --import
FPR="$(gpg --batch --with-colons --list-secret-keys | awk -F: '/^fpr:/ { print $10; exit }')"
[ -n "$FPR" ] || { echo "sign-linux: no secret key imported" >&2; exit 1; }

PASS_ARGS=(--pinentry-mode loopback)
if [ -n "${LINUX_GPG_PASSPHRASE:-}" ]; then
  printf '%s' "$LINUX_GPG_PASSPHRASE" > "$GNUPGHOME/passphrase"
  PASS_ARGS+=(--passphrase-file "$GNUPGHOME/passphrase")
fi

shopt -s nullglob
files=("$DIST"/*.tar.gz "$DIST"/*.AppImage "$DIST"/*.deb "$DIST"/*.rpm)
[ ${#files[@]} -gt 0 ] || { echo "sign-linux: nothing to sign in $DIST" >&2; exit 1; }

# Embedded rpm signature first: it rewrites the .rpm, so the detached signature must come after.
RPM_EMBEDDED=false
if [ "${LINUX_RPM_SIGN:-1}" != 0 ] && command -v rpmsign >/dev/null 2>&1; then
  for f in "$DIST"/*.rpm; do
    if rpmsign --addsign --define "_gpg_name $FPR" \
        --define "_gpg_sign_cmd_extra_args --batch ${PASS_ARGS[*]}" "$f" >/dev/null 2>&1; then
      RPM_EMBEDDED=true
    else
      echo "sign-linux: embedded rpm signature failed for $(basename "$f") (detached .asc is still made)" >&2
      RPM_EMBEDDED=false
      break
    fi
  done
fi

for f in "${files[@]}"; do
  gpg --batch --yes --quiet "${PASS_ARGS[@]}" --local-user "$FPR" --armor --detach-sign --output "$f.asc" "$f"
  gpg --batch --quiet --verify "$f.asc" "$f"
  echo "signed $(basename "$f")"
done

if [ -n "${LECTOR_SIGNING_STATUS:-}" ]; then
  mkdir -p "$(dirname "$LECTOR_SIGNING_STATUS")"
  {
    echo "platform=linux"
    echo "signed=true"
    echo "rpm_embedded=$RPM_EMBEDDED"
    echo "fingerprint=$FPR"
  } > "$LECTOR_SIGNING_STATUS"
fi
