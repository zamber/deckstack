#!/usr/bin/env python3
"""DS-050 in-crate test plugin (fixture). Speaks the newline-JSON wire protocol of
docs/contracts/plugins.md over stdin/stdout; stderr goes to the daemon log/ring."""
import json
import os
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
with open(os.path.join(HERE, "manifest.json")) as f:
    MANIFEST = json.load(f)

PID = os.getpid()


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def log(msg):
    send({"op": "log", "message": "%s %s" % (PID, msg)})


if not os.environ.get("DS_PLUGIN_TOKEN"):
    log("FATAL: no DS_PLUGIN_TOKEN in env")
    sys.exit(3)

log("started token-present")
state = {"count": 0}
pending = {}  # request_id -> invoke request_id (replies are forwarded as effects)

for raw in sys.stdin:
    line = raw.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except Exception:
        continue
    op = msg.get("op")
    if op == "hello":
        assert msg.get("protocol") == 1, msg
        send({"op": "register", "id": MANIFEST["id"], "manifest": MANIFEST})
    elif op == "event":
        log("event:" + str(msg.get("event")))
    elif op == "reply":
        rrid = msg.get("request_id")
        if rrid in pending:
            invoke_rid = pending.pop(rrid)
            send({
                "op": "effects",
                "request_id": invoke_rid,
                "effects": [
                    {"effect": "log", "message": "reply:" + json.dumps(msg)},
                    {"effect": "set", "path": "/plugins/%s/state/last_reply" % MANIFEST["id"], "value": msg},
                ],
            })
    elif op == "invoke":
        rid = msg.get("request_id")
        full = msg.get("id", "")
        action = full.split(".", 1)[1] if "." in full else full
        args = msg.get("args") or {}
        if action == "bump":
            state["count"] += int(args.get("by", 1))
            send({
                "op": "effects",
                "request_id": rid,
                "effects": [
                    {"effect": "set", "path": "/plugins/%s/state/count" % MANIFEST["id"], "value": state["count"]},
                    {"effect": "set", "path": "/controls/keys/0/label", "value": str(state["count"])},
                ],
            })
        elif action == "violate":
            send({
                "op": "effects",
                "request_id": rid,
                "effects": [
                    {"effect": "set", "path": "/policy/exec/allow", "value": ["/bin/sh"]},
                    {"effect": "emit", "topic": "app/evil/x", "payload": {"pwn": True}},
                ],
            })
        elif action == "sleep":
            time.sleep(30)
            send({"op": "effects", "request_id": rid, "effects": []})
        elif action == "crash":
            os._exit(1)
        elif action == "net":
            rrid = 9000 + (rid % 1000)
            pending[rrid] = rid
            send({"op": "request", "request_id": rrid, "kind": "net", "method": "GET", "url": args.get("url")})
        elif action == "getstate":
            rrid = 8000 + (rid % 1000)
            pending[rrid] = rid
            send({"op": "request", "request_id": rrid, "kind": "get", "path": args.get("path", "/controls")})
        elif action == "probe":
            secret = args.get("secret", "/etc/hostname")
            port = int(args.get("port", 9))
            res = {}
            try:
                with open(secret) as f:
                    f.read()
                res["secret"] = "read-ok"
            except Exception as e:
                res["secret"] = type(e).__name__
            try:
                import socket
                s = socket.create_connection(("127.0.0.1", port), timeout=1)
                s.close()
                res["tcp"] = "ok"
            except Exception as e:
                res["tcp"] = type(e).__name__
            send({
                "op": "effects",
                "request_id": rid,
                "effects": [
                    {"effect": "set", "path": "/plugins/%s/state/probe" % MANIFEST["id"], "value": res},
                    {"effect": "log", "message": "probe:" + json.dumps(res)},
                ],
            })
        elif action == "echo":
            send({
                "op": "effects",
                "request_id": rid,
                "effects": [{"effect": "log", "message": "echo:" + json.dumps(args)}],
            })
        else:
            send({"op": "effects", "request_id": rid, "effects": []})
