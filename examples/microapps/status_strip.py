#!/usr/bin/env python3
"""Micro-app: host status strip on keys 0-3 (CPU load / memory / time / uptime).

Teaches the API calls: `ds get <path> --json` (read back current key state),
`ds set <path> <value>` (write label + image colour), the global `--socket` /
`--data-dir` flags, and the exit-code contract (a nonzero `ds` exit is reported
with its `[exit N]` stderr and this program exits 1).

Stdlib only. No daemon of its own: it talks to a running ds-daemon via the `ds`
binary (argv-only subprocess, never a shell). At most 1 update per second
(--interval >= 1.0, the 1 Hz attention rule — see docs/microapps.md §rate).

    python3 examples/microapps/status_strip.py --once
    python3 examples/microapps/status_strip.py --loop --interval 2
    python3 examples/microapps/status_strip.py --socket /tmp/deck/daemon.sock \
        --data-dir /tmp/deck --loop

--loop restores the keys it wrote to their previous values on exit (Ctrl-C);
keys that did not exist before are left at the last value (there is no unset
op — documented in docs/microapps.md §errors).
"""

import argparse
import json
import os
import signal
import subprocess
import sys
import time

KEYS = (0, 1, 2, 3)  # 0=load, 1=mem, 2=time, 3=uptime


class DSError(RuntimeError):
    """A `ds` call failed; carries the exit code and the stderr text."""

    def __init__(self, argv, proc):
        self.exit_code = proc.returncode
        super().__init__(
            "ds %s failed [exit %d]: %s"
            % (" ".join(argv), proc.returncode, proc.stderr.strip() or proc.stdout.strip())
        )


def ds(ds_bin, socket, data_dir, args, timeout=15):
    """Run one `ds` verb (argv-only). Returns the CompletedProcess."""
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
    """`ds get <path> --json --compact`; None when the path does not exist yet."""
    p = ds(ds_bin, socket, data_dir, ["get", path, "--json", "--compact"])
    if p.returncode == 2:
        return None
    if p.returncode != 0:
        raise DSError(["get", path], p)
    return json.loads(p.stdout)["value"]


def set_json(ds_bin, socket, data_dir, path, value):
    ds_ok(ds_bin, socket, data_dir, ["set", path, "--json", json.dumps(value)])


def sample():
    """One status sample from /proc (stdlib file reads only)."""
    load1 = open("/proc/loadavg").read().split()[0]
    mem = {}
    for line in open("/proc/meminfo"):
        if line.startswith("MemTotal:"):
            mem["total"] = int(line.split()[1])
        elif line.startswith("MemAvailable:"):
            mem["avail"] = int(line.split()[1])
    mem_pct = round(100.0 * (mem["total"] - mem["avail"]) / mem["total"])
    clock = time.strftime("%H:%M")
    up_s = int(float(open("/proc/uptime").read().split()[0]))
    days, rem = divmod(up_s, 86400)
    hours, rem = divmod(rem, 3600)
    mins = rem // 60
    uptime = ("%dd %dh" % (days, hours)) if days else ("%dh %dm" % (hours, mins) if hours else "%dm" % mins)
    return {"load": load1, "mem_pct": mem_pct, "clock": clock, "uptime": uptime}


def load_color(load1):
    cores = os.cpu_count() or 1
    load = float(load1)
    if load < cores * 0.7:
        return "#00cc44"
    if load < cores * 1.5:
        return "#ffaa00"
    return "#ff2222"


def render(ds_bin, socket, data_dir, s):
    """Write the sample to keys 0-3: label = value, image = state colour."""
    fields = {
        0: ("load " + s["load"], load_color(s["load"])),
        1: ("mem %d%%" % s["mem_pct"], "#ff8800" if s["mem_pct"] >= 90 else "#00ccff"),
        2: (s["clock"], "#3cc86e"),
        3: ("up " + s["uptime"], "#888888"),
    }
    for k in KEYS:
        label, color = fields[k]
        set_json(ds_bin, socket, data_dir, "/controls/keys/%d/label" % k, label)
        set_json(ds_bin, socket, data_dir, "/controls/keys/%d/image" % k,
                 {"kind": "color", "value": color})


def main():
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    mode = ap.add_mutually_exclusive_group()
    mode.add_argument("--once", action="store_true", default=True,
                      help="update once and exit (default)")
    mode.add_argument("--loop", action="store_true",
                      help="update forever at --interval (>= 1.0 s, the 1 Hz rule)")
    ap.add_argument("--interval", type=float, default=1.0,
                    help="seconds between updates in --loop (default 1.0, min 1.0)")
    ap.add_argument("--socket", help="daemon control socket (default: ds's own default)")
    ap.add_argument("--data-dir", help="daemon data dir (passed through to ds)")
    ap.add_argument("--ds-bin", default=os.environ.get("DS_BIN", "ds"),
                    help="path to the ds binary (default: $DS_BIN or 'ds' on PATH)")
    args = ap.parse_args()
    if args.interval < 1.0:
        ap.error("--interval must be >= 1.0 s (the 1 Hz attention rule, docs/microapps.md)")

    # read current key state once — used by --loop cleanup to restore on exit
    prior = {k: (get_json(args.ds_bin, args.socket, args.data_dir, "/controls/keys/%d" % k))
             for k in KEYS}

    def restore():
        for k in KEYS:
            old = prior[k]
            if old is None:
                continue  # did not exist before: leave the last value (no unset op)
            for leaf, value in old.items():
                try:
                    set_json(args.ds_bin, args.socket, args.data_dir,
                             "/controls/keys/%d/%s" % (k, leaf), value)
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
            render(args.ds_bin, args.socket, args.data_dir, s)
            print(json.dumps(s, separators=(",", ":")), flush=True)
            if not args.loop or stopping["flag"]:
                break
            time.sleep(args.interval)
        return 0
    except DSError as e:
        print("status_strip: %s" % e, file=sys.stderr)
        return 1
    finally:
        if args.loop:
            restore()


if __name__ == "__main__":
    sys.exit(main())
