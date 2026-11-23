#!/usr/bin/env python3
"""Example agent: status poller (ticket DS-063).

Polls `ds daemon status` and the key state through the deckstack helper, printing
one compact JSON line per tick — the shape an agent loop wants. Telemetry paths
(`/telemetry/*`) return no rev (S1), which the helper passes through untouched.

Run from the repo root (boots its own ds-vdev + ds-daemon on a temp dir):

    python examples/agents/status_poller.py            # 3 ticks
    python examples/agents/status_poller.py --ticks 10 --interval 0.5

Or against a running daemon:

    python examples/agents/status_poller.py --socket $XDG_RUNTIME_DIR/deckstack/daemon.sock \
        --data-dir ~/.local/state/deckstack

Shell equivalent (R1 — the helper is optional sugar):

    ds daemon status --json --compact; ds get /controls/keys/0 --json --compact
"""

import argparse
import json
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "clients" / "ds-py"))

from deckstack import DeckClient  # noqa: E402
import smoke  # noqa: E402  (vdev+daemon boot pattern, only used without --socket)


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--socket", help="daemon control socket (default: boot own vdev+daemon)")
    ap.add_argument("--data-dir", help="daemon data dir (with --socket)")
    ap.add_argument("--ds-bin", default="ds", help="path to the ds binary")
    ap.add_argument("--ticks", type=int, default=3, help="poll count")
    ap.add_argument("--interval", type=float, default=1.0, help="seconds between ticks")
    args = ap.parse_args()

    stack = None
    if args.socket:
        client = DeckClient(ds=args.ds_bin, socket=args.socket, data_dir=args.data_dir,
                            cwd=str(ROOT))
    else:
        bins = smoke.ensure_binaries()
        stack = smoke.Stack(bins).start()
        client = stack.client()

    try:
        # one seed write so /controls/keys/0 exists to read
        client.set("/controls/keys/0/image", {"kind": "color", "value": "#3cc86e"})
        for tick in range(args.ticks):
            st = client.status()
            key = client.get("/controls/keys/0/image")
            print(json.dumps({
                "tick": tick,
                "running": st.get("running"),
                "device": st.get("device"),
                "key0": key.get("value"),
                "rev": key.get("rev"),
            }, separators=(",", ":")))
            if tick + 1 < args.ticks:
                time.sleep(args.interval)
        return 0
    finally:
        if stack is not None:
            stack.stop()


if __name__ == "__main__":
    sys.exit(main())
