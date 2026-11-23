#!/bin/sh
# Conformance test for the ds-focus sender (ticket DS-060, docs/components/focus-sender.md).
#
# Black-box, sh only: boots ds-vdev + ds-daemon on a fresh temp dir (vdev device), then
# drives `ds-focus --inject '{"app":"code","title":"x"}'` against that daemon and asserts:
#
#   1. ds-focus exits 0 and logs the emit at info level (stderr JSON log lines);
#   2. the event arrives on `ds watch /app/focus/changed --ndjson --max-events 1` with
#      exactly the injected payload (op=event, topic=app/focus/changed, ts_ms, no rev);
#   3. `ds get /focus --json` reflects the payload when the daemon build carries the S7
#      mirror (DS-044); on a tree without the mirror yet the get is E_PATH/sentinel and
#      the script says so and still passes (the sender contract is the emit, asserted in 2);
#   4. `--inject <file.ndjson>` mode replays several payloads in order.
#
# The script always cleans up (daemon stop, SIGTERM/SIGKILL of vdev, temp dir removed).
# If the binaries cannot be built/found and no cargo is available, it prints SKIP and
# exits 0 (--inject needs no compositor, so a missing compositor alone never skips).
#
# Usage: sh tests/conformance/focus_sender.sh
# Env:   DS_BIN_DIR  binary directory (default <repo>/target/debug)

set -u

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
BINS=${DS_BIN_DIR:-$ROOT/target/debug}
BOOT_TIMEOUT_S=30
EVENT_TIMEOUT_S=10
SETTLE_S=1

say() { printf '%s\n' "$*"; }
fail() { printf 'FAIL %s\n' "$*"; exit 1; }

need_bin() {
    [ -x "$BINS/$1" ] || return 1
}

# --- locate or build the binaries (bounded) ------------------------------------
if need_bin ds && need_bin ds-daemon && need_bin ds-vdev && need_bin ds-focus; then
    :
else
    if command -v cargo >/dev/null 2>&1; then
        say "building binaries in $BINS (bounded)"
        (cd "$ROOT" && cargo build --bin ds --bin ds-daemon --bin ds-vdev --bin ds-focus) \
            >/dev/null 2>&1 || (cd "$ROOT" && cargo build --bin ds --bin ds-daemon --bin ds-vdev --bin ds-focus) \
            || fail "cargo build of ds/ds-daemon/ds-vdev/ds-focus failed"
    else
        say "SKIP: binaries missing from $BINS and no cargo on PATH (ds-focus --inject unavailable)"
        exit 0
    fi
    need_bin ds && need_bin ds-daemon && need_bin ds-vdev && need_bin ds-focus \
        || { say "SKIP: binaries still missing from $BINS (ds-focus --inject unavailable)"; exit 0; }
fi

DS="$BINS/ds"
DS_FOCUS="$BINS/ds-focus"
export DS_DAEMON_BIN="$BINS/ds-daemon"

TMP=$(mktemp -d "${TMPDIR:-/tmp}/ds-focus-conf.XXXXXX") || fail "mktemp failed"
VDEV_SOCK="$TMP/vdev.sock"
DATA="$TMP/data"
SOCK="$DATA/daemon.sock"
VDEV_PID=""
WATCH_PID=""

cleanup() {
    # daemon stop first (graceful), then signals; always remove the temp dir
    if [ -S "$SOCK" ]; then
        "$DS" --socket "$SOCK" --data-dir "$DATA" daemon stop >/dev/null 2>&1
    fi
    for pid in $WATCH_PID $VDEV_PID; do
        [ -n "$pid" ] && kill "$pid" >/dev/null 2>&1
    done
    # bounded wait, then SIGKILL
    i=0
    while [ $i -lt 20 ]; do
        alive=0
        for pid in $WATCH_PID $VDEV_PID; do
            [ -n "$pid" ] && kill -0 "$pid" >/dev/null 2>&1 && alive=1
        done
        [ "$alive" -eq 0 ] && break
        sleep 0.25
        i=$((i + 1))
    done
    for pid in $WATCH_PID $VDEV_PID; do
        [ -n "$pid" ] && kill -9 "$pid" >/dev/null 2>&1
    done
    rm -rf "$TMP"
}
trap cleanup EXIT INT TERM

