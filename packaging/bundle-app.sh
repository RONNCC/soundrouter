#!/usr/bin/env bash
# Build target/SoundRouter.app. Ad-hoc signed unless SIGN_ID is set.
set -euo pipefail
[ -d "$HOME/.cargo/bin" ] && export PATH="$HOME/.cargo/bin:$PATH" || true
cd "$(dirname "$0")/.."

APP=target/SoundRouter.app
DRV=$APP/Contents/Resources/SoundRouter.driver

# rustup shims first so rust-toolchain.toml selects the pinned toolchain
export PATH="$HOME/.cargo/bin:$PATH"
# macos-15 runners default to 15.0; keep the app runnable on the 14.2 minimum (matches build-driver.sh)
export MACOSX_DEPLOYMENT_TARGET=14.0

cargo build --release -p soundrouter-app -p sr-cli

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp packaging/Info-app.plist "$APP/Contents/Info.plist"
cp target/release/SoundRouter "$APP/Contents/MacOS/SoundRouter"
cp target/release/sr-cli "$APP/Contents/MacOS/sr-cli"
cp app/assets/icon.icns "$APP/Contents/Resources/icon.icns"
cp packaging/uninstall.sh "$APP/Contents/Resources/uninstall.sh"
chmod +x "$APP/Contents/Resources/uninstall.sh"

# driver for the in-app "Install Driver..." flow; keeps the signature build-driver.sh gave it
[ -d target/SoundRouter.driver ] || packaging/build-driver.sh
cp -R target/SoundRouter.driver "$APP/Contents/Resources/SoundRouter.driver"

if command -v cargo-about >/dev/null 2>&1; then
  cargo about generate -c packaging/about.toml packaging/about.hbs \
    -o "$APP/Contents/Resources/ThirdPartyLicenses.html"
elif [ "${REQUIRE_NOTICES:-}" = 1 ]; then
  echo "cargo-about missing (cargo install --locked cargo-about)" >&2; exit 1
else
  echo "warning: cargo-about missing; ThirdPartyLicenses.html not generated" >&2
fi

if [ -n "${SIGN_ID:-}" ]; then
  # inside-out: driver (same flags as build-driver.sh), sr-cli, then the app
  codesign --force --timestamp --sign "$SIGN_ID" "$DRV"
  for f in "$APP/Contents/MacOS/sr-cli" "$APP"; do
    codesign --force --options runtime --timestamp \
      --entitlements packaging/entitlements-app.plist \
      --sign "$SIGN_ID" "$f"
  done
else
  codesign --force --sign - --timestamp=none "$DRV"
  codesign --force --sign - "$APP/Contents/MacOS/sr-cli"
  codesign --force --sign - "$APP"
fi

codesign --verify --deep --strict "$DRV"
codesign --verify --deep --strict "$APP"
echo "built $APP"
