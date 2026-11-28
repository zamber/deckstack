#!/usr/bin/env python3
"""DS-121 conformance smoke test: the static preview webapp (clients/preview-web/)
and the /preview API endpoints it consumes (ticket
docs/tickets/2026-11-28-preview-webapp.md).

Black-box on ds-vdev + ds-daemon --preview (NO hardware, NO browser automation):
boots its own stack, serves clients/preview-web/ with python3 -m http.server on an
ephemeral loopback port, and asserts

  preview_web_010  the app is servable: index.html 200 and references app.js +
                   style.css; app.js, style.css, config.js all 200
  preview_web_020  GET /preview/api/state -> 200 with {rev, controls, bindings,
                   plugins, devices}; devices is an object (populated per-module
                   once DS-112 lands — on this base it may still be empty, the app
                   falls back to serial order); after a `ds set` of a control the
                   matching /controls entry appears
  preview_web_030  GET /preview/api/action-metadata -> 200, protocol 1, core verbs
                   exactly [set, emit, plugin, exec, sleep] each with args +
                   capability; plugins is an array
  preview_web_040  GET /preview/ws?token=... -> 101 + preview.hello (read_only) +
                   preview.snapshot carrying state.devices and actions.core (the
                   app's live/fallback path when HTTP fetch is CORS-blocked), and a
                   subscribed /controls/* stream receives the event for a `ds set`;
                   upgrade without token -> 401
  preview_web_050  auth/T7 on the endpoints the app uses: no bearer -> 401 on both
                   /preview/api/* routes; non-loopback Host -> 421

Exit 0 = all green. Every wait carries a timeout; the daemon, vdev and the static
server are always stopped and the temp dir removed, so CI can never hang or leak.

Usage: python3 tests/conformance/preview_web/run.py [--verbose] [-k SUBSTR ...]
Registered in tests/conformance/run.py as the DS-121 suite (`-k preview_web`).
"""

import base64
import http.client
import json
import os
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
APP_DIR = ROOT / "clients" / "preview-web"

VERBOSE = "--verbose" in sys.argv
BUILD_TIMEOUT_S = 900
OP_TIMEOUT_S = 20
BOOT_TIMEOUT_S = 30
KILL_TIMEOUT_S = 5

BINARIES = ("ds", "ds-daemon", "ds-vdev", "ds-vdevctl")


def log(msg):
    if VERBOSE:
        print(msg, file=sys.stderr)


def bin_dir():
    env = os.environ.get("DS_BIN_DIR")
    if env:
        return Path(env)
    return ROOT / "target" / "debug"


def ensure_binaries():
    d = bin_dir()
    missing = [b for b in BINARIES if not (d / b).is_file()]
    if not missing:
        return d
    cargo = shutil.which("cargo")
    if cargo is None:
        raise RuntimeError("cargo not found and binaries missing from %s" % d)
    print("building binaries (bounded %ss): %s" % (BUILD_TIMEOUT_S, " ".join(missing)))
    subprocess.run(
        [cargo, "build", "--bin", "ds", "--bin", "ds-daemon", "--bin", "ds-vdev",
         "--bin", "ds-vdevctl"],
        cwd=str(ROOT), timeout=BUILD_TIMEOUT_S, check=True,
        stdout=subprocess.DEVNULL if not VERBOSE else None,
        stderr=subprocess.DEVNULL if not VERBOSE else None,
    )
    return d


