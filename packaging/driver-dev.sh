#!/bin/bash
# build | fakehost | install | uninstall | restart | test for the SoundRouter HAL driver.
# install/uninstall/restart/test use sudo internally: run this script as your user, not with sudo.
set -euo pipefail
cd "$(dirname "$0")/.."
NAME=SoundRouter
DEST=/Library/Audio/Plug-Ins/HAL/$NAME.driver
OUT=target/$NAME.driver
TOOLS=${TMPDIR:-/tmp}/sr-drvtools

tools() {
  mkdir -p "$TOOLS"
  clang -O2 -Wall -framework CoreAudio -framework CoreFoundation -o "$TOOLS/fakehost" packaging/fakehost.c
  clang -O2 -Wall -framework CoreAudio -framework CoreFoundation -o "$TOOLS/srctl" packaging/srctl.c
}
build() { packaging/build-driver.sh; tools; }
fakehost() { build; "$TOOLS/fakehost" "$PWD/$OUT"; }   # no sudo: CFPlugIn load + vtable drive
restart() { sudo killall coreaudiod || true; sleep 3; } # launchd respawns it; audio drops ~1-2 s
install() {
  build
  sudo rm -rf "$DEST"
  sudo ditto "$OUT" "$DEST"
  sudo chown -R root:wheel "$DEST"
  sudo xattr -dr com.apple.quarantine "$DEST" 2>/dev/null || true
  restart
}
uninstall() { sudo rm -rf "$DEST"; restart; }

step() { printf '\n== %s\n' "$*"; }
fail() { echo "FAIL: $*"; exit 1; }
profiler() { system_profiler SPAudioDataType; }
test_live() {
  [ -x "$TOOLS/srctl" ] || tools
  local srctl=$TOOLS/srctl log=/tmp/sr-coreaudiod.log lp
  log stream --level debug --predicate 'process == "coreaudiod" AND (eventMessage CONTAINS[c] "SoundRouter" OR eventMessage CONTAINS "PlugIn")' >"$log" 2>&1 &
  lp=$!; trap 'kill $lp 2>/dev/null || true' EXIT; sleep 1

  step "1/8 driver loaded by coreaudiod"
  "$srctl" get || fail "not loaded; see $log and: codesign -dv $DEST"
  step "2/8 runtime create (no restart)"
  "$srctl" set "soundrouter:v1:SoundRouter 1:2" "soundrouter:v2:SoundRouter 2:8"; sleep 1
  profiler | grep -q "SoundRouter 2" || fail "device not listed"
  step "3/8 companions hidden"
  "$srctl" list
  [ "$("$srctl" list | grep -c '\.engine$')" = 0 ] || fail "companion visible"
  profiler | grep -q "\.engine" && fail "companion in system_profiler"
  step "4/8 loopback latency (expect ~2x buffer)"
  for n in 512 128 32; do "$srctl" loop soundrouter:v1 "$n"; done
  step "5/8 channel change 8->16 while IO runs"
  "$srctl" loop soundrouter:v2 128 & sleep 1
  "$srctl" set "soundrouter:v1:SoundRouter 1:2" "soundrouter:v2:SoundRouter 2:16"; wait || true
  "$srctl" loop soundrouter:v2 128
  profiler | grep -A8 "SoundRouter 2" | grep -i -E "channels|SoundRouter"
  step "6/8 rename"
  "$srctl" set "soundrouter:v1:Renamed 1:2" "soundrouter:v2:SoundRouter 2:16"; sleep 1
  profiler | grep -q "Renamed 1" || fail "rename not visible"
  step "7/8 persistence across coreaudiod restart (sudo)"
  restart; "$srctl" get; profiler | grep -q "SoundRouter 2" || fail "devices lost after restart"
  step "8/8 remove all"
  "$srctl" set; sleep 1
  profiler | grep -q "SoundRouter 2" && fail "devices still present"
  echo "removed OK; log: $log"
}

case "${1:-}" in
  build) build ;; fakehost) fakehost ;; install) install ;; uninstall) uninstall ;; restart) restart ;;
  test) test_live ;;
  *) echo "usage: $0 build|fakehost|install|uninstall|restart|test"; exit 2 ;;
esac
