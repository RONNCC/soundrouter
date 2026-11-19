#!/usr/bin/env bash
# Build target/SoundRouter.pkg: driver into HAL, app into /Applications, uninstall.sh into Application Support.
# Needs target/SoundRouter.app (bundle-app.sh); the driver comes from the app's embedded copy,
# falling back to target/SoundRouter.driver (build-driver.sh).
# INSTALLER_ID ("Developer ID Installer: ...") signs the pkg; unset builds an unsigned pkg (not notarizable).
# The driver pkg replaces any older SoundRouter.driver wholesale (preinstall) and restarts coreaudiod (postinstall).
set -euo pipefail
[ -d "$HOME/.cargo/bin" ] && export PATH="$HOME/.cargo/bin:$PATH" || true
cd "$(dirname "$0")/.."

APP=target/SoundRouter.app
DRIVER=$APP/Contents/Resources/SoundRouter.driver
[ -d "$DRIVER" ] || DRIVER=target/SoundRouter.driver
OUT=target/SoundRouter.pkg
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

for p in "$DRIVER" "$APP"; do
  [ -d "$p" ] || { echo "missing $p" >&2; exit 1; }
done
VERSION=$(plutil -extract CFBundleShortVersionString raw "$APP/Contents/Info.plist")

mkdir -p "$WORK/hal" "$WORK/apps" "$WORK/scripts" "$WORK/support"
cp -R "$DRIVER" "$WORK/hal/"
cp -R "$APP" "$WORK/apps/"
cp packaging/uninstall.sh "$WORK/support/uninstall.sh"
chmod 755 "$WORK/support/uninstall.sh"

# an upgrade must not leave files of an older driver build inside the bundle (they break the code seal)
cat > "$WORK/scripts/preinstall" <<'EOF'
#!/bin/sh
rm -rf /Library/Audio/Plug-Ins/HAL/SoundRouter.driver
exit 0
EOF
cat > "$WORK/scripts/postinstall" <<'EOF'
#!/bin/sh
DRV=/Library/Audio/Plug-Ins/HAL/SoundRouter.driver
xattr -dr com.apple.quarantine "$DRV" 2>/dev/null || true
codesign --verify --strict "$DRV" || echo "SoundRouter: driver signature check failed; coreaudiod may refuse it" >&2
killall coreaudiod 2>/dev/null || true
exit 0
EOF
chmod +x "$WORK/scripts/preinstall" "$WORK/scripts/postinstall"

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

# Installer refuses Intel Macs (the binaries are arm64 only) and macOS < 14.2 (process taps).
cat > "$WORK/requirements.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>os</key><array><string>14.2</string></array>
<key>arch</key><array><string>arm64</string></array>
</dict></plist>
EOF
PKGS=(--package "$WORK/driver.pkg" --package "$WORK/app.pkg" --package "$WORK/support.pkg" --product "$WORK/requirements.plist")
rm -f "$OUT"
if [ -n "${INSTALLER_ID:-}" ]; then
  productbuild "${PKGS[@]}" --sign "$INSTALLER_ID" --timestamp "$OUT"
  pkgutil --check-signature "$OUT"
else
  echo "warning: INSTALLER_ID not set; building an UNSIGNED pkg (Gatekeeper blocks it, cannot be notarized)" >&2
  productbuild "${PKGS[@]}" "$OUT"
fi
echo "built $OUT"
