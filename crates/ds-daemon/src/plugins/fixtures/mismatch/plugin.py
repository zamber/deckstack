#!/usr/bin/env python3
"""Registers a manifest that does NOT match the on-disk one -> E_SCHEMA + kill."""
import json, os, sys
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
        tampered = dict(MANIFEST)
        tampered["name"] = "Tampered name"
        send({"op": "register", "id": MANIFEST["id"], "manifest": tampered})
