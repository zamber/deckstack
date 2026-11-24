#!/usr/bin/env python3
"""Micro-app: now-playing strip — first MPRIS player on the session bus → keys 0-2 + dial.

Teaches the API calls: `ds set <path> <value>` on key/dial label + image, `ds get
<path> --json` (read back prior values for cleanup), the global `--socket` /
`--data-dir` flags, and how to drive an external probe (`gdbus` / `dbus-send`)
argv-only from a micro-app — never a shell.

D-Bus access (stdlib only, no dbus bindings): the app shells out argv-only to
`gdbus call` when present, else `dbus-send`, and parses the printed reply with a
tolerant regex (both tools' text forms are handled). It lists bus names via
`org.freedesktop.DBus.ListNames`, picks the first `org.mpris.MediaPlayer2.*`
player (sorted), and reads `org.freedesktop.DBus.Properties.GetAll` on
`org.mpris.MediaPlayer2.Player` for `PlaybackStatus`, `xesam:title` and
`xesam:artist`.

**Documented no-player path (the tested default in CI):** when neither tool is
on PATH, the session bus is unreachable, no `org.mpris.MediaPlayer2.*` name is
on the bus, or the property read fails, the app renders the fallback sample
`{"player": null, "title": "no player", "artist": "", "status": "stopped"}`
(grey key 0, "no player" on key 1 and the dial) and exits 0 — a deck without a
player shows "no player", never an error.

Key map: 0 = playback state (playing #00cc44 / paused #ffaa00 / stopped or no
player #888888) · 1 = track title · 2 = artist · dial = state label + title as a
text visual. At most 1 update per second (--interval >= 1.0, the 1 Hz attention
rule — docs/microapps.md §7).

    python3 examples/microapps/now_playing.py --once
    python3 examples/microapps/now_playing.py --loop --interval 2
    python3 examples/microapps/now_playing.py --socket /tmp/deck/daemon.sock \
        --data-dir /tmp/deck --loop

--loop polls the bus at --interval and restores the keys/dial it wrote to their
previous values on exit (Ctrl-C); fields that did not exist before are left at
the last value (there is no unset op — docs/microapps.md §8).
"""

import argparse
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import time

KEY_STATE, KEY_TITLE, KEY_ARTIST = 0, 1, 2
DIAL = "/controls/dial"
WRITTEN = tuple(
    "/controls/keys/%d/%s" % (k, leaf)
    for k in (KEY_STATE, KEY_TITLE, KEY_ARTIST)
    for leaf in ("label", "image")
) + (DIAL + "/label", DIAL + "/image")
MAX_TEXT = 32  # text visual value / label maxLength (schemas/state.json)
PLAYER_RE = re.compile(r"org\.mpris\.MediaPlayer2\.[\w.\-]+")

NO_PLAYER = {"player": None, "title": "no player", "artist": "", "status": "stopped"}
STATUS_COLOR = {"playing": "#00cc44", "paused": "#ffaa00"}
DIM = "#888888"


class DSError(RuntimeError):
    def __init__(self, argv, proc):
        self.exit_code = proc.returncode
        super().__init__(
            "ds %s failed [exit %d]: %s"
            % (" ".join(argv), proc.returncode, proc.stderr.strip() or proc.stdout.strip())
        )


def ds(ds_bin, socket, data_dir, args, timeout=15):
    cmd = [ds_bin]
    if socket:
        cmd += ["--socket", socket]
    if data_dir:
        cmd += ["--data-dir", data_dir]
    cmd += [str(a) for a in args]
    return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)


def ds_ok(ds_bin, socket, data_dir, args, timeout=15):
    p = ds(ds_bin, socket, data_dir, args, timeout)
    if p.returncode != 0:
        raise DSError(args, p)
    return p


def get_json(ds_bin, socket, data_dir, path):
    p = ds(ds_bin, socket, data_dir, ["get", path, "--json", "--compact"])
    if p.returncode == 2:
        return None
    if p.returncode != 0:
        raise DSError(["get", path], p)
    return json.loads(p.stdout)["value"]


def set_json(ds_bin, socket, data_dir, path, value):
    ds_ok(ds_bin, socket, data_dir, ["set", path, "--json", json.dumps(value)])


def clip(s):
    return (s or "")[:MAX_TEXT]


def dbus_tool():
    """The D-Bus CLI probe to use: gdbus preferred, dbus-send fallback, else None."""
    if shutil.which("gdbus"):
        return "gdbus"
    if shutil.which("dbus-send"):
        return "dbus-send"
    return None


def run_dbus(tool, args, timeout=10):
    """Run one D-Bus CLI call argv-only; returns stdout, or None on any failure
    (missing bus, no reply — all mean "no player data" on this path)."""
    if tool == "gdbus":
        cmd = ["gdbus", "call", "--session"] + args
    else:
        cmd = ["dbus-send", "--session", "--print-reply"] + args
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    except (OSError, subprocess.TimeoutExpired):
        return None
    return p.stdout if p.returncode == 0 else None


def list_players(tool):
    if tool == "gdbus":
        out = run_dbus(tool, ["--dest", "org.freedesktop.DBus",
                              "--object-path", "/org/freedesktop/DBus",
                              "--method", "org.freedesktop.DBus.ListNames"])
    else:
        out = run_dbus(tool, ["--dest=org.freedesktop.DBus", "/org/freedesktop/DBus",
                              "org.freedesktop.DBus.ListNames"])
    if not out:
        return []
    return sorted(set(PLAYER_RE.findall(out)))


