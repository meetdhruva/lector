#!/usr/bin/env bash
# Developer ID signing, notarization and verification for the macOS release (docs/RELEASING.md).
#
#   scripts/sign-macos.sh setup            import the certificate into a temporary keychain and the
#                                          App Store Connect API key into a private temp dir
#   scripts/sign-macos.sh sign <path>      sign Lector.app (hardened runtime, inside-out) or a .dmg
#   scripts/sign-macos.sh notarize <path>  submit a .dmg / .zip / .app to Apple and wait; fails unless accepted
#   scripts/sign-macos.sh staple <path>    staple the notarization ticket (.app or .dmg)
#   scripts/sign-macos.sh verify <path>    codesign --verify --deep --strict, spctl, stapler validate
#   scripts/sign-macos.sh cleanup          delete the temporary keychain and key material
#
# Credentials come from the environment (GitHub secrets mapped by the workflow); nothing is ever
# printed. `setup` needs MACOS_CERTIFICATE_P12_BASE64 + MACOS_CERTIFICATE_PASSWORD (signing) and
# optionally APPLE_API_KEY_P8_BASE64 + APPLE_API_KEY_ID + APPLE_API_ISSUER_ID (notarization).
# MACOS_SIGNING_IDENTITY (e.g. "Developer ID Application: Name (TEAMID)") is optional; without it the
# first "Developer ID Application" identity in the imported certificate is used.
#
# State lives in $LECTOR_SIGN_STATE (default: $RUNNER_TEMP/lector-sign or a mktemp directory) in files
# with mode 600, so `setup`, `sign`, ... may run in different processes of one job.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ENTITLEMENTS="$ROOT/packaging/macos/entitlements.plist"
STATE="${LECTOR_SIGN_STATE:-${RUNNER_TEMP:-${TMPDIR:-/tmp}}/lector-sign}"

die() { echo "sign-macos: $*" >&2; exit 1; }

load_state() {
  [ -f "$STATE/env" ] || die "no signing state in $STATE (run 'setup' first)"
  # shellcheck disable=SC1091
  . "$STATE/env"
}

cmd_setup() {
  umask 077
  mkdir -p "$STATE"
  : > "$STATE/env"
  if [ -n "${MACOS_CERTIFICATE_P12_BASE64:-}" ]; then
    : "${MACOS_CERTIFICATE_PASSWORD:?MACOS_CERTIFICATE_PASSWORD is required with the certificate}"
    local kc="$STATE/lector-signing.keychain-db" pw p12="$STATE/certificate.p12"
    pw="$(openssl rand -base64 24)"
    echo "::add-mask::$pw" 2>/dev/null || true
    printf '%s' "$MACOS_CERTIFICATE_P12_BASE64" | base64 --decode > "$p12"
    security create-keychain -p "$pw" "$kc"
    security set-keychain-settings -lut 21600 "$kc"
    security unlock-keychain -p "$pw" "$kc"
    # The password travels on argv (`security import -P`): mask it in the log first, in case a tool
    # echoes the command line on failure. (GitHub already masks the secret itself.)
    echo "::add-mask::$MACOS_CERTIFICATE_PASSWORD"
    security import "$p12" -P "$MACOS_CERTIFICATE_PASSWORD" -A -t cert -f pkcs12 -k "$kc" >/dev/null
    rm -f "$p12"
    security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$pw" "$kc" >/dev/null
    # Make the keychain visible to codesign without removing the user's own keychains.
    # shellcheck disable=SC2046
    security list-keychains -d user -s "$kc" $(security list-keychains -d user | tr -d '"')
    local identity="${MACOS_SIGNING_IDENTITY:-}"
    if [ -z "$identity" ]; then
      identity="$(security find-identity -v -p codesigning "$kc" | sed -n 's/.*\([0-9A-F]\{40\}\) "Developer ID Application:.*/\1/p' | head -n 1)"
    fi
    [ -n "$identity" ] || die "no 'Developer ID Application' identity found in the imported certificate"
    {
      printf 'LECTOR_KEYCHAIN=%q\n' "$kc"
      printf 'LECTOR_MACOS_SIGN_IDENTITY=%q\n' "$identity"
    } >> "$STATE/env"
  fi
  if [ -n "${APPLE_API_KEY_P8_BASE64:-}" ]; then
    : "${APPLE_API_KEY_ID:?APPLE_API_KEY_ID is required with the API key}"
    : "${APPLE_API_ISSUER_ID:?APPLE_API_ISSUER_ID is required with the API key}"
    printf '%s' "$APPLE_API_KEY_P8_BASE64" | base64 --decode > "$STATE/AuthKey.p8"
    {
      printf 'LECTOR_NOTARY_KEY=%q\n' "$STATE/AuthKey.p8"
      printf 'LECTOR_NOTARY_KEY_ID=%q\n' "$APPLE_API_KEY_ID"
      printf 'LECTOR_NOTARY_ISSUER=%q\n' "$APPLE_API_ISSUER_ID"
    } >> "$STATE/env"
  fi
  echo "signing material imported (state: $STATE)"
}

is_macho() { file -b "$1" | grep -q 'Mach-O'; }

