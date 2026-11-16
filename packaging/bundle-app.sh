#!/usr/bin/env bash
# Build target/SoundRouter.app. Ad-hoc signed unless SIGN_ID is set.
set -euo pipefail
cd "$(dirname "$0")/.."

APP=target/SoundRouter.app

# rustup shims first so rust-toolchain.toml selects the pinned toolchain
export PATH="$HOME/.cargo/bin:$PATH"

cargo build --release -p soundrouter-app -p sr-cli

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp packaging/Info-app.plist "$APP/Contents/Info.plist"
cp target/release/SoundRouter "$APP/Contents/MacOS/SoundRouter"
cp target/release/sr-cli "$APP/Contents/MacOS/sr-cli"
cp app/assets/icon.icns "$APP/Contents/Resources/icon.icns"

if command -v cargo-about >/dev/null 2>&1; then
  cargo about generate -c packaging/about.toml packaging/about.hbs \
    -o "$APP/Contents/Resources/ThirdPartyLicenses.html"
elif [ "${REQUIRE_NOTICES:-}" = 1 ]; then
  echo "cargo-about missing (cargo install --locked cargo-about)" >&2; exit 1
else
  echo "warning: cargo-about missing; ThirdPartyLicenses.html not generated" >&2
fi

if [ -n "${SIGN_ID:-}" ]; then
  # inside-out: nested binary before the bundle
  for f in "$APP/Contents/MacOS/sr-cli" "$APP"; do
    codesign --force --options runtime --timestamp \
      --entitlements packaging/entitlements-app.plist \
      --sign "$SIGN_ID" "$f"
  done
else
  codesign --force --sign - "$APP"
fi

codesign --verify --deep --strict "$APP"
echo "built $APP"
