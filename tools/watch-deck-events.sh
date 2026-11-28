#!/bin/sh
# watch-deck-events.sh — log timestamped deck events from a running ds-daemon.
#
# Part of the DS-112 preparation (docs/components/module-orientation-procedure.md).
# Works against ANY daemon: the ds-vdev rehearsal stack started by
# tools/observe-orientation.sh, or a real deck daemon later (hardware session).
# It only reads events; it never writes state and never touches hardware.
#
# Usage:
#   watch-deck-events.sh --socket PATH --data-dir DIR --log FILE [--seconds N] [--pattern /deck/*]
#   watch-deck-events.sh --help
#
# Arguments:
#   --socket PATH    daemon control socket (e.g. $DS112_DIR/data/daemon.sock)
#   --data-dir DIR   daemon data dir (for the token file)
#   --log FILE       append timestamped NDJSON events here (created if missing)
#   --seconds N      stop after N seconds (default: 0 = run until Ctrl-C/SIGTERM)
#   --pattern P      topic pattern to subscribe (default: /deck/* — every deck/… topic,
#                    incl. deck/device/<serial>/orientation|connected|disconnected)
#
# Environment:
#   DS_BIN_DIR       directory with the ds binary (default: <repo>/target/debug)
#   DS_HARDWARE=1    refused: this script is for local rehearsal / supervised
#                    hardware observation only, never for unattended hardware runs
#
# Idempotent: appends to the log file; running it twice just adds a second,
# independent watcher (stop the old one with Ctrl-C or kill its pid). Safe to
# re-run after a daemon restart — it exits non-zero if the daemon is unreachable.
set -u

SCRIPT=$(basename "$0")
REPO=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

usage() {
    sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

[ "${DS_HARDWARE:-0}" = "1" ] && { echo "$SCRIPT: refusing to run with DS_HARDWARE=1" >&2; exit 1; }

SOCKET="" DATA_DIR="" LOG="" SECONDS=0 PATTERN="/deck/*"
while [ $# -gt 0 ]; do
    case "$1" in
        --socket) SOCKET=${2:?}; shift 2;;
        --data-dir) DATA_DIR=${2:?}; shift 2;;
        --log) LOG=${2:?}; shift 2;;
        --seconds) SECONDS=${2:?}; shift 2;;
        --pattern) PATTERN=${2:?}; shift 2;;
        -h|--help) usage 0;;
        *) echo "$SCRIPT: unknown argument: $1 (try --help)" >&2; exit 1;;
    esac
done
[ -n "$SOCKET" ] && [ -n "$DATA_DIR" ] && [ -n "$LOG" ] || { echo "$SCRIPT: --socket, --data-dir and --log are required (try --help)" >&2; exit 1; }

BIN_DIR=${DS_BIN_DIR:-$REPO/target/debug}
DS=$BIN_DIR/ds
[ -x "$DS" ] || { echo "$SCRIPT: ds binary not found at $DS (build with: cargo build --workspace, or set DS_BIN_DIR)" >&2; exit 1; }

mkdir -p "$(dirname -- "$LOG")"
printf '%s %s watcher attached socket=%s pattern=%s\n' \
    "$(date -u +%FT%T.%3NZ)" "$SCRIPT" "$SOCKET" "$PATTERN" >> "$LOG"

watch_loop() {
    "$DS" --socket "$SOCKET" --data-dir "$DATA_DIR" watch "$PATTERN" --ndjson 2>>"$LOG" \
        | while IFS= read -r line; do
            printf '%s %s\n' "$(date -u +%FT%T.%3NZ)" "$line" >> "$LOG"
        done
}

if [ "$SECONDS" -gt 0 ] 2>/dev/null; then
    timeout "$SECONDS" "$DS" --socket "$SOCKET" --data-dir "$DATA_DIR" watch "$PATTERN" --ndjson 2>>"$LOG" \
        | while IFS= read -r line; do
            printf '%s %s\n' "$(date -u +%FT%T.%3NZ)" "$line" >> "$LOG"
        done
else
    watch_loop
fi
rc=$?
printf '%s %s watcher detached rc=%s\n' "$(date -u +%FT%T.%3NZ)" "$SCRIPT" "$rc" >> "$LOG"
exit "$rc"
