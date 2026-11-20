#!/usr/bin/env bash
# uninstall.sh against a fake home and stubbed system tools (sudo, dscl, pkgutil, killall, launchctl, pkill).
# Nothing outside a temp dir changes; refuses to run as root, where uninstall.sh would skip the sudo stub.
set -euo pipefail
cd "$(dirname "$0")/.."
[ "$(id -u)" -ne 0 ] || { echo "run as a normal user" >&2; exit 1; }

T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
HOME_DIR="$T/Home Dir ü" # spaces and non-ASCII
mkdir -p "$T/bin"
for tool in sudo killall launchctl pkill; do
  printf '#!/bin/sh\necho "%s $*" >> "%s/calls"\n' "$tool" "$T" > "$T/bin/$tool"
done
printf '#!/bin/sh\necho "NFSHomeDirectory: %s"\n' "$HOME_DIR" > "$T/bin/dscl"
printf '#!/bin/sh\nexit 0\n' > "$T/bin/pkgutil" # every receipt "installed"; --forget runs through the sudo stub
chmod +x "$T"/bin/*

fail() { echo "FAIL: $*" >&2; exit 1; }
seed() {
  mkdir -p "$HOME_DIR/Library/Application Support/SoundRouter" "$HOME_DIR/Library/Logs/SoundRouter" \
    "$HOME_DIR/Library/LaunchAgents"
  touch "$HOME_DIR/Library/LaunchAgents/com.ronncc.soundrouter.login.plist"
  : > "$T/calls"
}
un() { PATH="$T/bin:$PATH" packaging/uninstall.sh "$@"; }

rc=0; un --bogus 2>/dev/null || rc=$?
[ "$rc" = 2 ] || fail "unknown option exited $rc, want 2"

seed
un --purge --dry-run >/dev/null
[ -d "$HOME_DIR/Library/Application Support/SoundRouter" ] || fail "--dry-run deleted settings"
[ ! -s "$T/calls" ] || fail "--dry-run ran: $(cat "$T/calls")"

seed
un --purge >/dev/null
for p in "Application Support/SoundRouter" "Logs/SoundRouter" "LaunchAgents/com.ronncc.soundrouter.login.plist"; do
  [ ! -e "$HOME_DIR/Library/$p" ] || fail "--purge left $p"
done
# a running app would write its settings folder back with its next save
grep -qx "pkill -U $(id -u) -x SoundRouter" "$T/calls" || fail "--purge did not quit the app: $(cat "$T/calls")"
grep -q "^sudo pkgutil --forget com.ronncc.soundrouter.driver.pkg" "$T/calls" || fail "receipt not forgotten"
echo "uninstall.sh: ok"
