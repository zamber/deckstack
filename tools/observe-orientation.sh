#!/bin/sh
# observe-orientation.sh — DS-112 rehearsal: observe orientation / slot / position
# signals on the ds-vdev virtual device and simulate a module move / replug.
#
# PREP ONLY for ticket DS-112 (docs/tickets/2026-11-25-module-position-orientation.md).
# This script never touches hardware and never changes daemon behaviour; it only
# starts ds-vdev + ds-daemon on a private temp dir, logs events, and drives
# ds-vdevctl. Safe to run repeatedly (idempotent subcommands; see each one).
#
# Usage:
#   observe-orientation.sh <command> [options]
#
# Commands:
#   rehearse [--dir D] [--keep]   end-to-end local dry-run (default command):
#                                 boot vdev+daemon, watch events, flip base
#                                 orientation flat<->upright, push a key frame,
#                                 simulate a replug, assert the logged signals.
#                                 Exit 0 = all assertions green. Cleans up unless
#                                 --keep (or --dir given: leaves the stack for
#                                 interactive use with the other commands).
#   start    [--dir D]            boot ds-vdev + ds-daemon on D (default
#                                 $DS112_DIR or /tmp/ds112-vdev). Idempotent:
#                                 a healthy stack is left as-is ("already running").
#   stop     [--dir D] [--remove] stop the stack (daemon stop + TERM/KILL the
#                                 recorded pids). Idempotent; --remove also
#                                 deletes the dir. Never touches any other
#                                 daemon (patterns are pinned to the dir's socket).
#   status   [--dir D]            ds status + ds-vdevctl ping for the stack.
#   watch    [--dir D] [--seconds N]
#                                 log timestamped events of /deck/* (incl.
#                                 deck/device/<serial>/orientation|connected|
#                                 disconnected) to D/events.log. Appends.
#   orient   [--dir D] flat|upright
#                                 inject base orientation (input tag 0x2B payload
#                                 01=flat/horizontal, 00=upright/vertical via
#                                 feature 0x35 path) with ds-vdevctl inject.
#   replug   [--dir D]            simulate unplug+replug of a module: arm
#                                 ds-vdevctl drop-after, force a write past the
#                                 budget (disconnected event), then let the daemon
#                                 reconnect on its own (DS-120: NO daemon restart,
#                                 pid must be unchanged) with a fresh write budget,
#                                 verify the frame re-push sha256.
#   move     [--dir D]            closest available analogue of a module move
#                                 today: base rotation (orient flip) + replug,
#                                 plus a /devices snapshot. NOTE: the vdev has no
#                                 slot-change knob yet — the (slot, orientation)
#                                 identity remap is the DS-112 implementation, not
#                                 part of this prep.
#   snapshot [--dir D]            append timestamped ds get /devices, ds-vdevctl
#                                 reports/writes/frames to D/snapshots.log.
#
# Options:
#   --dir D     work dir for the stack (default: ${DS112_DIR:-/tmp/ds112-vdev});
#               rehearse uses a fresh mktemp dir unless --dir is given.
#   --keep      (rehearse) keep the stack + logs after the run.
#
# Environment:
#   DS_BIN_DIR    directory with ds / ds-daemon / ds-vdev / ds-vdevctl
#                 (default: <repo>/target/debug; built on demand if missing)
#   DS112_DIR     default work dir
#   DS_HARDWARE=1 refused — local rehearsal only.
#
# Exit codes: 0 ok / assertions green · 1 usage or assertion failure ·
#             2 daemon unreachable · 6 stack not running (where applicable)
set -u

SCRIPT=$(basename "$0")
REPO=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
DEFAULT_DIR=${DS112_DIR:-/tmp/ds112-vdev}

[ "${DS_HARDWARE:-0}" = "1" ] && { echo "$SCRIPT: refusing to run with DS_HARDWARE=1" >&2; exit 1; }

