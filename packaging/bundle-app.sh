#!/usr/bin/env bash
# Build target/SoundRouter.app. Ad-hoc signed unless SIGN_ID is set (then hardened runtime + timestamp).
set -euo pipefail
[ -d "$HOME/.cargo/bin" ] && export PATH="$HOME/.cargo/bin:$PATH" || true
cd "$(dirname "$0")/.."
# shellcheck source=packaging/lib.sh
. packaging/lib.sh

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
stamp_version "$APP/Contents/Info.plist"
cp target/release/SoundRouter "$APP/Contents/MacOS/SoundRouter"
cp target/release/sr-cli "$APP/Contents/MacOS/sr-cli"
cp app/assets/icon.icns "$APP/Contents/Resources/icon.icns"
cp LICENSE-MIT LICENSE-APACHE "$APP/Contents/Resources/"
cp packaging/uninstall.sh "$APP/Contents/Resources/uninstall.sh"
chmod +x "$APP/Contents/Resources/uninstall.sh"

# driver for the in-app "Install Driver..." flow; always rebuilt so its version and signing match this run
packaging/build-driver.sh
cp -R target/SoundRouter.driver "$APP/Contents/Resources/SoundRouter.driver"

if command -v cargo-about >/dev/null 2>&1; then
  cargo about generate -c packaging/about.toml packaging/about.hbs \
    -o "$APP/Contents/Resources/ThirdPartyLicenses.html"
elif [ "${REQUIRE_NOTICES:-}" = 1 ]; then
  echo "cargo-about missing (cargo install --locked cargo-about)" >&2; exit 1
else
  echo "warning: cargo-about missing; ThirdPartyLicenses.html not generated" >&2
fi

# inside-out: sr-cli, then the app (the driver keeps the signature build-driver.sh gave it)
sign_path "$APP/Contents/MacOS/sr-cli" packaging/entitlements-app.plist
sign_path "$APP" packaging/entitlements-app.plist

check_signed "$DRV"
check_signed "$APP"
echo "built $APP"
