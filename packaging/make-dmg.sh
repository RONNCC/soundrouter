#!/usr/bin/env bash
# Build target/SoundRouter.dmg containing SoundRouter.app, an /Applications link and uninstall.sh.
set -euo pipefail
cd "$(dirname "$0")/.."

APP=target/SoundRouter.app
OUT=target/SoundRouter.dmg
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

[ -d "$APP" ] || { echo "missing $APP" >&2; exit 1; }

cp -R "$APP" "$WORK/"
ln -s /Applications "$WORK/Applications"
cp packaging/uninstall.sh "$WORK/uninstall.sh"
chmod 755 "$WORK/uninstall.sh"

rm -f "$OUT"
hdiutil create -volname SoundRouter -srcfolder "$WORK" -ov -format UDZO "$OUT"
echo "built $OUT"
