#!/usr/bin/env bash
# Build target/SoundRouter.pkg: driver into HAL, app into /Applications.
# Needs target/SoundRouter.driver (build-driver.sh) and target/SoundRouter.app (bundle-app.sh).
# productsign only when INSTALLER_ID is set.
set -euo pipefail
cd "$(dirname "$0")/.."

DRIVER=target/SoundRouter.driver
APP=target/SoundRouter.app
OUT=target/SoundRouter.pkg
VERSION=0.1.0
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

for p in "$DRIVER" "$APP"; do
  [ -d "$p" ] || { echo "missing $p" >&2; exit 1; }
done

mkdir -p "$WORK/hal" "$WORK/apps" "$WORK/scripts"
cp -R "$DRIVER" "$WORK/hal/"
cp -R "$APP" "$WORK/apps/"

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

pkgbuild --root "$WORK/apps" \
  --install-location /Applications \
  --identifier com.ronncc.soundrouter.app.pkg \
  --version "$VERSION" \
  "$WORK/app.pkg"

rm -f "$OUT"
if [ -n "${INSTALLER_ID:-}" ]; then
  productbuild --package "$WORK/driver.pkg" --package "$WORK/app.pkg" \
    --sign "$INSTALLER_ID" "$OUT"
else
  productbuild --package "$WORK/driver.pkg" --package "$WORK/app.pkg" "$OUT"
fi
echo "built $OUT"
