#!/bin/sh
# DS-027 live-deck demo: drive the real deck (ds daemon on hid:), set key:0 to a
# known colour and the knob LEDs to green, then run the deckstack-verify pipeline
# (devices -> capture -> calibrate -> compare/check) against the camera.
#
# Usage (one shell invocation — the daemon must live inside it):
#   DS_BIN_DIR=<dir with ds/ds-daemon> sh tools/verify/demo/run_live_demo.sh
# Env: DSV_PANEL="x,y,w,h"  panel hint in image pixels (default 505,150,285,345,
#       measured for the 1280x720 AUKEY frame on the reference setup; see
#       tools/verify/README.md "Calibration").
set -u
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd)
cd "$ROOT"
BINS=${DS_BIN_DIR:-$ROOT/../target/debug}
V="python3 $ROOT/tools/verify/deckstack_verify.py"
DEMO=$ROOT/tools/verify/demo
mkdir -p "$DEMO"
D=/tmp/dsv-demo-run
PANEL=${DSV_PANEL:-505,150,285,345}
rm -rf "$D"; mkdir -p "$D"
DS="$BINS/ds"

echo "== binaries: $BINS ($(ls "$BINS/ds" "$BINS/ds-daemon" 2>/dev/null | tr '\n' ' '))"
echo "== starting daemon on hid:"
"$DS" daemon start --device hid: --data-dir "$D" || exit 1
i=0; while [ $i -lt 40 ]; do
  "$DS" --socket "$D/daemon.sock" --data-dir "$D" daemon status --json 2>/dev/null \
    | grep -q '"device_status": "connected' && break
  i=$((i+1)); sleep 0.5
done
"$DS" --socket "$D/daemon.sock" --data-dir "$D" daemon status --json \
  | python3 -c "import json,sys; d=json.load(sys.stdin); print('daemon:', d.get('device_status'), 'pid', d.get('pid'))"

echo "== set key:0 image #ff0000 + brightness 100, knob LEDs #00ff00"
"$DS" --socket "$D/daemon.sock" --data-dir "$D" set /controls/keys/0/image \
  --json '{"kind":"color","value":"#ff0000"}'
"$DS" --socket "$D/daemon.sock" --data-dir "$D" set /controls/keys/0/brightness 100
for n in 0 1 2; do
  "$DS" --socket "$D/daemon.sock" --data-dir "$D" set /controls/knobs/$n/led \
    --json '{"mode":"static","color":"#00ff00","brightness":100}'
done
sleep 2
echo "== daemon write log (pushed lines):"
grep '"pushed' "$D/daemon.log" | tail -5

echo "== deckstack-verify devices:"
$V devices

echo "== deckstack-verify capture --out $DEMO/frame.jpg:"
$V capture --out "$DEMO/frame.jpg" --settle 800 || { echo "CAPTURE FAILED"; kill "$(cat "$D/daemon.pid")"; exit 1; }

echo "== deckstack-verify calibrate (--panel $PANEL):"
$V calibrate --frame "$DEMO/frame.jpg" --out "$DEMO/calibration.json" \
  --overlay "$DEMO/overlay.png" --panel "$PANEL"

echo "== deckstack-verify compare --expected key:0=#ff0000,led:1=#00ff00 (json):"
$V compare --expected "key:0=#ff0000,led:1=#00ff00" --actual "$DEMO/frame.jpg" \
  --calibration "$DEMO/calibration.json" --json
echo "compare rc=$? (7 = mismatch, expected here for key:0 — see README 'Limits')"

echo "== deckstack-verify compare --expected led:1=#00ff00 (LED zone only):"
$V compare --expected "led:1=#00ff00" --actual "$DEMO/frame.jpg" \
  --calibration "$DEMO/calibration.json"
echo "compare led rc=$?"

echo "== deckstack-verify check --target key:0 --expected key:0=#ff0000:"
$V check --target key:0 --expected "key:0=#ff0000" --calibration "$DEMO/calibration.json" \
  --keep "$DEMO/check-frame.jpg" --settle 800
echo "check rc=$? (7 = mismatch: the M4315 LCD does not render pushed images on this unit — see README 'Limits')"

echo "== deckstack-verify compare --expected $DEMO/frame.jpg (golden) --actual check-frame.jpg:"
echo "   (second capture of the unchanged deck; per-target noise must be within tolerance)"
$V compare --expected "$DEMO/frame.jpg" --actual "$DEMO/check-frame.jpg" \
  --calibration "$DEMO/calibration.json"
echo "golden-compare rc=$?"

echo "== stopping daemon (SIGTERM):"
kill "$(cat "$D/daemon.pid")"; sleep 1
tail -3 "$D/daemon.log"
"$DS" --socket "$D/daemon.sock" --data-dir "$D" daemon status >/dev/null 2>&1
echo "ds daemon status after stop rc=$? (6 = E_UNREACHABLE, expected)"
echo "== demo artifacts:"
ls -l "$DEMO"
