#!/bin/bash
# Live read-ahead matrix against the INSTALLED driver (no sudo), silent by design: only srtone (refuses
# non-virtual devices) and srctl's impulse probe write audio, both into soundrouter:* devices.
# Quit SoundRouter.app first (the engine and the app fight over the driver list).
#   packaging/readahead-matrix.sh engine|direct [-b "64 128 256 512"] [-m "0 1"] [-l 0|8] [-g] [-r RUNS=3] [-s SECS=60] [-o OUTDIR]
# engine: per cell (buffer x read-ahead off/on x CPU burners) `sr-cli run --buffer N [--read-ahead]` with v1 =
#   Pass-Thru (round trip V.out -> C.in -> mixer -> C.out -> V.in), srtone playing into v1, RUNS x SECS of
#   `srctl loop` on v1. One line per run, then a CELL line: engine delay readout, xruns, driver overloads, glitches.
# -g: engine cells only: no engine guard. The engine is started without --read-ahead and `sr-cli driver read-ahead on --force`
#   switches it on once srtone runs, so the engine's trip (it switches read-ahead off on the first overload) cannot hide later shortfalls.
# direct: no engine; the driver's own V loop, one device per buffer size (all sizes at once), read-ahead set
#   with `sr-cli driver read-ahead on|off`. (An engine removes devices it does not hold, so the two cannot share.)
# Restores the driver device list to what it was.
cd "$(dirname "$0")/.." || exit 1
MODE=${1:-engine}; shift
BUFS="64 128 256 512"; MODES="0 1"; GUARD=1; LOAD=0; RUNS=3; SECS=60; OUT=target/ra-matrix
while getopts b:m:l:r:s:o:g o; do case $o in g) GUARD=0 ;; b) BUFS=$OPTARG ;; m) MODES=$OPTARG ;; l) LOAD=$OPTARG ;; r) RUNS=$OPTARG ;; s) SECS=$OPTARG ;; o) OUT=$OPTARG ;; *) exit 2 ;; esac; done
CLI=$PWD/target/release/sr-cli; T=$PWD/target/sr-drvtools
[ -x "$CLI" ] && [ -x "$T/srctl" ] || { echo "build first: cargo build --release -p sr-cli; packaging/driver-dev.sh build"; exit 1; }
clang -O2 -Wall -framework CoreAudio -framework CoreFoundation -o "$T/srtone" packaging/srtone.c || exit 1
mkdir -p "$OUT"
"$CLI" driver get >"$OUT/orig.json" || { echo "driver not installed"; exit 1; }
cat >"$OUT/cfg.json" <<'JSON'
{"version":1,"devices":[
 {"id":"v1","name":"Loopback Audio","channels":2,"sources":[{"id":"p","kind":{"type":"pass_thru"},"channels":2}],"wires":[{"source":"p","src_ch":0,"out_ch":0},{"source":"p","src_ch":1,"out_ch":1}]}]}
JSON
pids=()
cleanup() { kill "${pids[@]}" 2>/dev/null; wait 2>/dev/null; "$CLI" driver read-ahead off >/dev/null 2>&1; "$CLI" driver set "$OUT/orig.json" >/dev/null 2>&1; }
trap cleanup EXIT
trap 'exit 130' INT TERM
burners() { local k; for ((k = 0; k < $1; k++)); do yes >/dev/null & pids+=($!); done; }
echo "machine: $(sysctl -n machdep.cpu.brand_string), macOS $(sw_vers -productVersion), $(date '+%F %H:%M'), mode $MODE"

