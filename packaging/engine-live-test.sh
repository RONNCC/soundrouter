#!/bin/bash
# Live engine check against the INSTALLED driver (no sudo): afplay -> process tap -> engine aggregate
# (hidden companion C + tap) -> mixer -> C.out -> ring B -> V.in, measured with `sr-cli meter`.
# Needs soundrouter:v1 (2 ch) installed and System Audio Recording granted to this terminal.
# Plays a quiet 1 kHz tone (L peak 0.2, R peak 0.1) on the default output; sets nothing system-wide.
set -u
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
cargo build -q -p sr-cli || exit 1
CLI=$PWD/target/debug/sr-cli
W=$(mktemp -d)
trap 'kill $(jobs -p) 2>/dev/null; rm -rf "$W"' EXIT
fails=0
check() { # name got want
  if [ "$2" = "$3" ]; then echo "ok   $1: $2"; else echo "FAIL $1: got '$2', want '$3'"; fails=$((fails + 1)); fi
}

python3 - "$W" <<'PY'
import json, math, struct, sys, wave
w = sys.argv[1]
wv = wave.open(f"{w}/tone.wav", "wb"); wv.setnchannels(2); wv.setsampwidth(2); wv.setframerate(48000)
wv.writeframes(b"".join(struct.pack("<hh", round(s * 0.2 * 32767), round(s * 0.1 * 32767))
                        for s in (math.sin(2 * math.pi * 1000 * i / 48000) for i in range(48000 * 30))))
wv.close()
both = [(0, 0), (1, 1)]
def cfg(name, vol=0.0, muted=False, wires=both, sources=None):
    src = sources if sources is not None else [{"id": "s1", "kind": {"type": "app", "bundle_id": "path:/usr/bin/afplay"},
                                               "channels": 2, "volume_db": vol, "muted": muted}]
    dev = {"id": "v1", "name": "Loopback Audio", "channels": 2, "sources": src,
           "wires": [{"source": src[0]["id"], "src_ch": s, "out_ch": o} for s, o in wires] if src else []}
    json.dump({"version": 1, "devices": [dev]}, open(f"{w}/{name}.json", "w"))
cfg("unity"); cfg("minus6", vol=-6.0); cfg("muted", muted=True)
cfg("route01", wires=[(0, 1)]); cfg("swap", wires=[(0, 1), (1, 0)]); cfg("empty", sources=[])
PY

# Meter lines (dB per channel, 4 Hz) must settle on exactly `want` for the last 3 lines.
# L -14.0 dB = 0.2, R -20.0 dB = 0.1; -6 dB gain gives -20.0 / -26.0.
while read -r name want; do
  afplay "$W/tone.wav" & ap=$!
  sleep 0.5
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

# Stopping `sr-cli run` must drop V's input back to the direct loop (silent while only afplay plays elsewhere).
afplay "$W/tone.wav" & ap=$!
sleep 0.5
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

[ "$fails" = 0 ] && echo "PASS" || { echo "$fails check(s) FAILED"; exit 1; }