BIN_DIR=${DS_BIN_DIR:-$REPO/target/debug}
ensure_bins() {
    missing=""
    for b in ds ds-daemon ds-vdev ds-vdevctl; do
        [ -x "$BIN_DIR/$b" ] || missing="$missing $b"
    done
    if [ -n "$missing" ]; then
        echo "$SCRIPT: building missing binaries:$missing (cargo build --workspace)" >&2
        (cd "$REPO" && cargo build --workspace) >&2 || {
            echo "$SCRIPT: cargo build failed; set DS_BIN_DIR" >&2; exit 1; }
    fi
}
DS=$BIN_DIR/ds
DS_DAEMON=$BIN_DIR/ds-daemon
DS_VDEV=$BIN_DIR/ds-vdev
DS_VDEVCTL=$BIN_DIR/ds-vdevctl

ts() { date -u +%FT%T.%3NZ; }

usage() {
    sed -n '2,60p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

# ---- arg parsing -----------------------------------------------------------
CMD=${1:-rehearse}
[ $# -gt 0 ] && shift
DIR=""
DIR_SET=0
KEEP=0
REMOVE=0
SECONDS_ARG=""
ORIENT_ARG=""
while [ $# -gt 0 ]; do
    case "$1" in
        --dir) DIR=${2:?}; DIR_SET=1; shift 2;;
        --keep) KEEP=1; shift;;
        --remove) REMOVE=1; shift;;
        --seconds) SECONDS_ARG=${2:?}; shift 2;;
        -h|--help) usage 0;;
        flat|upright|horizontal|vertical) ORIENT_ARG=$1; shift;;
        *) echo "$SCRIPT: unknown argument: $1 (try --help)" >&2; exit 1;;
    esac
done

case $CMD in
    rehearse|start|stop|status|watch|orient|replug|move|snapshot) ;;
    -h|--help|help) usage 0;;
    *) echo "$SCRIPT: unknown command: $CMD (try --help)" >&2; exit 1;;
esac

[ -n "$DIR" ] || DIR=$DEFAULT_DIR
set_paths() {
    SOCK=$DIR/vdev.sock
    DATA=$DIR/data
    DSOCK=$DATA/daemon.sock
    EVENTS=$DIR/events.log
    SNAPS=$DIR/snapshots.log
}
set_paths

vdevctl() { "$DS_VDEVCTL" --socket "$SOCK" "$@"; }
ds() { DS_DAEMON_BIN=$DS_DAEMON "$DS" --socket "$DSOCK" --data-dir "$DATA" "$@"; }

vdev_alive() { vdevctl ping >/dev/null 2>&1; }
daemon_alive() { ds status >/dev/null 2>&1; }

wait_file() { # path timeout_s what
    i=0
    while [ $i -lt $((${2} * 20)) ]; do
        [ -e "$1" ] && return 0
        sleep 0.05; i=$((i + 1))
    done
    echo "$SCRIPT: timed out after ${2}s waiting for $3 ($1)" >&2
    return 1
}

cmd_start() {
    ensure_bins
    mkdir -p "$DIR"
    if vdev_alive && daemon_alive; then
        echo "$SCRIPT: already running (vdev=$SOCK daemon=$DSOCK)"
        return 0
    fi
    # clean any stale half-stack pinned to this dir's socket
    pkill -f "ds-vdev --socket $SOCK" 2>/dev/null
    rm -f "$SOCK" "$SOCK.ctl" "$DSOCK"
    setsid "$DS_VDEV" --socket "$SOCK" >"$DIR/vdev.log" 2>&1 </dev/null &
    wait_file "$SOCK" 10 "ds-vdev device socket" || return 1
    vdev_alive || { echo "$SCRIPT: ds-vdev control socket not answering" >&2; return 1; }
    DS_DAEMON_BIN=$DS_DAEMON setsid "$DS" daemon start \
        --device "virtual:$SOCK" --data-dir "$DATA" >"$DIR/daemon-start.log" 2>&1
    wait_file "$DSOCK" 10 "daemon control socket" || return 1
    i=0
    while ! daemon_alive; do
        i=$((i + 1)); [ $i -gt 100 ] && { echo "$SCRIPT: daemon did not come up; see $DATA/daemon.log" >&2; return 1; }
        sleep 0.1
    done
    echo "$SCRIPT: stack up: vdev=$SOCK daemon=$DSOCK (data-dir=$DATA)"
}

