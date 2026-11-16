#!/usr/bin/env bash
# Build target/SoundRouter.app. Ad-hoc signed unless SIGN_ID is set.
set -euo pipefail
cd "$(dirname "$0")/.."

APP=target/SoundRouter.app

# No rust-toolchain.toml: pin the toolchain (app needs 1.95) unless caller set one.
export PATH="$HOME/.cargo/bin:$PATH"
if command -v rustup >/dev/null 2>&1; then export RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-1.95}"; fi

cargo build --release -p soundrouter-app -p sr-cli

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp packaging/Info-app.plist "$APP/Contents/Info.plist"
cp target/release/SoundRouter "$APP/Contents/MacOS/SoundRouter"
cp target/release/sr-cli "$APP/Contents/MacOS/sr-cli"
cp app/assets/icon.icns "$APP/Contents/Resources/icon.icns"

if [ -n "${SIGN_ID:-}" ]; then
  codesign --force --options runtime --timestamp \
    --entitlements packaging/entitlements-app.plist \
    --sign "$SIGN_ID" "$APP"
else
  codesign --force --sign - "$APP"
fi

codesign --verify --deep --strict "$APP"
echo "built $APP"