ds() { "$DS" --socket "$SOCK" --data-dir "$DATA" "$@"; }

# --- boot the stack on a vdev ---------------------------------------------------
say "booting ds-vdev + ds-daemon on $TMP"
"$BINS/ds-vdev" --socket "$VDEV_SOCK" >"$TMP/vdev.log" 2>&1 &
VDEV_PID=$!
i=0
while [ ! -S "$VDEV_SOCK" ]; do
    i=$((i + 1)); [ $i -gt $((BOOT_TIMEOUT_S * 10)) ] && { cat "$TMP/vdev.log"; fail "vdev socket never appeared"; }
    sleep 0.1
done
"$DS" daemon start --device "virtual:$VDEV_SOCK" --data-dir "$DATA" >"$TMP/boot.out" 2>&1 \
    || { cat "$TMP/boot.out"; cat "$DATA/daemon.log" 2>/dev/null; fail "ds daemon start failed"; }
i=0
while [ ! -S "$SOCK" ]; do
    i=$((i + 1)); [ $i -gt $((BOOT_TIMEOUT_S * 10)) ] && fail "daemon socket never appeared"
    sleep 0.1
done
say "stack up: socket=$SOCK"

# --- seed /focus with a sentinel when the state accepts writes ------------------
# (the S7 mirror that overwrites /focus from app/focus/changed lands with DS-044; on a
# daemon without it the value stays the sentinel and the script reports that below)
SENTINEL='{"app":"__unset__","title":"__unset__"}'
ds set /focus "$SENTINEL" --json >"$TMP/seed.out" 2>&1 && SEED_OK=1 || SEED_OK=0

# --- phase 1: inline --inject, one event on ds watch ----------------------------
say "phase 1: ds-focus --inject '{\"app\":\"code\",\"title\":\"x\"}' + ds watch --ndjson"
ds watch /app/focus/changed --ndjson --max-events 1 >"$TMP/watch1.out" 2>"$TMP/watch1.err" &
WATCH_PID=$!
sleep $SETTLE_S   # let the subscription reach the daemon

"$DS_FOCUS" --socket "$SOCK" --inject '{"app":"code","title":"x"}' >"$TMP/focus1.out" 2>"$TMP/focus1.err"
FOCUS_RC=$?
[ "$FOCUS_RC" -eq 0 ] || { cat "$TMP/focus1.err"; fail "ds-focus --inject exited $FOCUS_RC (want 0)"; }

# bounded wait for the event line
i=0
while ! grep -q '"topic":"app/focus/changed"' "$TMP/watch1.out" 2>/dev/null; do
    i=$((i + 1)); [ $i -gt $((EVENT_TIMEOUT_S * 10)) ] && { cat "$TMP/watch1.out" "$TMP/watch1.err"; fail "no app/focus/changed event within ${EVENT_TIMEOUT_S}s"; }
    sleep 0.1
done
# let the watcher exit on its --max-events 1
i=0
while kill -0 $WATCH_PID >/dev/null 2>&1 && [ $i -lt 50 ]; do sleep 0.1; i=$((i + 1)); done
kill $WATCH_PID >/dev/null 2>&1
WATCH_PID=""

LINES=$(grep -c . "$TMP/watch1.out" || true)
[ "$LINES" -eq 1 ] || { cat "$TMP/watch1.out"; fail "expected exactly 1 watch line, got $LINES"; }
LINE=$(grep . "$TMP/watch1.out" | head -n 1)
for needle in '"op":"event"' '"topic":"app/focus/changed"' '"path":"/app/focus/changed"' \
              '"payload"' '"app":"code"' '"title":"x"' '"ts_ms":'; do
    case "$LINE" in *"$needle"*) ;; *) fail "event line missing $needle: $LINE";; esac
