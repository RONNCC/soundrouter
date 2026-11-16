#!/usr/bin/env bash
# Build target/SoundRouter.pkg: driver into HAL, app into /Applications, uninstall.sh into Application Support.
# Needs target/SoundRouter.app (bundle-app.sh); the driver comes from the app's embedded copy,
# falling back to target/SoundRouter.driver (build-driver.sh).
# productsign only when INSTALLER_ID is set.
set -euo pipefail
[ -d "$HOME/.cargo/bin" ] && export PATH="$HOME/.cargo/bin:$PATH" || true
cd "$(dirname "$0")/.."

APP=target/SoundRouter.app
DRIVER=$APP/Contents/Resources/SoundRouter.driver
[ -d "$DRIVER" ] || DRIVER=target/SoundRouter.driver
OUT=target/SoundRouter.pkg
VERSION=$(plutil -extract CFBundleShortVersionString raw packaging/Info-app.plist)
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

for p in "$DRIVER" "$APP"; do
  [ -d "$p" ] || { echo "missing $p" >&2; exit 1; }
done

mkdir -p "$WORK/hal" "$WORK/apps" "$WORK/scripts" "$WORK/support"
cp -R "$DRIVER" "$WORK/hal/"
cp -R "$APP" "$WORK/apps/"
cp packaging/uninstall.sh "$WORK/support/uninstall.sh"
chmod 755 "$WORK/support/uninstall.sh"

cat > "$WORK/scripts/postinstall" <<'EOF'
#!/bin/sh
killall coreaudiod 2>/dev/null || true
exit 0
EOF
chmod +x "$WORK/scripts/postinstall"

pkgbuild --root "$WORK/hal" \
  --install-location /Library/Audio/Plug-Ins/HAL \
  --identifier com.ronncc.soundrouter.driver.pkg \
  --version "$VERSION" \
  --scripts "$WORK/scripts" \
  "$WORK/driver.pkg"

# non-relocatable: Installer must not "upgrade" a stray SoundRouter.app elsewhere
pkgbuild --analyze --root "$WORK/apps" "$WORK/app.plist"
plutil -replace 0.BundleIsRelocatable -bool NO "$WORK/app.plist"

pkgbuild --root "$WORK/apps" \
  --component-plist "$WORK/app.plist" \
  --install-location /Applications \
  --identifier com.ronncc.soundrouter.app.pkg \
  --version "$VERSION" \
  "$WORK/app.pkg"

pkgbuild --root "$WORK/support" \
  --install-location "/Library/Application Support/SoundRouter" \
  --identifier com.ronncc.soundrouter.support.pkg \
  --version "$VERSION" \
  "$WORK/support.pkg"

rm -f "$OUT"
if [ -n "${INSTALLER_ID:-}" ]; then
  productbuild --package "$WORK/driver.pkg" --package "$WORK/app.pkg" --package "$WORK/support.pkg" \
    --sign "$INSTALLER_ID" "$OUT"
else
  productbuild --package "$WORK/driver.pkg" --package "$WORK/app.pkg" --package "$WORK/support.pkg" "$OUT"
fi
echo "built $OUT"
