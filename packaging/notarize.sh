#!/usr/bin/env bash
# Notarize and staple a .pkg or .dmg after checking it is actually notarizable.
# Usage: NOTARY_PROFILE=<profile> packaging/notarize.sh [--check] <file.pkg|file.dmg>
#   --check   run the pre-flight checks only (no Apple credentials needed, nothing uploaded)
# Create the profile once: xcrun notarytool store-credentials <profile>
# Pre-flight: every app/driver inside must be Developer ID signed with hardened runtime and a secure timestamp;
# a pkg must carry a Developer ID Installer signature; a dmg must be signed.
set -euo pipefail

CHECK_ONLY=0
if [ "${1:-}" = --check ]; then CHECK_ONLY=1; shift; fi
FILE=${1:?usage: notarize.sh [--check] <file.pkg|file.dmg>}
if [ "$CHECK_ONLY" = 0 ]; then
  : "${NOTARY_PROFILE:?set NOTARY_PROFILE to a notarytool keychain profile (xcrun notarytool store-credentials <profile>)}"
fi

die() { echo "notarize: $*" >&2; exit 1; }

[ -f "$FILE" ] || die "no such file: $FILE"
case "$FILE" in *.pkg|*.dmg) ;; *) die "expected a .pkg or .dmg, got: $FILE" ;; esac
xcrun --find notarytool >/dev/null 2>&1 || die "notarytool not found (install Xcode or the Command Line Tools)"

WORK=$(mktemp -d)
MNT=
cleanup() {
  if [ -n "$MNT" ]; then hdiutil detach -quiet "$MNT" || true; fi
  rm -rf "$WORK"
}
trap cleanup EXIT

# check_bundle <.app|.driver>
check_bundle() {
  local b=$1 info
  codesign --verify --deep --strict "$b" 2>/dev/null || die "$b: signature invalid or contents modified after signing (codesign --verify --deep --strict failed)"
  info=$(codesign -dvv "$b" 2>&1)
  if grep -q 'Signature=adhoc' <<<"$info"; then
    die "$b is ad-hoc signed; Apple rejects it. Rebuild with SIGN_ID='Developer ID Application: Name (TEAMID)'"
  fi
  grep -q '^Authority=Developer ID Application' <<<"$info" \
    || die "$b is not signed with a 'Developer ID Application' certificate (Apple Development / self-signed certs cannot be notarized)"
  grep -q 'flags=0x[0-9a-f]*(.*runtime' <<<"$info" || die "$b lacks the hardened runtime (sign with --options runtime)"
  grep -q '^Timestamp=' <<<"$info" || die "$b lacks a secure timestamp (sign with --timestamp)"
  case "$b" in
    *.app)
      # expected before notarization: "rejected / source=Unnotarized Developer ID"; shown for information only
      spctl --assess --type execute --verbose=2 "$b" 2>&1 | sed 's/^/  spctl: /' || true ;;
  esac
  echo "ok: $b"
}

# check_tree <dir>: every app/driver bundle below it
check_tree() {
  local found=0 b
  while IFS= read -r -d '' b; do
    found=1
    check_bundle "$b"
  done < <(find "$1" \( -name '*.app' -o -name '*.driver' \) -prune -print0)
  [ "$found" = 1 ] || die "no .app or .driver found inside $FILE"
}

case "$FILE" in
  *.pkg)
    sig=$(pkgutil --check-signature "$FILE" 2>&1) || die "$FILE is not signed (build with INSTALLER_ID='Developer ID Installer: Name (TEAMID)')"
    grep -q 'Developer ID Installer' <<<"$sig" || die "$FILE is signed, but not with a 'Developer ID Installer' certificate"
    pkgutil --expand-full "$FILE" "$WORK/pkg" >/dev/null || die "cannot expand $FILE"
    check_tree "$WORK/pkg" ;;
  *.dmg)
    codesign --verify --strict "$FILE" 2>/dev/null || die "$FILE is not signed or is damaged (build with SIGN_ID set)"
    MNT=$WORK/mnt
    mkdir "$MNT"
    hdiutil attach -readonly -nobrowse -noverify -mountpoint "$MNT" "$FILE" >/dev/null || die "cannot mount $FILE"
    check_tree "$MNT" ;;
esac

if [ "$CHECK_ONLY" = 1 ]; then echo "pre-flight ok: $FILE"; exit 0; fi

if ! out=$(xcrun notarytool submit "$FILE" --keychain-profile "$NOTARY_PROFILE" --wait --output-format json); then
  echo "$out" >&2
  die "notarytool submit failed; check the profile with: xcrun notarytool history --keychain-profile $NOTARY_PROFILE"
fi
status=$(plutil -extract status raw -o - - <<<"$out")
id=$(plutil -extract id raw -o - - <<<"$out")
if [ "$status" != Accepted ]; then
  xcrun notarytool log "$id" --keychain-profile "$NOTARY_PROFILE" >&2 || true
  die "notarization $id ended with status '$status' (log above)"
fi

xcrun stapler staple "$FILE"
xcrun stapler validate "$FILE"
case "$FILE" in
  *.pkg) spctl --assess --type install --verbose=2 "$FILE" ;;
  *.dmg) spctl --assess --type open --context context:primary-signature --verbose=2 "$FILE" ;;
esac
echo "notarized and stapled: $FILE"
