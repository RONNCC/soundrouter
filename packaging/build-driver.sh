#!/bin/bash
# Build + assemble + ad-hoc (or SIGN_ID) sign target/SoundRouter.driver. Run from anywhere.
set -euo pipefail
[ -d "$HOME/.cargo/bin" ] && export PATH="$HOME/.cargo/bin:$PATH" || true
cd "$(dirname "$0")/.."
export MACOSX_DEPLOYMENT_TARGET=14.0
OUT=target/SoundRouter.driver
cargo build --release -p soundrouter-driver
rm -rf "$OUT"
mkdir -p "$OUT/Contents/MacOS"
cp driver/Info.plist "$OUT/Contents/Info.plist"
cp target/release/libsoundrouter_driver.dylib "$OUT/Contents/MacOS/SoundRouter"
# the cargo dylib carries an absolute build-dir install name; make it bundle-relative
install_name_tool -id "@loader_path/SoundRouter" "$OUT/Contents/MacOS/SoundRouter"
plutil -lint "$OUT/Contents/Info.plist"
# factory symbol named in Info.plist must be exported
nm -gU "$OUT/Contents/MacOS/SoundRouter" | grep -q ' _SoundRouter_Create$' || { echo "missing _SoundRouter_Create" >&2; exit 1; }
if [ -n "${SIGN_ID:-}" ]; then
  codesign --force --timestamp --sign "$SIGN_ID" "$OUT"
else
  codesign --force --sign - --timestamp=none "$OUT"
fi
codesign --verify --strict --verbose=2 "$OUT"
echo "built $OUT"
