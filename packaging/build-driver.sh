#!/bin/bash
# Build + assemble + sign target/SoundRouter.driver. Run from anywhere.
# SIGN_ID set: hardened runtime + secure timestamp (notarizable). Unset: ad-hoc.
set -euo pipefail
[ -d "$HOME/.cargo/bin" ] && export PATH="$HOME/.cargo/bin:$PATH" || true
cd "$(dirname "$0")/.."
# shellcheck source=packaging/lib.sh
. packaging/lib.sh
export MACOSX_DEPLOYMENT_TARGET=14.0
OUT=target/SoundRouter.driver
cargo build --release -p soundrouter-driver
rm -rf "$OUT"
mkdir -p "$OUT/Contents/MacOS"
cp driver/Info.plist "$OUT/Contents/Info.plist"
stamp_version "$OUT/Contents/Info.plist"
cp target/release/libsoundrouter_driver.dylib "$OUT/Contents/MacOS/SoundRouter"
# the cargo dylib carries an absolute build-dir install name; make it bundle-relative
install_name_tool -id "@loader_path/SoundRouter" "$OUT/Contents/MacOS/SoundRouter"
plutil -lint "$OUT/Contents/Info.plist"
# factory symbol named in Info.plist must be exported
nm -gU "$OUT/Contents/MacOS/SoundRouter" | grep -q ' _SoundRouter_Create$' || { echo "missing _SoundRouter_Create" >&2; exit 1; }
sign_driver "$OUT"
check_signed "$OUT"
codesign --verify --strict --verbose=2 "$OUT"
echo "built $OUT"
