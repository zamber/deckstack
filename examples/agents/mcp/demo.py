#!/usr/bin/env python3
"""Minimal MCP agent demo (ticket DS-098): initialize, tools/list, set a key
colour, read it back, print the result — the docs/agents.md route in miniature.

Run from the repo root (boots its own ds-vdev + ds-daemon on a temp dir):

    python examples/agents/mcp/demo.py

Or against an already-running daemon:

    python examples/agents/mcp/demo.py --socket $XDG_RUNTIME_DIR/deckstack/daemon.sock \
        --data-dir ~/.local/state/deckstack

MCP client config snippet: mcp-servers.json next to this file (drop it into
.mcp.json for Claude Code or claude_desktop_config.json for Claude Desktop —
both use the same {"mcpServers": …} shape; see docs/agents.md §4).
Normative tool spec: docs/components/mcp-bridge.md.
"""

import argparse
import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "clients" / "ds-py"))
import smoke  # noqa: E402  (vdev+daemon boot pattern, clients/ds-py/smoke.py)


def rpc(proc, rid, method, params=None):
    """Send one JSON-RPC 2.0 line to ds-mcp, return the response with id=rid."""
    msg = {"jsonrpc": "2.0", "id": rid, "method": method}
    if params is not None:
        msg["params"] = params
    proc.stdin.write(json.dumps(msg) + "\n")
    proc.stdin.flush()
    while True:
        line = proc.stdout.readline()
        if not line:
            raise SystemExit("ds-mcp exited early: " + proc.stderr.read())
        got = json.loads(line)
        if got.get("id") == rid:
            return got


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--socket", help="daemon control socket (default: boot own vdev+daemon)")
    ap.add_argument("--data-dir", help="daemon data dir (with --socket)")
    args = ap.parse_args()

    stack = None
    if args.socket:
        bins = smoke.bin_dir()
        sock, data = args.socket, args.data_dir
    else:
        bins = smoke.ensure_binaries()
        stack = smoke.Stack(bins).start()
        sock, data = stack.sock, stack.data

    try:
        mcp = subprocess.Popen(
            [str(bins / "ds-mcp"), "--socket", str(sock), "--data-dir", str(data)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, bufsize=1)
        r = rpc(mcp, 1, "initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                        "clientInfo": {"name": "demo", "version": "0.1.0"}})
        print("initialize ->", json.dumps(r["result"]["serverInfo"]))
        mcp.stdin.write(json.dumps({"jsonrpc": "2.0",
                                    "method": "notifications/initialized"}) + "\n")
        mcp.stdin.flush()
        tools = rpc(mcp, 2, "tools/list")["result"]["tools"]
        print("tools/list ->", ", ".join(t["name"] for t in tools))
        r = rpc(mcp, 3, "tools/call", {"name": "deck_set", "arguments": {
            "path": "/controls/keys/0/image",
            "value": {"kind": "color", "value": "#3cc86e"}}})
        print("deck_set  ->", json.dumps(r["result"]["structuredContent"]))
        r = rpc(mcp, 4, "tools/call", {"name": "deck_get",
                                       "arguments": {"path": "/controls/keys/0/image"}})
        got = r["result"]["structuredContent"]
        print("deck_get  ->", json.dumps(got))
        assert got["value"] == {"kind": "color", "value": "#3cc86e"}, got
        mcp.stdin.close()
        mcp.wait(timeout=5)
        vdev_note = (" (on vdev: ds-vdevctl --socket %s frames key:0)"
                     % stack.vdev_sock) if stack is not None else ""
        print("OK — /controls/keys/0/image is now #3cc86e%s" % vdev_note)
        return 0
    finally:
        if stack is not None:
            stack.stop()


if __name__ == "__main__":
    sys.exit(main())