done
case "$LINE" in *'"rev"'*) fail "non-state event must not carry rev: $LINE";; *) ;; esac
say "PASS event on watch: $LINE"

# every emit is logged at info
grep -q '"level":"info"' "$TMP/focus1.err" || { cat "$TMP/focus1.err"; fail "no info-level log line from ds-focus"; }
grep -q 'app/focus/changed' "$TMP/focus1.err" || { cat "$TMP/focus1.err"; fail "emit not logged at info"; }
say "PASS ds-focus logged the emit at info"

# --- /focus mirror check ---------------------------------------------------------
say "checking ds get /focus --json"
FOCUS_JSON=$(ds get /focus --json 2>"$TMP/focusget.err")
GET_RC=$?
if [ "$GET_RC" -eq 0 ]; then
    case "$FOCUS_JSON" in
        *'"app":"code"'*|*'"app": "code"'*)
            case "$FOCUS_JSON" in
                *'"title":"x"'*|*'"title": "x"'*) say "PASS /focus mirrored the payload: $FOCUS_JSON" ;;
                *) fail "/focus has app=code but not title=x: $FOCUS_JSON" ;;
            esac ;;
        *)
            if [ "$SEED_OK" -eq 1 ]; then
                say "NOTE: /focus did not change (still '$FOCUS_JSON') — the S7 /focus mirror lands with DS-044 (in flight); the emit contract is asserted via watch above"
            else
                say "NOTE: /focus readable but not mirrored in this daemon build (DS-044 in flight): $FOCUS_JSON"
            fi ;;
    esac
else
    say "NOTE: ds get /focus --json exited $GET_RC in this daemon build (S7 mirror lands with DS-044, in flight); emit contract asserted via watch above"
fi

# --- phase 2: --inject <file.ndjson> replays in order ----------------------------
say "phase 2: --inject file.ndjson with two payloads"
printf '%s\n%s\n' '{"app":"code","title":"a"}' '{"app":"term","title":"b"}' >"$TMP/ev.ndjson"
ds watch /app/focus/changed --ndjson --max-events 2 >"$TMP/watch2.out" 2>"$TMP/watch2.err" &
WATCH_PID=$!
sleep $SETTLE_S
"$DS_FOCUS" --socket "$SOCK" --inject "$TMP/ev.ndjson" >"$TMP/focus2.out" 2>"$TMP/focus2.err" \
    || { cat "$TMP/focus2.err"; fail "ds-focus --inject <file> failed"; }
i=0
while [ "$(grep -c '"topic":"app/focus/changed"' "$TMP/watch2.out" 2>/dev/null || echo 0)" -lt 2 ]; do
    i=$((i + 1)); [ $i -gt $((EVENT_TIMEOUT_S * 10)) ] && { cat "$TMP/watch2.out"; fail "expected 2 events within ${EVENT_TIMEOUT_S}s"; }
    sleep 0.1
done
i=0
while kill -0 $WATCH_PID >/dev/null 2>&1 && [ $i -lt 50 ]; do sleep 0.1; i=$((i + 1)); done
kill $WATCH_PID >/dev/null 2>&1
WATCH_PID=""
L1=$(grep '"topic":"app/focus/changed"' "$TMP/watch2.out" | sed -n '1p')
L2=$(grep '"topic":"app/focus/changed"' "$TMP/watch2.out" | sed -n '2p')
case "$L1" in *'"app":"code"'*'"title":"a"'*) ;; *) fail "first ndjson payload not delivered in order: $L1";; esac
case "$L2" in *'"app":"term"'*'"title":"b"'*) ;; *) fail "second ndjson payload not delivered in order: $L2";; esac
say "PASS ndjson inject delivered both payloads in order"

say "RESULT: OK"
exit 0