class Stack:
    """One ds-vdev + ds-daemon run with --preview on a fresh temp dir."""

    def __init__(self, bins):
        self.bins = bins
        self.tmp = Path(tempfile.mkdtemp(prefix="ds-121-preview-web-"))
        self.vdev_sock = self.tmp / "vdev.sock"
        self.data = self.tmp / "data"
        self.xdg = self.tmp / "xdg"
        self.xdg.mkdir()
        self.vdev = None
        self.daemon = None

    def start(self):
        self.vdev = subprocess.Popen(
            [str(self.bins / "ds-vdev"), "--socket", str(self.vdev_sock)],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL,
        )
        self._wait_for_file(self.vdev_sock, 10, "ds-vdev device socket", self.vdev)
        env = dict(os.environ)
        env["XDG_DATA_HOME"] = str(self.xdg)
        self.daemon = subprocess.Popen(
            [str(self.bins / "ds-daemon"), "run",
             "--device", "virtual:%s" % self.vdev_sock,
             "--data-dir", str(self.data), "--preview"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            stdin=subprocess.DEVNULL, env=env,
        )
        self.sock = self.data / "daemon.sock"
        self._wait_for_file(self.sock, BOOT_TIMEOUT_S, "daemon control socket", self.daemon)
        self.token = (self.data / "token").read_text().strip()
        self.http_port_file = self.data / "http.port"
        self._wait_for_file(self.http_port_file, 10, "http.port", self.daemon)
        self.port = int(self.http_port_file.read_text().strip())
        return self

    def _wait_for_file(self, path, timeout_s, what, proc=None):
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            if path.exists():
                return
            if proc is not None and proc.poll() is not None:
                raise RuntimeError("process exited early rc=%d while waiting for %s"
                                   % (proc.returncode, what))
            time.sleep(0.05)
        raise RuntimeError("timed out after %ss waiting for %s (%s)" % (timeout_s, what, path))

    def ds(self, *args, timeout=OP_TIMEOUT_S):
        return subprocess.run(
            [str(self.bins / "ds"), "--socket", str(self.sock),
             "--data-dir", str(self.data)] + list(args),
            capture_output=True, text=True, timeout=timeout,
        )

    def http(self, method, path, token="__unset__", host="127.0.0.1"):
        """One raw HTTP request to the preview port. Returns (status, head, body)."""
        s = socket.create_connection(("127.0.0.1", self.port), timeout=OP_TIMEOUT_S)
        s.settimeout(OP_TIMEOUT_S)
        auth = ""
        if token != "__unset__":
            auth = "Authorization: Bearer %s\r\n" % (token or "")
        req = ("%s %s HTTP/1.1\r\nHost: %s\r\n%sContent-Length: 0\r\n"
               "Connection: close\r\n\r\n" % (method, path, host, auth))
        s.sendall(req.encode())
        buf = b""
        try:
            while True:
                chunk = s.recv(65536)
                if not chunk:
                    break
                buf += chunk
        except socket.timeout:
            pass
        s.close()
        head, _, rest = buf.partition(b"\r\n\r\n")
        text = head.decode(errors="replace")
        status = int(text.split("\r\n")[0].split()[1]) if text else 0
        return status, text, rest.decode(errors="replace")

    def stop(self):
        problems = []
        for proc in (self.daemon, self.vdev):
            if proc is None or proc.poll() is not None:
                continue
            proc.send_signal(signal.SIGTERM)
            try:
                proc.wait(timeout=KILL_TIMEOUT_S)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=KILL_TIMEOUT_S)
                problems.append("had to SIGKILL pid %d" % proc.pid)
        shutil.rmtree(self.tmp, ignore_errors=True)
        if problems:
            raise RuntimeError("; ".join(problems))


