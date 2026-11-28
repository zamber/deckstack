#!/usr/bin/env python3
"""Print the preview-webapp link for the currently running deckstack daemon.

Reads the preview port and token from the daemon's data dir and prints the URL
of the static preview webapp (clients/preview-web, served by serve.py on :8080).

Usage:
    preview_link.py                 # real deck daemon (default data dir)
    preview_link.py --vdev          # the DS-121 vdev demo (/tmp/ds121-demo, port 39955)
    preview_link.py --dir /path     # another data dir
    preview_link.py --port 8080     # webapp server port (default 8080)

Env: DECKSTACK_DATA_DIR overrides the default data dir
(/run/user/1000/deckstack, or %XDG_RUNTIME_DIR%/deckstack).
"""
from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path

WEBAPP_PORT = 8080
VDEV = {"dir": "/tmp/ds121-demo", "port": 39955}


def default_data_dir() -> Path:
    env = os.environ.get("DECKSTACK_DATA_DIR")
    if env:
        return Path(env)
    rt = os.environ.get("XDG_RUNTIME_DIR", "/run/user/1000")
    return Path(rt) / "deckstack"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--vdev", action="store_true", help="use the vdev demo daemon")
    ap.add_argument("--dir", help="daemon data dir (default: $DECKSTACK_DATA_DIR or $XDG_RUNTIME_DIR/deckstack)")
    ap.add_argument("--port", type=int, default=WEBAPP_PORT, help="preview webapp server port")
    ap.add_argument("--webapp", default="http://127.0.0.1:{}/", help="webapp base URL template with {} for the port")
    args = ap.parse_args()

    if args.vdev:
        ddir, dport = Path(VDEV["dir"]), VDEV["port"]
    else:
        ddir = Path(args.dir) if args.dir else default_data_dir()
        port_file = ddir / "http.port"
        if not port_file.exists():
            print(f"no {port_file} — is the daemon running with --preview?", file=sys.stderr)
            return 1
        dport = int(port_file.read_text().strip())

    token_file = ddir / "token"
    token = token_file.read_text().strip() if token_file.exists() else ""
    if not token and not args.vdev:
        # vdev demo token is well-known; real daemon without token file is an error
        print(f"no {token_file}", file=sys.stderr)
        return 1
    if args.vdev and not token:
        token = "dace73774761fe86d712982550059c118bdf01ba109978ba94fde760695a7fa6"

    base = args.webapp.format(args.port)
    print(f"{base}?token={token}&port={dport}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