def extract_prop(props_text, key):
    """The value string for `key` in a gdbus `<'...'>` / dbus-send
    `variant string "..."` reply. Tolerant on purpose; None when absent."""
    i = props_text.find(key)
    if i < 0:
        return None
    rest = props_text[i + len(key):]
    m = (re.search(r"<\[?['\"]([^'\"]*)['\"]", rest)         # gdbus <'v'> / array <['v',…]>
         or re.search(r"string\s+['\"]([^'\"]*)['\"]", rest)  # dbus-send string "value"
         or re.search(r"['\"]([^'\"]*)['\"]", rest))          # bare quoted
    return m.group(1) if m else None


def player_props(tool, player):
    if tool == "gdbus":
        return run_dbus(tool, ["--dest", player,
                               "--object-path", "/org/mpris/MediaPlayer2",
                               "--method", "org.freedesktop.DBus.Properties.GetAll",
                               "org.mpris.MediaPlayer2.Player"])
    return run_dbus(tool, ["--dest=" + player, "/org/mpris/MediaPlayer2",
                           "org.freedesktop.DBus.Properties.GetAll",
                           "string:org.mpris.MediaPlayer2.Player"])


def sample():
    """One now-playing sample; the documented no-player fallback when the bus,
    the probe tool, or a player is missing."""
    tool = dbus_tool()
    if tool is None:
        return dict(NO_PLAYER)
    players = list_players(tool)
    if not players:
        return dict(NO_PLAYER)
    player = players[0]
    props = player_props(tool, player)
    if not props:
        return dict(NO_PLAYER)
    status = (extract_prop(props, "PlaybackStatus") or "Stopped").lower()
    title = extract_prop(props, "xesam:title") or "unknown"
    artist = extract_prop(props, "xesam:artist") or ""
    return {"player": player, "title": title, "artist": artist, "status": status}


def render(ds_bin, socket, data_dir, s):
    if s["player"] is None:
        state_label, color = "no player", DIM
    else:
        state_label = s["status"]
        color = STATUS_COLOR.get(s["status"], DIM)
    title, artist = clip(s["title"]), clip(s["artist"]) or "-"
    set_json(ds_bin, socket, data_dir, "/controls/keys/%d/label" % KEY_STATE, state_label)
    set_json(ds_bin, socket, data_dir, "/controls/keys/%d/image" % KEY_STATE,
             {"kind": "color", "value": color})
    set_json(ds_bin, socket, data_dir, "/controls/keys/%d/label" % KEY_TITLE, title)
    set_json(ds_bin, socket, data_dir, "/controls/keys/%d/image" % KEY_TITLE,
             {"kind": "color", "value": "#0066cc"})
    set_json(ds_bin, socket, data_dir, "/controls/keys/%d/label" % KEY_ARTIST, artist)
    set_json(ds_bin, socket, data_dir, "/controls/keys/%d/image" % KEY_ARTIST,
             {"kind": "color", "value": "#0066cc"})
    set_json(ds_bin, socket, data_dir, DIAL + "/label", state_label)
    set_json(ds_bin, socket, data_dir, DIAL + "/image",
             {"kind": "text", "value": title, "fg": "#ffffff", "bg": color})
    return {"player": s["player"], "title": clip(s["title"]),
            "artist": clip(s["artist"]), "status": s["status"]}


def main():
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    mode = ap.add_mutually_exclusive_group()
    mode.add_argument("--once", action="store_true", default=True,
                      help="read the bus once, render, exit (default)")
    mode.add_argument("--loop", action="store_true",
                      help="poll the bus every --interval seconds and re-render")
    ap.add_argument("--interval", type=float, default=5.0,
                    help="seconds between polls in --loop (default 5.0, min 1.0)")
    ap.add_argument("--socket", help="daemon control socket (default: ds's own default)")
    ap.add_argument("--data-dir", help="daemon data dir (passed through to ds)")
    ap.add_argument("--ds-bin", default=os.environ.get("DS_BIN", "ds"),
                    help="path to the ds binary (default: $DS_BIN or 'ds' on PATH)")
    args = ap.parse_args()
    if args.interval < 1.0:
        ap.error("--interval must be >= 1.0 s (the 1 Hz attention rule, docs/microapps.md)")

    prior = {p: get_json(args.ds_bin, args.socket, args.data_dir, p) for p in WRITTEN}

    def restore():
        for p in WRITTEN:
            old = prior[p]
            if old is None:
                continue  # did not exist before: leave the last value (no unset op)
            try:
                set_json(args.ds_bin, args.socket, args.data_dir, p, old)
            except DSError as e:
                print("cleanup: %s" % e, file=sys.stderr)

    stopping = {"flag": False}

    def on_signal(signum, frame):
        stopping["flag"] = True

    signal.signal(signal.SIGINT, on_signal)
    signal.signal(signal.SIGTERM, on_signal)

    try:
        while True:
            s = sample()
            out = render(args.ds_bin, args.socket, args.data_dir, s)
            print(json.dumps(out, separators=(",", ":")), flush=True)
            if not args.loop or stopping["flag"]:
                break
            time.sleep(args.interval)
        return 0
    except DSError as e:
        print("now_playing: %s" % e, file=sys.stderr)
        return 1
    finally:
        if args.loop:
            restore()


if __name__ == "__main__":
    sys.exit(main())