# codesign flags shared by everything we sign: hardened runtime, secure timestamp.
codesign_args() {
  printf '%s\n' --force --timestamp --options runtime --keychain "$LECTOR_KEYCHAIN" --sign "$LECTOR_MACOS_SIGN_IDENTITY"
}

cmd_sign() {
  load_state
  local target="$1"
  [ -n "${LECTOR_MACOS_SIGN_IDENTITY:-}" ] || die "no signing identity imported"
  local args=()
  while IFS= read -r a; do args+=("$a"); done < <(codesign_args)
  case "$target" in
    *.app)
      local main="$target/Contents/MacOS/lector"
      [ -f "$main" ] || die "$main not found"
      # Nested code first (deepest paths first), the bundle last. Lector.app has no nested
      # code today; this keeps the order right if helpers or frameworks are added.
      while IFS= read -r f; do
        [ "$f" = "$main" ] && continue
        if is_macho "$f"; then
          echo "signing nested $f"
          codesign "${args[@]}" "$f"
        fi
      done < <(find "$target/Contents" -type f -print | awk '{ print length($0) "\t" $0 }' | sort -rn | cut -f2-)
      codesign "${args[@]}" --entitlements "$ENTITLEMENTS" "$target"
      ;;
    *.dmg)
      codesign "${args[@]}" "$target"
      ;;
    *) die "don't know how to sign $target" ;;
  esac
  echo "signed $target"
}

notary_json_field() { # <field> reads JSON on stdin
  /usr/bin/python3 -c 'import json,sys; print(json.load(sys.stdin).get(sys.argv[1], ""))' "$1"
}

cmd_notarize() {
  load_state
  local target="$1" upload="$1" tmp=""
  [ -n "${LECTOR_NOTARY_KEY:-}" ] || die "no notarization credentials imported"
  if [ -d "$target" ]; then # an .app must be zipped for upload
    tmp="$(mktemp -d)"
    upload="$tmp/$(basename "$target").zip"
    ditto -c -k --sequesterRsrc --keepParent "$target" "$upload"
  fi
  local out id status
  out="$(xcrun notarytool submit "$upload" --key "$LECTOR_NOTARY_KEY" --key-id "$LECTOR_NOTARY_KEY_ID" \
    --issuer "$LECTOR_NOTARY_ISSUER" --wait --timeout 45m --output-format json)" || {
    echo "$out" >&2
    die "notarytool submit failed"
  }
  [ -z "$tmp" ] || rm -rf "$tmp"
  id="$(printf '%s' "$out" | notary_json_field id)"
  status="$(printf '%s' "$out" | notary_json_field status)"
  echo "notarization of $(basename "$target"): $status (submission $id)"
  if [ "$status" != "Accepted" ]; then
    xcrun notarytool log "$id" --key "$LECTOR_NOTARY_KEY" --key-id "$LECTOR_NOTARY_KEY_ID" \
      --issuer "$LECTOR_NOTARY_ISSUER" >&2 || true
    die "notarization was not accepted"
  fi
}

cmd_staple() {
  xcrun stapler staple "$1"
  xcrun stapler validate "$1"
}

# verify <path> [notarized]: with "notarized" Gatekeeper (spctl) and the ticket must pass; for a
# signed-only build the spctl verdict is printed but not enforced (Gatekeeper rejects it by design).
cmd_verify() {
  local target="$1" notarized="${2:-}" spctl_args
  case "$target" in
    *.app)
      codesign --verify --deep --strict --verbose=2 "$target"
      codesign -dvv "$target" 2>&1 | grep -E '^(Identifier|Authority|TeamIdentifier|Timestamp|Runtime)' || true
      spctl_args=(-a -vv -t exec)
      ;;
    *.dmg)
      codesign --verify --strict --verbose=2 "$target"
      spctl_args=(-a -vv -t open --context context:primary-signature)
      ;;
    *) die "don't know how to verify $target" ;;
  esac
  if [ "$notarized" = notarized ]; then
    spctl "${spctl_args[@]}" "$target"
    xcrun stapler validate "$target"
  else
    spctl "${spctl_args[@]}" "$target" || echo "(spctl rejects this build: signed but not notarized)"
  fi
}

cmd_cleanup() {
  if [ -f "$STATE/env" ]; then
    # shellcheck disable=SC1091
    . "$STATE/env"
    if [ -n "${LECTOR_KEYCHAIN:-}" ]; then
      security delete-keychain "$LECTOR_KEYCHAIN" 2>/dev/null || true
    fi
  fi
  rm -rf "$STATE"
  echo "signing material removed"
}

sub="${1:-}"
[ $# -gt 0 ] && shift
case "$sub" in
  setup) cmd_setup ;;
  sign) cmd_sign "${1:?usage: sign <Lector.app|file.dmg>}" ;;
  notarize) cmd_notarize "${1:?usage: notarize <file>}" ;;
  staple) cmd_staple "${1:?usage: staple <file>}" ;;
  verify) cmd_verify "${1:?usage: verify <file> [notarized]}" "${2:-}" ;;
  cleanup) cmd_cleanup ;;
  *) sed -n '2,12p' "$0" >&2; exit 2 ;;
esac
