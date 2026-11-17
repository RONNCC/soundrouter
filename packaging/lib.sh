#!/usr/bin/env bash
# Shared helpers for the packaging scripts. Source after cd to the repo root:
#   . packaging/lib.sh
# SIGN_ID set   -> hardened runtime + secure timestamp (what notarization needs).
# SIGN_ID unset -> no runtime flag, no timestamp; nothing to notarize and it loads fine in coreaudiod. Signed with the
#   local dev identity if packaging/make-dev-identity.sh created it (stable identity: macOS keeps Microphone / System
#   Audio Recording grants across rebuilds), else ad-hoc (new identity every build, grants reset).

# [workspace.package] version from Cargo.toml: the single version source.
workspace_version() {
  awk -F'"' '/^\[/ { ws = ($0 == "[workspace.package]") } ws && /^version[ ]*=/ { print $2; exit }' Cargo.toml
}

# stamp_version <Info.plist>: short version from Cargo.toml, build number = commit count (BUILD_NUMBER overrides).
stamp_version() {
  local v b
  v=$(workspace_version)
  [ -n "$v" ] || { echo "no [workspace.package] version in Cargo.toml" >&2; return 1; }
  b=${BUILD_NUMBER:-$(git rev-list --count HEAD 2>/dev/null || echo 1)}
  plutil -replace CFBundleShortVersionString -string "$v" "$1"
  plutil -replace CFBundleVersion -string "$b" "$1"
}

# Self-signed, so only the unfiltered list shows it (`-v` hides it as untrusted); codesign -s <name> accepts it anyway.
DEV_ID_NAME="SoundRouter Local Dev"
dev_identity_exists() { security find-identity -p codesigning 2>/dev/null | grep -qF "\"$DEV_ID_NAME\""; }

if [ -z "${SIGN_ID:-}" ] && [ -z "${SR_SIGN_NOTED:-}" ]; then
  export SR_SIGN_NOTED=1
  if dev_identity_exists; then
    echo "signing with local identity '$DEV_ID_NAME' (stable across rebuilds)" >&2
  else
    echo "ad-hoc signing: macOS will re-ask for permissions after every rebuild; run packaging/make-dev-identity.sh once to stop that" >&2
  fi
fi

# sign_path <path> [entitlements.plist]
sign_path() {
  local args=()
  if [ -n "${SIGN_ID:-}" ]; then
    args=(--force --options runtime --timestamp)
    [ -z "${2:-}" ] || args+=(--entitlements "$2")
    codesign "${args[@]}" --sign "$SIGN_ID" "$1"
  elif dev_identity_exists; then
    codesign --force --sign "$DEV_ID_NAME" --timestamp=none "$1"
  else
    codesign --force --sign - --timestamp=none "$1"
  fi
}

# sign_driver <SoundRouter.driver>: the bundle executable is itself the dylib; sign it first, then the bundle.
sign_driver() {
  sign_path "$1/Contents/MacOS/SoundRouter"
  sign_path "$1"
}

# check_signed <path>: strict deep verify; with SIGN_ID also require hardened runtime and a secure timestamp.
check_signed() {
  codesign --verify --deep --strict "$1"
  if [ -n "${SIGN_ID:-}" ]; then
    local info
    info=$(codesign -dvv "$1" 2>&1)
    grep -q 'flags=0x[0-9a-f]*(.*runtime' <<<"$info" || { echo "$1: hardened runtime missing" >&2; return 1; }
    grep -q '^Timestamp=' <<<"$info" || { echo "$1: secure timestamp missing" >&2; return 1; }
  fi
}