if [ "$MODE" = direct ]; then
  specs=(); i=2; for b in $BUFS; do specs+=("soundrouter:v$i:SR Direct $b:2"); i=$((i + 1)); done
  "$T/srctl" set "${specs[@]}" >/dev/null || exit 1
  sleep 1
  for ra in $MODES; do
    pids=(); burners "$LOAD"
    "$CLI" driver read-ahead off >/dev/null 2>&1; [ "$ra" = 1 ] && "$CLI" driver read-ahead on --force
    for r in $(seq "$RUNS"); do
      i=2; for b in $BUFS; do
        "$T/srctl" loop "soundrouter:v$i" "$b" "$SECS" >"$OUT/direct-$b-ra$ra-load$LOAD.r$r" 2>&1 & pids+=($!); i=$((i + 1))
      done
      wait "${pids[@]:$LOAD}"
      st=$("$CLI" driver read-ahead 2>&1); want=off; [ "$ra" = 1 ] && want=on
      [ "$st" = "read-ahead: $want" ] || { echo "INVALID run: switch is now '$st' (someone else wrote it); others: $(pgrep -fl 'SoundRouter|sr-cli' | tr '\n' ';')"; "$CLI" driver read-ahead "$want" --force >/dev/null; }
      for b in $BUFS; do echo "direct ra$ra load$LOAD r$r | $(tr '\n' ' ' <"$OUT/direct-$b-ra$ra-load$LOAD.r$r")"; done
    done
    echo "STATE ra$ra load$LOAD | $("$CLI" driver read-ahead 2>&1)"
    kill "${pids[@]}" 2>/dev/null; wait 2>/dev/null
  done
  exit 0
fi

for load in $LOAD; do for buf in $BUFS; do for ra in $MODES; do
  tag=$buf-ra$ra-load$load; pids=()
  "$CLI" driver read-ahead off >/dev/null 2>&1
  "$CLI" driver set "$OUT/orig.json" >/dev/null 2>&1
  flag=""; [ "$ra" = 1 ] && [ "$GUARD" = 1 ] && flag="--read-ahead"
  "$CLI" -v run "$OUT/cfg.json" --buffer "$buf" --force $flag >"$OUT/$tag.run" 2>&1 & eng=$!; pids+=("$eng")
  burners "$load"
  sleep 4
  "$T/srtone" soundrouter:v1 $((RUNS * SECS + 30)) 0.2 0.1 >"$OUT/$tag.tone" 2>&1 & pids+=($!)
  sleep 1
  [ "$ra" = 1 ] && [ "$GUARD" = 0 ] && { "$CLI" driver read-ahead on --force >/dev/null; sleep 1; }
  for r in $(seq "$RUNS"); do
    "$T/srctl" loop soundrouter:v1 "$buf" "$SECS" >"$OUT/$tag.r$r" 2>&1
    st=$("$CLI" driver read-ahead 2>&1); want=off; [ "$ra" = 1 ] && want=on
    [ "$st" = "read-ahead: $want" ] || echo "NOTE: switch reads '$st' after this run (engine trip, or another process wrote it: $(pgrep -fl 'SoundRouter|sr-cli' | grep -v "^$eng " | tr '\n' ';'))"
    echo "$tag r$r | $(tr '\n' ' ' <"$OUT/$tag.r$r")"
  done
  state=$("$CLI" driver read-ahead 2>&1)
  kill -INT "$eng" 2>/dev/null; sleep 1; kill "${pids[@]}" 2>/dev/null; wait 2>/dev/null
  # engine view: median delay readout after 5 s, last glitches, last health line, read-ahead state before stop
  med=$(grep -E '^v1 \[running\]' "$OUT/$tag.run" | sed -n '20,$p' | sed -E 's/.*delay ([0-9.]+) ms.*/\1/' | sort -n | awk '{a[NR]=$1} END{print a[int((NR+1)/2)]}')
  gl=$(grep -E '^v1 \[' "$OUT/$tag.run" | tail -1 | sed -E 's/.*glitches ([0-9]+).*/\1/')
  echo "CELL $tag | $state | engine delay median ${med:-n/a} ms, glitches ${gl:-n/a} | $(grep '^health:' "$OUT/$tag.run" | tail -1) | $(grep -ci 'switching it off' "$OUT/$tag.run") trips"
done; done; done
