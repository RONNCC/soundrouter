#!/usr/bin/env bash
# Remove the SoundRouter audio driver. Standalone: needs nothing from the repo.
# Usage: uninstall.sh [--purge] [--dry-run] [--help]
set -euo pipefail

DRIVER=/Library/Audio/Plug-Ins/HAL/SoundRouter.driver
PKG_IDS="com.ronncc.soundrouter.driver.pkg com.ronncc.soundrouter.app.pkg com.ronncc.soundrouter.support.pkg"
LOGIN_LABEL=com.ronncc.soundrouter.login
PURGE=0
DRY=0

usage() {
  cat <<USAGE
Usage: uninstall.sh [--purge] [--dry-run] [--help]

Removes $DRIVER, restarts coreaudiod and forgets the
SoundRouter installer receipts. Safe to run repeatedly. Uses sudo unless root.

  --purge     also quit SoundRouter and remove ~/Library/Application Support/SoundRouter,
              ~/Library/Logs/SoundRouter and the login LaunchAgent
  --dry-run   print the actions without executing them
  --help      show this text
USAGE
}

for a in "$@"; do
  case $a in
    --purge) PURGE=1 ;;
    --dry-run) DRY=1 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $a" >&2; usage >&2; exit 2 ;;
  esac
done

# run a command (prefixed with sudo when not root) or just print it
run() {
  if [ "$DRY" = 1 ]; then echo "[dry-run] $*"; else "$@"; fi
}
say() { [ "$DRY" = 1 ] || echo "$*"; }
asroot() {
  if [ "$(id -u)" -eq 0 ]; then run "$@"; else run sudo "$@"; fi
}

# whose home to purge: the invoking user, even under sudo / an admin-privileged GUI helper
if [ "$(id -u)" -eq 0 ]; then
  TARGET_USER=${SUDO_USER:-$(stat -f %Su /dev/console)}
else
  TARGET_USER=$(id -un)
fi
# `|| true`: an unknown user (no console user) must not abort under set -e/pipefail before the driver is removed
TARGET_HOME=$(dscl . -read "/Users/$TARGET_USER" NFSHomeDirectory 2>/dev/null | tr '\n' ' ' | sed 's/^NFSHomeDirectory:[[:space:]]*//; s/[[:space:]]*$//' || true)  # not awk $2: homes may contain spaces
TARGET_HOME=${TARGET_HOME:-$HOME}

if [ -e "$DRIVER" ] || [ -L "$DRIVER" ]; then
  asroot rm -rf "$DRIVER"
  say "removed $DRIVER"
  if [ "$DRY" = 1 ]; then
    echo "[dry-run] killall coreaudiod"
  else
    asroot killall coreaudiod 2>/dev/null || true
    say "restarted coreaudiod"
  fi
else
  echo "driver not installed ($DRIVER)"
fi

for id in $PKG_IDS; do
  if pkgutil --pkg-info "$id" >/dev/null 2>&1; then
    asroot pkgutil --forget "$id" >/dev/null
    say "forgot receipt $id"
  fi
done

if [ "$PURGE" = 1 ]; then
  uid=$(id -u "$TARGET_USER")
  plist="$TARGET_HOME/Library/LaunchAgents/$LOGIN_LABEL.plist"
  if [ -e "$plist" ]; then
    run launchctl bootout "gui/$uid/$LOGIN_LABEL" 2>/dev/null || true
    run rm -f "$plist"
    say "removed $plist"
  fi
  # a running app writes its settings folder back with its next save
  run pkill -U "$uid" -x SoundRouter || true
  for d in "$TARGET_HOME/Library/Application Support/SoundRouter" "$TARGET_HOME/Library/Logs/SoundRouter"; do
    if [ -e "$d" ]; then
      run rm -rf "$d"
      say "removed $d"
    fi
  done
fi
echo "done"