cmd_stop() {
    DS_DAEMON_BIN=$DS_DAEMON "$DS" --socket "$DSOCK" --data-dir "$DATA" daemon stop >/dev/null 2>&1
    if [ -f "$DATA/daemon.pid" ]; then
        p=$(cat "$DATA/daemon.pid" 2>/dev/null)
        [ -n "$p" ] && kill "$p" 2>/dev/null
    fi
    pkill -f "ds-daemon run.*--data-dir $DATA" 2>/dev/null
    pkill -f "ds-vdev --socket $SOCK" 2>/dev/null
    sleep 0.3
    pkill -9 -f "ds-vdev --socket $SOCK" 2>/dev/null
    if [ -n "$DATA" ] && [ -f "$DATA/daemon.pid" ]; then
        p=$(cat "$DATA/daemon.pid" 2>/dev/null); [ -n "$p" ] && kill -9 "$p" 2>/dev/null
    fi
    if [ "$REMOVE" = "1" ]; then
        rm -rf "$DIR"
        echo "$SCRIPT: stack stopped, $DIR removed"
    else
        echo "$SCRIPT: stack stopped (logs kept in $DIR)"
    fi
    return 0
}

cmd_status() {
    if vdev_alive; then echo "vdev:  up ($SOCK)"; else echo "vdev:  down"; fi
    if daemon_alive; then echo "daemon: up"; ds status || true; else echo "daemon: down (exit 6)"; return 6; fi
}

cmd_watch() {
    daemon_alive || { echo "$SCRIPT: daemon not running at $DSOCK (start first)" >&2; return 6; }
    S=${SECONDS_ARG:-30}
    echo "$SCRIPT: watching /deck/* for ${S}s -> $EVENTS"
    sh "$REPO/tools/watch-deck-events.sh" --socket "$DSOCK" --data-dir "$DATA" \
        --log "$EVENTS" --seconds "$S" --pattern "/deck/*"
}

cmd_orient() {
    daemon_alive || { echo "$SCRIPT: daemon not running at $DSOCK (start first)" >&2; return 6; }
    [ $# -gt 0 ] && ORIENT_ARG=$1
    case $ORIENT_ARG in
        flat|horizontal) O=horizontal;;
        upright|vertical) O=vertical;;
        *) echo "$SCRIPT: orient needs flat|upright (try --help)" >&2; return 1;;
    esac
    f=$DIR/orient-$O.ndjson
    printf '{"module":"base","orientation":"%s"}\n' "$O" > "$f"
    if [ "$O" = horizontal ]; then P=01; else P=00; fi
    echo "$(ts) orient: injecting base orientation=$O (input tag 0x2B payload $P, feature 0x35 path)"
    vdevctl inject "$f"
}

cmd_snapshot() {
    daemon_alive || { echo "$SCRIPT: daemon not running at $DSOCK (start first)" >&2; return 6; }
    {
        echo "=== $(ts) snapshot ==="
        echo "--- ds get /devices --json"; ds get /devices --json 2>&1
        echo "--- ds-vdevctl reports"; vdevctl reports 2>&1
        echo "--- ds-vdevctl writes 20"; vdevctl writes 20 2>&1
        echo "--- ds-vdevctl frames"; vdevctl frames 2>&1
    } >> "$SNAPS"
    echo "$SCRIPT: snapshot appended to $SNAPS"
    tail -20 "$SNAPS"
}

frame_sha() { vdevctl frames "$1" 2>/dev/null | awk '{print $2}'; }

