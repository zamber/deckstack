#!/usr/bin/env python3
"""Micro-app: focus panel — render the focused window's app on the dial + key 0/1.

Teaches the API calls: `ds watch /app/focus/changed --ndjson` (the streaming
verb: topic subscription, one JSON event per line, only --ndjson is accepted),
`ds get /focus --json` (the daemon's mirror of the last focus event, S7),
`ds set <path> <value>` (dial/key label + a text visual), plus `--socket` /
`--data-dir` and exit-code handling.

Who sends app/focus/changed: the DS-060 sender `ds-focus` (docs/components/
focus-sender.md) — `ds-focus --backend auto` on a real session, or
`ds-focus --inject '{"app":"code","title":"x"}'` to fake one in tests. An
agent can also run its own D-Bus (KWin queryWindowInfo) or zwlr
(foreign-toplevel) probe and publish via the daemon `emit` op — note the CLI
verb `ds emit` is refused with exit 1 in phase 1 (reported gap), the MCP tool
`deck_emit` and the raw IPC `{"op":"emit",…}` work.

Stdlib only; talks to a running daemon through the `ds` binary.

    python3 examples/microapps/focus_panel.py --once
    python3 examples/microapps/focus_panel.py --loop
    python3 examples/microapps/focus_panel.py --socket /tmp/deck/daemon.sock --loop

--loop restores the dial/key fields it wrote on exit (Ctrl-C); fields that did
not exist before are left at the last value (no unset op).
"""

import argparse
import json
import os
import signal
import subprocess
import sys

FOCUS_TOPIC_PATH = "/app/focus/changed"  # topic subscription spelling (leading /)
DIAL = "/controls/dial"
KEY0 = "/controls/keys/0"
KEY1 = "/controls/keys/1"
WRITTEN = (DIAL + "/label", DIAL + "/image", KEY0 + "/label", KEY0 + "/image",
           KEY1 + "/label")
MAX_TEXT = 32  # text visual value / label maxLength (schemas/state.json)


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


def render(ds_bin, socket, data_dir, focus):
    """focus = {"app","title","pid"} (or None → placeholder)."""
    if focus and focus.get("app"):
        app, title = clip(focus["app"]), clip(focus.get("title", ""))
        color = "#0066cc"
    else:
        app, title = "no focus", ""
        color = "#444444"
    set_json(ds_bin, socket, data_dir, DIAL + "/label", app)
    set_json(ds_bin, socket, data_dir, DIAL + "/image",
             {"kind": "text", "value": app, "fg": "#ffffff", "bg": color})
    set_json(ds_bin, socket, data_dir, KEY0 + "/label", "focus: " + app)
    set_json(ds_bin, socket, data_dir, KEY0 + "/image", {"kind": "color", "value": color})
    set_json(ds_bin, socket, data_dir, KEY1 + "/label", title)
    return {"app": app, "title": title}


def current_focus(ds_bin, socket, data_dir):
    """--once path: read the daemon's /focus mirror (E_PATH → None)."""
    return get_json(ds_bin, socket, data_dir, "/focus")


def watch_loop(ds_bin, socket, data_dir, on_event, stopping):
    """--loop path: `ds watch /app/focus/changed --ndjson`, one event per line."""
    cmd = [ds_bin]
    if socket:
        cmd += ["--socket", socket]
    if data_dir:
        cmd += ["--data-dir", data_dir]
    cmd += ["watch", FOCUS_TOPIC_PATH, "--ndjson"]
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            text=True)
    try:
        for line in proc.stdout:
            if stopping["flag"]:
                break
            line = line.strip()
            if not line:
                continue
            try:
                ev = json.loads(line)
            except ValueError:
                continue  # not an event line; ignore
            payload = ev.get("payload") or {}
            if ev.get("topic") == "app/focus/changed" or "app" in payload:
                on_event(payload)
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


def main():
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    mode = ap.add_mutually_exclusive_group()
    mode.add_argument("--once", action="store_true", default=True,
                      help="render the current /focus once and exit (default)")
    mode.add_argument("--loop", action="store_true",
                      help="watch app/focus/changed --ndjson and re-render per event")
    ap.add_argument("--socket", help="daemon control socket (default: ds's own default)")
    ap.add_argument("--data-dir", help="daemon data dir (passed through to ds)")
    ap.add_argument("--ds-bin", default=os.environ.get("DS_BIN", "ds"),
                    help="path to the ds binary (default: $DS_BIN or 'ds' on PATH)")
    args = ap.parse_args()

    prior = {p: get_json(args.ds_bin, args.socket, args.data_dir, p) for p in WRITTEN}

    def restore():
        for p in WRITTEN:
            old = prior[p]  # leaf value read via `ds get <path> --json` at start
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
        if not args.loop:
            out = render(args.ds_bin, args.socket, args.data_dir,
                         current_focus(args.ds_bin, args.socket, args.data_dir))
            print(json.dumps(out, separators=(",", ":")), flush=True)
            return 0

        def on_event(payload):
            out = render(args.ds_bin, args.socket, args.data_dir, payload)
            print(json.dumps(out, separators=(",", ":")), flush=True)

        watch_loop(args.ds_bin, args.socket, args.data_dir, on_event, stopping)
        return 0
    except DSError as e:
        print("focus_panel: %s" % e, file=sys.stderr)
        return 1
    finally:
        if args.loop:
            restore()


if __name__ == "__main__":
    sys.exit(main())
