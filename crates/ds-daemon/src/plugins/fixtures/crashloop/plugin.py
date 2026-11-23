#!/usr/bin/env python3
"""Registers correctly, then exits after a short delay (watchdog crash-loop fixture)."""
import json, os, sys, time
HERE = os.path.dirname(os.path.abspath(__file__))
with open(os.path.join(HERE, "manifest.json")) as f:
    MANIFEST = json.load(f)
def send(o):
    sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
for raw in sys.stdin:
    line = raw.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except Exception:
        continue
    if msg.get("op") == "hello":
        send({"op": "register", "id": MANIFEST["id"], "manifest": MANIFEST})
        time.sleep(0.3)
        os._exit(1)
