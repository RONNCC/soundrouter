#!/usr/bin/env bash
# Generate app/assets/icon.png and icon.icns (stdlib python + sips + iconutil). Idempotent.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
assets="$root/app/assets"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$assets"

python3 "$root/packaging/gen_icon.py" "$assets/icon.png"

set_dir="$work/icon.iconset"
mkdir -p "$set_dir"
for s in 16 32 128 256 512; do
  sips -z "$s" "$s" "$assets/icon.png" --out "$set_dir/icon_${s}x${s}.png" >/dev/null
  sips -z "$((s * 2))" "$((s * 2))" "$assets/icon.png" --out "$set_dir/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$set_dir" -o "$assets/icon.icns"
echo "wrote $assets/icon.png and $assets/icon.icns"
