#!/usr/bin/env bash
# Notarize and staple a .pkg or .dmg. Usage: NOTARY_PROFILE=<profile> packaging/notarize.sh <file>
# Create the profile once: xcrun notarytool store-credentials <profile>
set -euo pipefail

FILE=${1:?usage: notarize.sh <file.pkg|file.dmg>}
: "${NOTARY_PROFILE:?set NOTARY_PROFILE to a notarytool keychain profile}"

xcrun notarytool submit "$FILE" --keychain-profile "$NOTARY_PROFILE" --wait
xcrun stapler staple "$FILE"
xcrun stapler validate "$FILE"