cmd_replug() {
    daemon_alive || { echo "$SCRIPT: daemon not running at $DSOCK (start first)" >&2; return 6; }
    SHA1=$(frame_sha key:0)
    PID0=$(cat "$DATA/daemon.pid" 2>/dev/null)
    echo "$(ts) replug: baseline frame sha (key:0) = $SHA1; daemon pid=$PID0"
    echo "$(ts) replug: arming drop-after=1, then forcing a write past the budget"
    vdevctl throttle off 1
    ds set /controls/keys/1/image '{"kind":"color","value":"#aa0000"}' --json >/dev/null 2>&1
    # wait for the drop to actually land (the push is async on the device task) before
    # clearing the budget, else the replug reconnect could race the armed drop
    i=0; GONE=""
    while [ $i -lt 50 ]; do
        grep -q "device gone" "$DATA/daemon.log" 2>/dev/null && { GONE=1; break; }
        sleep 0.1; i=$((i + 1))
    done
    [ -n "$GONE" ] || { echo "$SCRIPT: replug: the armed drop never landed" >&2; return 1; }
    vdevctl throttle off off >/dev/null   # replug: the daemon's reconnect gets a fresh write budget
    sleep 2
    # DS-120: the daemon must survive the hot-unplug with the SAME pid (no restart)
    daemon_alive || { echo "$SCRIPT: replug: daemon died on hot-unplug (DS-120 regression)" >&2; return 1; }
    PID1=$(cat "$DATA/daemon.pid" 2>/dev/null)
    if [ "$PID0" != "$PID1" ]; then
        echo "$SCRIPT: replug: daemon pid changed $PID0 -> $PID1 (DS-120 regression)" >&2
        return 1
    fi
    echo "$(ts) replug: daemon survived the unplug, pid unchanged=$PID1"
    # the daemon reconnects on its own within the <= 5 s budget: wait for the re-push
    RE=0; i=0
    while [ $i -lt 50 ]; do
        tail -n 20 "$DATA/daemon.log" 2>/dev/null | grep -q "re-pushing" && { RE=1; break; }
        sleep 0.1; i=$((i + 1))
    done
    [ "$RE" = "1" ] || { echo "$SCRIPT: replug: no re-push logged within 5 s of the replug" >&2; return 1; }
    echo "$(ts) replug: reconnected + re-pushed cached targets (within 5 s budget)"
    sha2=$(frame_sha key:0)
    echo "$(ts) replug: frame sha after reconnect (key:0) = $sha2"
    [ "$sha2" = "$SHA1" ] || { echo "$SCRIPT: replug: frame sha changed $SHA1 -> $sha2" >&2; return 1; }
    echo "$(ts) replug: frame sha256 intact across the hot-unplug"
    cmd_snapshot
    return 0
}

cmd_move() {
    echo "$(ts) move: analogue of a module move/replug on today's vdev:"
    echo "$(ts) move: (1) base rotation (orientation flip) — the slot/port-path signal is"
    echo "$(ts) move:     NOT yet simulated by ds-vdev; (slot, orientation) identity remap"
    echo "$(ts) move:     is the DS-112 implementation (ticket req. 1/2/5), not this prep."
    [ -n "$ORIENT_ARG" ] || ORIENT_ARG=upright
    cmd_orient
    sleep 1
    cmd_replug
    cmd_snapshot
    echo "$(ts) move: current /devices slot signal:"
    ds get /devices --json 2>&1
}

# ---- rehearse --------------------------------------------------------------
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); echo "PASS: $*"; }
bad() { FAIL=$((FAIL+1)); echo "FAIL: $*"; }
check() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1 (want '$3' got '$2')"; fi; }