class WsClient:
    """Minimal RFC 6455 client (handshake + masked text frames), stdlib only."""

    def __init__(self, port, token=None, timeout=OP_TIMEOUT_S):
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=timeout)
        self.sock.settimeout(timeout)
        key = base64.b64encode(os.urandom(16)).decode()
        q = ("?token=" + token) if token is not None else ""
        req = (
            "GET /preview/ws%s HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\n"
            "Connection: Upgrade\r\nSec-WebSocket-Key: %s\r\nSec-WebSocket-Version: 13\r\n"
            "Authorization: Bearer %s\r\n\r\n" % (q, key, token or "")
        )
        self.sock.sendall(req.encode())
        buf = b""
        while b"\r\n\r\n" not in buf:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise AssertionError("no WS handshake response")
            buf += chunk
        head, _, rest = buf.partition(b"\r\n\r\n")
        self.status = int(head.split(b"\r\n")[0].split()[1])
        self.head = head.decode(errors="replace")
        self._buf = rest

    def send_text(self, obj):
        payload = json.dumps(obj).encode()
        mask = os.urandom(4)
        header = bytearray([0x81])
        n = len(payload)
        if n < 126:
            header.append(0x80 | n)
        elif n < 65536:
            header.append(0x80 | 126)
            header += struct.pack(">H", n)
        else:
            header.append(0x80 | 127)
            header += struct.pack(">Q", n)
        header += mask
        self.sock.sendall(bytes(header) + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))

    def recv(self, timeout=OP_TIMEOUT_S):
        self.sock.settimeout(timeout)
        b1, b2 = self._read_exact(2)
        opcode = b1 & 0x0F
        ln = b2 & 0x7F
        if ln == 126:
            ln = struct.unpack(">H", self._read_exact(2))[0]
        elif ln == 127:
            ln = struct.unpack(">Q", self._read_exact(8))[0]
        payload = self._read_exact(ln)
        assert opcode == 0x1, "expected a text frame, got opcode %d" % opcode
        return json.loads(payload.decode())

    def _read_exact(self, n):
        while len(self._buf) < n:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise AssertionError("WS connection closed")
            self._buf += chunk
        out, self._buf = self._buf[:n], self._buf[n:]
        return out

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


class StaticServer:
    """python3 -m http.server equivalent for clients/preview-web on an ephemeral port."""

    def __init__(self):
        handler = lambda *a, **kw: SimpleHTTPRequestHandler(*a, directory=str(APP_DIR), **kw)
        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), handler)
        self.port = self.httpd.server_address[1]
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)

    def start(self):
        self.thread.start()
        return self

    def get(self, path):
        c = http.client.HTTPConnection("127.0.0.1", self.port, timeout=OP_TIMEOUT_S)
        c.request("GET", path, headers={"Host": "127.0.0.1"})
        r = c.getresponse()
        body = r.read().decode(errors="replace")
        c.close()
        return r.status, dict(r.getheaders()), body

    def stop(self):
        self.httpd.shutdown()
        self.httpd.server_close()
        self.thread.join(timeout=KILL_TIMEOUT_S)


