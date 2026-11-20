#!/bin/bash
# Live engine check against the INSTALLED driver (no sudo), silent by design: srtone -> process tap ->
# engine aggregate (hidden companion C + tap) -> mixer -> C.out -> ring B -> V.in, measured with `sr-cli meter`.
# Needs soundrouter:v1 installed and System Audio Recording granted to this terminal.
# Nothing here can reach a speaker: the 1 kHz tone (L peak 0.2, R peak 0.1) is played by `srtone` into the
# virtual device soundrouter:v2 (srtone refuses any other than soundrouter:* / BlackHole), the engine taps
# that process and routes the tap into v1. No default device is changed. The driver device list is set to
# v1 + v2 for the run and restored to what it was on exit (also on failure or Ctrl-C).
set -u
cd "$(dirname "$0")/.." || exit 1
export PATH="$HOME/.cargo/bin:$PATH"
cargo build -q -p sr-cli || exit 1
CLI=$PWD/target/debug/sr-cli
W=$(mktemp -d) && W=$(cd "$W" && pwd -P) || exit 1 # physical path: the tap keys the player by its real executable path
SRTONE=$W/srtone
clang -O2 -Wall -framework CoreAudio -framework CoreFoundation -o "$SRTONE" packaging/srtone.c || exit 1
PLAYER=soundrouter:v2 # player target: a virtual device that nothing reads, so the tap is feedback-free
case $PLAYER in soundrouter:*) ;; *) echo "refusing: player target '$PLAYER' is not a soundrouter: device"; exit 1 ;; esac
"$CLI" driver get >"$W/orig.json" || { echo "driver not installed"; rm -rf "$W"; exit 1; }
# shellcheck disable=SC2046
cleanup() { kill $(jobs -p) 2>/dev/null; wait 2>/dev/null; "$CLI" driver set "$W/orig.json" >/dev/null 2>&1; rm -rf "$W"; }
trap cleanup EXIT
trap 'exit 130' INT TERM
fails=0
check() { # name got want
  if [ "$2" = "$3" ]; then echo "ok   $1: $2"; else echo "FAIL $1: got '$2', want '$3'"; fails=$((fails + 1)); fi
}
play() { # start the tone player on $PLAYER (sets $ap) and prove from its own output that it opened that device
  "$SRTONE" "$PLAYER" 30 0.2 0.1 >"$W/play.log" 2>&1 & ap=$!
  for _ in $(seq 50); do grep -q "($PLAYER) ch 0-1" "$W/play.log" && break; sleep 0.1; done # start takes 0.25-0.6 s, longer under load
  grep -q "($PLAYER) ch 0-1" "$W/play.log" || { echo "FAIL player did not open $PLAYER: $(cat "$W/play.log")"; fails=$((fails + 1)); }
}

# v1 = device under test, v2 = player target. The engine syncs the driver list to the config, so the config lists both.
cat >"$W/devices.json" <<'JSON'
[{"uid":"soundrouter:v1","name":"Loopback Audio","channels":2},{"uid":"soundrouter:v2","name":"SR Test Player","channels":2}]
JSON
"$CLI" driver set "$W/devices.json" >/dev/null || { echo "FAIL driver set"; exit 1; }

python3 - "$W" "$SRTONE" <<'PY'
import json, sys
w, player = sys.argv[1], sys.argv[2]
both = [(0, 0), (1, 1)]
v2 = {"id": "v2", "name": "SR Test Player", "channels": 2, "sources": [], "wires": []}
def cfg(name, vol=0.0, muted=False, wires=both, sources=None):
    src = sources if sources is not None else [{"id": "s1", "kind": {"type": "app", "bundle_id": f"path:{player}"},
                                               "channels": 2, "volume_db": vol, "muted": muted}]
    dev = {"id": "v1", "name": "Loopback Audio", "channels": 2, "sources": src,
           "wires": [{"source": src[0]["id"], "src_ch": s, "out_ch": o} for s, o in wires] if src else []}
    json.dump({"version": 1, "devices": [dev, v2]}, open(f"{w}/{name}.json", "w"))
cfg("unity"); cfg("minus6", vol=-6.0); cfg("muted", muted=True)
cfg("route01", wires=[(0, 1)]); cfg("swap", wires=[(0, 1), (1, 0)]); cfg("empty", sources=[])
PY

# Meter lines (dB per channel, 4 Hz) must settle on exactly `want` for the last 3 lines.
# L -14.0 dB = 0.2, R -20.0 dB = 0.1; -6 dB gain gives -20.0 / -26.0.
while read -r name want; do
  play
  "$CLI" -v run "$W/$name.json" >"$W/$name.run" 2>&1 & run=$!
  sleep 4
  "$CLI" meter v1 >"$W/$name.meter" 2>&1 & mt=$!
  sleep 3
  kill -INT $mt $run; wait $mt $run 2>/dev/null; kill $ap; wait $ap 2>/dev/null
  check "$name" "$(tail -n 3 "$W/$name.meter" | sort -u | tr -s ' ' | sed 's/^ //')" "$want"
  grep -q "aggregate main device 'soundrouter:v1.engine'" "$W/$name.run" || { echo "FAIL $name: engine did not use the hidden companion"; fails=$((fails + 1)); }
done <<'EOF'
unity -14.0 -20.0
minus6 -20.0 -26.0
muted -inf -inf
route01 -inf -14.0
swap -20.0 -14.0
EOF

# Stopping `sr-cli run` must drop V's input back to the direct loop (silent while only the player plays, into v2).
play
"$CLI" run "$W/unity.json" >/dev/null 2>&1 & run=$!
sleep 4
"$CLI" meter v1 >"$W/revert.meter" 2>&1 & mt=$!
sleep 2
n=$(wc -l <"$W/revert.meter")
check "revert: before stop" "$(sed -n "$((n - 1)),${n}p" "$W/revert.meter" | sort -u | tr -s ' ' | sed 's/^ //')" "-14.0 -20.0"
kill -INT $run; wait $run 2>/dev/null
sleep 2
kill -INT $mt; wait $mt 2>/dev/null; kill $ap; wait $ap 2>/dev/null
check "revert: after stop" "$(tail -n 3 "$W/revert.meter" | sort -u | tr -s ' ' | sed 's/^ //')" "-inf -inf"

# V.out -> V.in loop: works with no engine, is replaced by ring B while an (empty) engine runs.
"$CLI" loopback-test v1 --secs 2 >/dev/null 2>&1; check "loopback-test, no engine (exit)" "$?" 0
"$CLI" run "$W/empty.json" >/dev/null 2>&1 & run=$!
sleep 3
"$CLI" loopback-test v1 --secs 2 >/dev/null 2>&1; check "loopback-test, engine running (exit)" "$?" 1
"$CLI" doctor >/dev/null 2>&1; check "doctor while engine running (exit)" "$?" 0
check "SoundRouter devices in system_profiler (companion hidden)" \
  "$(system_profiler SPAudioDataType | grep -c 'Manufacturer: SoundRouter')" "$("$CLI" driver get | grep -c '"uid"')"
kill -INT $run; wait $run 2>/dev/null

if [ "$fails" = 0 ]; then echo "PASS"; else echo "$fails check(s) FAILED"; exit 1; fi