cmd_rehearse() {
    ensure_bins
    if [ "$DIR_SET" = "0" ]; then
        DIR=$(mktemp -d /tmp/ds112-rehearse-XXXXXX)
        set_paths
    fi
    echo "$SCRIPT: rehearsal dir: $DIR"
    cmd_start || { echo "$SCRIPT: rehearse: stack failed to start" >&2; return 1; }

    # background event watcher for the whole run
    sh "$REPO/tools/watch-deck-events.sh" --socket "$DSOCK" --data-dir "$DATA" \
        --log "$EVENTS" --seconds 20 --pattern "/deck/*" &
    WPID=$!
    sleep 1

    echo "--- baseline snapshot"
    cmd_snapshot >/dev/null
    ds status >/dev/null 2>&1 && ok "daemon status ok" || bad "daemon status"

    echo "--- step 1: rotate base upright (orientation vertical)"
    cmd_orient upright >/dev/null; sleep 1
    grep -q '/orientation' "$EVENTS" && grep '/orientation' "$EVENTS" | grep -q '"vertical"' \
        && ok "orientation event vertical logged" || bad "orientation event vertical"

    echo "--- step 2: rotate base flat (orientation horizontal)"
    cmd_orient flat >/dev/null; sleep 1
    grep '/orientation' "$EVENTS" | grep -q '"horizontal"' \
        && ok "orientation event horizontal logged" || bad "orientation event horizontal"

    echo "--- step 3: push a key frame, record sha"
    ds set /controls/keys/0/image '{"kind":"color","value":"#336699"}' --json >/dev/null
    sleep 1
    SHA1=$(frame_sha key:0)
    [ -n "$SHA1" ] && ok "frame key:0 pushed, sha256=$SHA1" || bad "frame key:0 pushed"

    echo "--- step 4: simulate module hot-unplug + replug (DS-120: no daemon restart)"
    PID_BEFORE=$(cat "$DATA/daemon.pid" 2>/dev/null)
    LOG_BEFORE=$(wc -l < "$DATA/daemon.log" 2>/dev/null || echo 0)
    vdevctl throttle off 1 >/dev/null
    ds set /controls/keys/1/image '{"kind":"color","value":"#aa0000"}' --json >/dev/null 2>&1
    # wait for the drop to actually land (the push is async on the device task) before
    # clearing the budget, else the reconnect could race the still-armed drop
    DISC=0; i=0
    while [ $i -lt 50 ]; do
        grep -q '/disconnected' "$EVENTS" && { DISC=1; break; }
        sleep 0.1; i=$((i + 1))
    done
    [ "$DISC" = "1" ] && ok "disconnected event logged" || bad "disconnected event"
    vdevctl throttle off off >/dev/null   # replug: the daemon's reconnect gets a fresh budget
    sleep 2
    # DS-120 no-restart assertion: same daemon pid, still alive
    PID_AFTER=$(cat "$DATA/daemon.pid" 2>/dev/null)
    check "daemon pid unchanged across hot-unplug" "$PID_AFTER" "$PID_BEFORE"
    daemon_alive && ok "daemon still alive after unplug (no restart)" || bad "daemon alive after unplug"
    # the daemon reconnects + re-handshakes + re-pushes on its own (<= 5 s budget)
    RE=0; i=0
    while [ $i -lt 50 ]; do
        tail -n +$((LOG_BEFORE + 1)) "$DATA/daemon.log" 2>/dev/null | grep -q "re-pushing" && { RE=1; break; }
        sleep 0.1; i=$((i + 1))
    done
    [ "$RE" = "1" ] && ok "re-handshake + re-push after replug within 5s (same pid)" \
        || bad "re-handshake + re-push after replug within 5s"
    sleep 1
    NC=$(grep -c '/connected' "$EVENTS" 2>/dev/null); NC=${NC:-0}
    # the boot-time connected event predates the watcher; >=1 here means the replug one
    [ "$NC" -ge 1 ] 2>/dev/null && ok "connected event after replug (connected events=$NC)" \
        || bad "connected event after replug (connected events=$NC)"
    SHA2=$(frame_sha key:0)
    check "frame re-push sha256 intact across hot-unplug (same daemon pid)" "$SHA2" "$SHA1"

    cmd_snapshot >/dev/null
    wait $WPID 2>/dev/null

    echo
    echo "=== captured events ($EVENTS) ==="
    cat "$EVENTS"
    echo
    echo "=== result: $PASS passed, $FAIL failed ==="
    if [ "$FAIL" -gt 0 ]; then
        echo "$SCRIPT: rehearsal FAILED (logs in $DIR)"
        return 1
    fi
    if [ "$KEEP" = "1" ] || [ "$DIR_SET" = "1" ]; then
        echo "$SCRIPT: rehearsal green; stack kept in $DIR (stop with: $0 stop --dir $DIR)"
    else
        cmd_stop >/dev/null
        rm -rf "$DIR"
        echo "$SCRIPT: rehearsal green; cleaned up $DIR"
    fi
    return 0
}

case $CMD in
    rehearse) cmd_rehearse;;
    start) cmd_start;;
    stop) cmd_stop;;
    status) cmd_status;;
    watch) cmd_watch;;
    orient) cmd_orient;;
    replug) cmd_replug;;
    move) cmd_move;;
    snapshot) cmd_snapshot;;
esac
