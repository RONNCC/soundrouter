#!/usr/bin/env bash
# Create the self-signed code-signing identity "SoundRouter Local Dev" in your login keychain, once.
# packaging/lib.sh then signs local builds with it, so the app keeps one code identity across rebuilds and macOS
# stops re-asking for Microphone / System Audio Recording. Not for distribution (not notarizable).
# Idempotent. KEYCHAIN_PASSWORD (login password) lets us pre-authorize codesign; otherwise it is prompted when
# interactive, else skipped (macOS then asks "codesign wants to use key" on the first build: choose Always Allow).
set -euo pipefail
cd "$(dirname "$0")/.."
export SR_SIGN_NOTED=1 # lib.sh's "ad-hoc" nag is for the build scripts, not this one
# shellcheck source=packaging/lib.sh
. packaging/lib.sh

W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
KC=$(security default-keychain | tr -d ' "')
if dev_identity_exists; then
  echo "identity '$DEV_ID_NAME' already exists; nothing to do"
else
  PW=$(openssl rand -hex 16)
  cat > "$W/cert.cnf" <<CNF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $DEV_ID_NAME
[ext]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
CNF
  # /usr/bin/openssl (LibreSSL): its PKCS#12 output is what `security import` reads; Homebrew OpenSSL 3's is not
  /usr/bin/openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -config "$W/cert.cnf" \
    -keyout "$W/key.pem" -out "$W/cert.pem" 2>/dev/null
  /usr/bin/openssl pkcs12 -export -inkey "$W/key.pem" -in "$W/cert.pem" -name "$DEV_ID_NAME" \
    -passout "pass:$PW" -out "$W/id.p12"
  security import "$W/id.p12" -k "$KC" -P "$PW" -T /usr/bin/codesign -T /usr/bin/security >/dev/null
  echo "created '$DEV_ID_NAME' (10 years) in $KC"

  # Without a partition list, codesign prompts for the keychain password on every build.
  KP=${KEYCHAIN_PASSWORD:-}
  if [ -z "$KP" ] && [ -t 0 ]; then
    read -rsp "login keychain password (Enter to skip; only used to let codesign use the key silently): " KP; echo
  fi
  if [ -n "$KP" ] && security set-key-partition-list -S apple-tool:,apple:,codesign: -s -l "$DEV_ID_NAME" -k "$KP" "$KC" >/dev/null 2>&1; then
    echo "codesign pre-authorized for the key"
  else
    echo "note: partition list not set (needs the login password: KEYCHAIN_PASSWORD=... or run on a terminal). If a build ever shows 'codesign wants to use key', enter your login password and choose Always Allow" >&2
  fi
fi

dev_identity_exists || { echo "identity not visible to codesign" >&2; exit 1; }
# smoke test: a copy of /bin/true signed with the identity must verify (also proves codesign needs no prompt)
cp /usr/bin/true "$W/t"
if ! { timeout 30 codesign --force --sign "$DEV_ID_NAME" --timestamp=none "$W/t" && codesign --verify --strict "$W/t"; }; then
  echo "test signing failed or is waiting on a keychain dialog; allow it and re-run" >&2; exit 1
fi
cat <<MSG
ok. packaging/bundle-app.sh, build-driver.sh and driver-dev.sh now sign with '$DEV_ID_NAME' automatically
(unless SIGN_ID is set). It shows as "not trusted" in 'security find-identity -v'; that is expected and harmless.
Grant Microphone and System Audio Recording once to the first build signed this way; rebuilds keep them.
MSG
