#!/usr/bin/env python3
"""Example agent: focus-reactive recolour (ticket DS-063).

A tiny "agent" built only on the deckstack helper: it binds a deck event topic to
a recolour step, then simulates a key press on the virtual device (via
`ds-vdevctl inject`, the vdev way to fake hardware input) and watches the
resulting state change.

Run from the repo root (boots its own ds-vdev + ds-daemon on a temp dir, then
cleans up):

    python examples/agents/focus_recolor.py

Or against an already-running stack:

    python examples/agents/focus_recolor.py --socket $XDG_RUNTIME_DIR/deckstack/daemon.sock \
        --data-dir ~/.local/state/deckstack --ds-bin target/release/ds

Everything here is also reachable with the bare `ds` CLI (R1) — the helper is
optional sugar; the same binding in shell is:

    ds bind deck/control/keys/0/down --do 'set /controls/keys/5/image {"kind":"color","value":"#ff8800"}'
"""

import argparse
import json
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "clients" / "ds-py"))

from deckstack import DeckClient  # noqa: E402
import smoke  # noqa: E402  (reuses the vdev+daemon boot pattern from clients/ds-py)


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--socket", help="daemon control socket (default: boot own vdev+daemon)")
    ap.add_argument("--data-dir", help="daemon data dir (with --socket)")
    ap.add_argument("--ds-bin", default="ds", help="path to the ds binary")
    args = ap.parse_args()

    stack = None
    if args.socket:
        client = DeckClient(ds=args.ds_bin, socket=args.socket, data_dir=args.data_dir,
                            cwd=str(ROOT))
        vdev_sock = None
    else:
        bins = smoke.ensure_binaries()
        stack = smoke.Stack(bins).start()
        client = stack.client()
        vdev_sock = stack.vdev_sock

    try:
        # 1. the agent declares what it reacts to: key 0 pressed -> key 5 turns orange
        bid = client.bind(
            "deck/control/keys/0/down",
            'set /controls/keys/5/image {"kind":"color","value":"#ff8800"}',
        )
        print("bound %s on deck/control/keys/0/down" % bid)

        # 2. watch the recolour land (streaming verb: --ndjson only, never --json)
        got = {}
        watcher = client.watch("/controls/keys/5/image", max_events=1)

        def reader():
            got["events"] = list(watcher)

        t = threading.Thread(target=reader, daemon=True)
        t.start()
        time.sleep(1.0)  # let the subscription reach the daemon

        # 3. simulate the key press on the virtual device
        if vdev_sock is None:
            print("no vdev socket known; press key 0 on the real deck to trigger")
            t.join(30)
        else:
            inj = tempfile.NamedTemporaryFile("w", suffix=".ndjson", delete=False)
            inj.write(json.dumps({"module": "keys", "keys": [0]}) + "\n")
            inj.close()
            r = subprocess.run([str(smoke.bin_dir() / "ds-vdevctl"), "--socket", str(vdev_sock),
                                "inject", inj.name], capture_output=True, text=True, timeout=20)
            print("injected key press: %s" % r.stdout.strip())
            t.join(20)

        events = got.get("events", [])
        if not events:
            print("FAIL: no recolor event observed")
            return 1
        ev = events[0]
        print("recolour event: path=%s rev=%s value=%s"
              % (ev.get("path"), ev.get("rev"), json.dumps(ev.get("value"))))
        assert ev["value"] == {"kind": "color", "value": "#ff8800"}, ev

        # 4. read it back through the helper
        g = client.get("/controls/keys/5/image")
        assert g["value"] == {"kind": "color", "value": "#ff8800"}, g
        print("get round-trip ok: %s" % json.dumps(g["value"]))

        client.unbind(binding_id=bid)
        print("unbound %s — demo done" % bid)
        return 0
    finally:
        if stack is not None:
            stack.stop()


if __name__ == "__main__":
    sys.exit(main())
