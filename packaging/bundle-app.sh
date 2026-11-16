#!/usr/bin/env bash
# Build target/SoundRouter.app. Ad-hoc signed unless SIGN_ID is set.
set -euo pipefail
cd "$(dirname "$0")/.."

APP=target/SoundRouter.app

cargo build --release -p soundrouter-app -p sr-cli

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp packaging/Info-app.plist "$APP/Contents/Info.plist"
cp target/release/SoundRouter "$APP/Contents/MacOS/SoundRouter"
cp target/release/sr-cli "$APP/Contents/MacOS/sr-cli"

if [ -n "${SIGN_ID:-}" ]; then
  codesign --force --options runtime --timestamp \
    --entitlements packaging/entitlements-app.plist \
    --sign "$SIGN_ID" "$APP"
else
  codesign --force --sign - "$APP"
fi

codesign --verify --deep --strict "$APP"
echo "built $APP"