class PreviewWebConformance(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.bins = ensure_binaries()
        assert APP_DIR.is_dir(), "clients/preview-web must exist in the tree"

    def stack(self):
        st = Stack(self.bins).start()
        self.addCleanup(st.stop)
        return st

    def static(self):
        srv = StaticServer().start()
        self.addCleanup(srv.stop)
        return srv

    # -- 010 the app is servable as static files --------------------------------

    def test_preview_web_010_static_app_files_served(self):
        srv = self.static()
        status, _, body = srv.get("/index.html")
        self.assertEqual(status, 200, body[:200])
        self.assertIn("app.js", body)
        self.assertIn("style.css", body)
        self.assertIn("config.js", body)
        for f in ("/app.js", "/style.css", "/config.js"):
            status, _, body = srv.get(f)
            self.assertEqual(status, 200, "%s -> %s" % (f, status))
            self.assertTrue(len(body) > 100, "%s served empty" % f)
        # app.js must reference the endpoints it consumes
        _, _, appjs = srv.get("/app.js")
        self.assertIn("/preview/api/state", appjs)
        self.assertIn("/preview/api/action-metadata", appjs)
        self.assertIn("/preview/ws", appjs)

    # -- 020 /preview/api/state shape --------------------------------------------

    def test_preview_web_020_state_endpoint_shape(self):
        st = self.stack()
        status, head, body = st.http("GET", "/preview/api/state", token=st.token)
        self.assertEqual(status, 200, head)
        self.assertIn("application/json", head)
        snap = json.loads(body)
        for f in ("rev", "controls", "bindings", "plugins", "devices"):
            self.assertIn(f, snap, "state snapshot missing %s" % f)
        self.assertIsInstance(snap["rev"], int)
        self.assertIsInstance(snap["bindings"], list)
        # /devices is populated per module by the DS-112 branch; on this base the
        # vdev handshake publishes only bus events, so assert the shape only.
        devs = snap["devices"]
        self.assertIsInstance(devs, dict)
        for serial, d in devs.items():
            self.assertIn("kind", d, "/devices/%s missing kind" % serial)
            self.assertIn("connection", d, "/devices/%s missing connection" % serial)
        # a control written through the real CLI shows up in the snapshot
        p = st.ds("set", "/controls/keys/0/label", '"k0"')
        self.assertEqual(p.returncode, 0, p.stderr)
        status, _, body = st.http("GET", "/preview/api/state", token=st.token)
        self.assertEqual(status, 200)
        ctrls = json.loads(body)["controls"]
        self.assertIn("0", (ctrls.get("keys") or {}), "set key 0 must appear under /controls/keys")

    # -- 030 /preview/api/action-metadata shape -----------------------------------

    def test_preview_web_030_action_metadata_shape(self):
        st = self.stack()
        status, _, body = st.http("GET", "/preview/api/action-metadata", token=st.token)
        self.assertEqual(status, 200, body[:400])
        doc = json.loads(body)
        self.assertEqual(doc["protocol"], 1)
        self.assertEqual([c["do"] for c in doc["core"]],
                         ["set", "emit", "plugin", "exec", "sleep"])
        for c in doc["core"]:
            self.assertIn("args", c)
            self.assertIn("capability", c)
            self.assertIn("description", c)
        self.assertIsInstance(doc["plugins"], list)

    # -- 040 WS live/fallback path -------------------------------------------------

    def test_preview_web_040_ws_snapshot_carries_state_and_actions(self):
        st = self.stack()
        ws = WsClient(st.port, token=None)
        self.assertEqual(ws.status, 401, ws.head)
        ws.close()
        ws = WsClient(st.port, token=st.token)
        self.assertEqual(ws.status, 101, ws.head)
        hello = ws.recv()
        self.assertEqual(hello["op"], "preview.hello")
        self.assertEqual(hello["read_only"], True)
        snap = ws.recv()
        self.assertEqual(snap["op"], "preview.snapshot")
        self.assertIsInstance(snap["state"]["devices"], dict)
        self.assertEqual([c["do"] for c in snap["actions"]["core"]],
                         ["set", "emit", "plugin", "exec", "sleep"])
        # live stream: subscribe /controls/* then a real CLI set must arrive as an event
        ws.send_text({"id": 1, "op": "subscribe", "path": "/controls/*"})
        ack = ws.recv()
        self.assertEqual(ack["op"], "preview.result")
        p = st.ds("set", "/controls/keys/0/label", '"hovered"')
        self.assertEqual(p.returncode, 0, p.stderr)
        ev = ws.recv()
        self.assertEqual(ev["op"], "event")
        self.assertEqual(ev["path"], "/controls/keys/0/label")
        self.assertEqual(ev["value"], "hovered")
        ws.close()

    # -- 050 auth + T7 on the endpoints the app uses -------------------------------

    def test_preview_web_050_api_auth_and_host_checks(self):
        st = self.stack()
        for path in ("/preview/api/state", "/preview/api/action-metadata"):
            self.assertEqual(st.http("GET", path, token=None)[0], 401, path)
            self.assertEqual(st.http("GET", path, token="0" * 64)[0], 401, path)
            self.assertEqual(st.http("GET", path, token=st.token, host="evil.example")[0], 421, path)
            self.assertEqual(st.http("GET", path, token=st.token)[0], 200, path)


def preview_web_suite():
    return unittest.defaultTestLoader.loadTestsFromTestCase(PreviewWebConformance)


def _parse_k(argv):
    patterns = []
    i = 1
    while i < len(argv):
        if i + 1 < len(argv) and argv[i] == "-k":
            patterns.append(argv[i + 1])
            i += 2
        else:
            i += 1
    return patterns


def main():
    patterns = _parse_k(sys.argv)
    suite = preview_web_suite()
    if patterns:
        kept = unittest.TestSuite()
        for t in suite:
            if any(p.lower() in t.id().lower() for p in patterns):
                kept.addTest(t)
        suite = kept
    runner = unittest.TextTestRunner(verbosity=2 if VERBOSE else 1)
    result = runner.run(suite)
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    sys.exit(main())
