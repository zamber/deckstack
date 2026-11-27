#!/usr/bin/env python3
"""DS-113 conformance: the /preview debug surface + action metadata (ticket
docs/tickets/2026-11-25-action-metadata-virtual-deck.md; contract docs/contracts/preview.md).

Black-box on ds-vdev + ds-daemon (NO hardware): boots its own stack per case and drives
it through raw loopback HTTP/WebSocket (stdlib only — the WS client is hand-rolled,
masked frames included) plus the real `ds` / `ds-vdevctl` binaries.

Cases (ids contain "preview", so `python3 tests/conformance/run.py -k preview` runs only
these):

  preview_010  default-off: no --preview flag -> no <data>/http.port, no listener,
               /policy/preview absent; setting it true after boot does not bind
  preview_020  GET /preview with bearer -> 200 text/html; missing/bad token -> 401;
               Host: evil.example -> 421 (T7); Host: localhost -> 200; `/` and
               /api/get -> 404; POST /preview* -> 405
  preview_030  action metadata lists the demo plugin's actions (bump/get_lamp with
               params schemas) after examples/plugins/demo is installed; core
               vocabulary is exactly set/emit/plugin/exec/sleep
  preview_040  WS: upgrade without token -> 401; with token -> preview.hello
               (read_only) + preview.snapshot; subscribe /controls/* then a `ds set`
               streams an event frame; a mutating WS op -> E_SCOPE and rev unchanged
  preview_050  negative: a whole preview session (HTTP GETs + WS subscribe/get/ping)
               leaves rev unchanged and adds zero entries to `ds-vdevctl writes`
  preview_060  /policy/preview=false disables the running surface live (404),
               true re-enables it
  preview_070  `ds schema action-metadata` / `ds schema preview` exit 0 and match
               schemas/ on disk; the daemon `schema` op serves both names

Exit 0 = all green. Every wait carries a timeout; the daemon and vdev are always killed
(SIGTERM then SIGKILL) and the temp dir is removed, so CI can never hang or leak.

Usage: python3 tests/conformance/preview/run.py [--verbose] [-k SUBSTR ...]
"""

import base64
import hashlib
import json
import os
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
DEMO = ROOT / "examples" / "plugins" / "demo"

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


# ---------------------------------------------------------------- harness ------


class Ipc:
    """Raw NDJSON control-socket client (hello/auth/rpc/recv)."""

    def __init__(self, sock_path, token, timeout=OP_TIMEOUT_S):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(timeout)
        self.sock.connect(str(sock_path))
        self.f = self.sock.makefile("rw", encoding="utf-8", newline="\n")
        self.rpc({"id": 0, "op": "hello", "protocol": 1, "client": "ds113/1"})
        r = self.rpc({"id": 1, "op": "auth", "token": token, "scope": "admin"})
        assert r.get("ok"), r

    def rpc(self, obj):
        self.f.write(json.dumps(obj) + "\n")
        self.f.flush()
        line = self.f.readline()
        if not line:
            raise AssertionError("daemon closed the control connection")
        return json.loads(line)

    def recv(self, timeout=OP_TIMEOUT_S):
        self.sock.settimeout(timeout)
        line = self.f.readline()
        if not line:
            raise AssertionError("timeout waiting for an event line")
        return json.loads(line)

    def close(self):
        try:
            self.f.close()
            self.sock.close()
        except OSError:
            pass


class WsClient:
    """Minimal RFC 6455 client (handshake + masked text frames), stdlib only."""

    def __init__(self, port, token=None, host="127.0.0.1", path="/preview/ws",
                 timeout=OP_TIMEOUT_S):
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=timeout)
        self.sock.settimeout(timeout)
        key = base64.b64encode(os.urandom(16)).decode()
        q = ("?token=" + token) if token is not None else ""
        req = (
            "GET %s%s HTTP/1.1\r\nHost: %s\r\nUpgrade: websocket\r\n"
            "Connection: Upgrade\r\nSec-WebSocket-Key: %s\r\nSec-WebSocket-Version: 13\r\n"
            "Authorization: Bearer %s\r\n\r\n" % (path, q, host, key, token or "")
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

    def _read_exact(self, n):
        while len(self._buf) < n:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise AssertionError("WS connection closed")
            self._buf += chunk
        out, self._buf = self._buf[:n], self._buf[n:]
        return out

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

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


class PreviewStack:
    """One ds-vdev + ds-daemon (started directly, optionally with --preview)."""

    def __init__(self, bins, preview_flag=True):
        self.bins = bins
        self.tmp = Path(tempfile.mkdtemp(prefix="ds-113-preview-"))
        self.vdev_sock = self.tmp / "vdev.sock"
        self.data = self.tmp / "data"
        self.xdg = self.tmp / "xdg"
        self.xdg.mkdir()
        self.vdev = None
        self.daemon = None
        self.preview_flag = preview_flag

    def start(self):
        self.vdev = subprocess.Popen(
            [str(self.bins / "ds-vdev"), "--socket", str(self.vdev_sock)],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL,
        )
        self._wait_for_file(self.vdev_sock, 10, "ds-vdev device socket")
        env = dict(os.environ)
        env["XDG_DATA_HOME"] = str(self.xdg)
        args = [str(self.bins / "ds-daemon"), "run",
                "--device", "virtual:%s" % self.vdev_sock,
                "--data-dir", str(self.data)]
        if self.preview_flag:
            args.append("--preview")
        self.daemon = subprocess.Popen(
            args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            stdin=subprocess.DEVNULL, env=env,
        )
        self.sock = self.data / "daemon.sock"
        self._wait_for_file(self.sock, BOOT_TIMEOUT_S, "daemon control socket")
        self.token = (self.data / "token").read_text().strip()
        self.http_port_file = self.data / "http.port"
        if self.preview_flag:
            self._wait_for_file(self.http_port_file, 10, "http.port")
            self.port = int(self.http_port_file.read_text().strip())
        else:
            self.port = None
        return self

    def _wait_for_file(self, path, timeout_s, what):
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            if path.exists():
                return
            if self.daemon is not None and self.daemon.poll() is not None:
                raise RuntimeError("ds-daemon exited early rc=%d while waiting for %s"
                                   % (self.daemon.returncode, what))
            time.sleep(0.05)
        raise RuntimeError("timed out after %ss waiting for %s (%s)" % (timeout_s, what, path))

    # ---- drivers ---------------------------------------------------------------

    def ds(self, *args, timeout=OP_TIMEOUT_S):
        return subprocess.run(
            [str(self.bins / "ds"), "--socket", str(self.sock),
             "--data-dir", str(self.data)] + list(args),
            capture_output=True, text=True, timeout=timeout,
        )

    def vdevctl(self, *args, timeout=OP_TIMEOUT_S):
        return subprocess.run(
            [str(self.bins / "ds-vdevctl"), "--socket", str(self.vdev_sock)] + list(args),
            capture_output=True, text=True, timeout=timeout,
        )

    def rev(self):
        """Current committed rev, via the real CLI."""
        p = self.ds("get", "/meta", "--json")
        assert p.returncode == 0, p.stderr
        return json.loads(p.stdout)["value"]["rev"]

    def write_log_len(self):
        """Number of entries in the vdev write log (device-bound reports)."""
        p = self.vdevctl("writes", "100000")
        assert p.returncode == 0, p.stderr
        return len([l for l in p.stdout.splitlines() if l.strip() and l.strip() != "."])

    def ipc(self):
        return Ipc(self.sock, self.token)

    def http(self, method, path, token="__unset__", host="127.0.0.1", body=b""):
        """One raw HTTP request to the preview port. Returns (status, head, body)."""
        assert self.port is not None
        s = socket.create_connection(("127.0.0.1", self.port), timeout=OP_TIMEOUT_S)
        s.settimeout(OP_TIMEOUT_S)
        auth = ""
        if token != "__unset__":
            auth = "Authorization: Bearer %s\r\n" % (token or "")
        req = ("%s %s HTTP/1.1\r\nHost: %s\r\n%sContent-Length: %d\r\n"
               "Connection: close\r\n\r\n" % (method, path, host, auth, len(body)))
        s.sendall(req.encode() + body)
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

    def install_demo_plugin(self):
        """Install examples/plugins/demo under $XDG_DATA_HOME/deckstack/plugins/demo/."""
        pdir = self.xdg / "deckstack" / "plugins" / "demo"
        pdir.mkdir(parents=True)
        shutil.copy(DEMO / "plugin.py", pdir / "plugin.py")
        p = subprocess.run(
            [sys.executable, str(DEMO / "make_manifest.py"), str(pdir / "manifest.json")],
            capture_output=True, text=True, timeout=OP_TIMEOUT_S, cwd=str(ROOT),
        )
        assert p.returncode == 0, "make_manifest failed: %s%s" % (p.stdout, p.stderr)

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


# ---------------------------------------------------------------- cases --------


class PreviewConformance(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.bins = ensure_binaries()

    def stack(self, preview_flag=True):
        st = PreviewStack(self.bins, preview_flag=preview_flag).start()
        self.addCleanup(st.stop)
        return st

    # -- 010 default-off ----------------------------------------------------------

    def test_preview_010_default_off_no_listener(self):
        st = self.stack(preview_flag=False)
        self.assertFalse(st.http_port_file.exists(),
                         "no http.port without --preview / /policy/preview")
        # /policy/preview is absent (default false)
        p = st.ds("get", "/policy", "--json")
        self.assertEqual(p.returncode, 0, p.stderr)
        policy = json.loads(p.stdout)["value"]
        self.assertNotIn("preview", policy)
        # setting it true AFTER boot does not bind a listener (enable happens at boot)
        p = st.ds("set", "/policy/preview", "true")
        self.assertEqual(p.returncode, 0, p.stderr)
        time.sleep(0.5)
        self.assertFalse(st.http_port_file.exists(),
                         "policy set after boot must not bind the loopback listener")

    # -- 020 HTTP routes, bearer, T7 Host -----------------------------------------

    def test_preview_020_http_html_auth_and_host_checks(self):
        st = self.stack(preview_flag=True)
        tok = st.token
        status, head, body = st.http("GET", "/preview", token=tok)
        self.assertEqual(status, 200, head)
        self.assertIn("text/html", head)
        self.assertIn("deckstack /preview", body)
        self.assertIn("/preview/ws", body)  # the page wires the live WS
        # localhost Host is fine too
        self.assertEqual(st.http("GET", "/preview", token=tok, host="localhost")[0], 200)
        self.assertEqual(st.http("GET", "/preview", token=tok, host="127.0.0.1:%d" % st.port)[0], 200)
        # missing / bad token -> 401
        self.assertEqual(st.http("GET", "/preview", token=None)[0], 401)
        self.assertEqual(st.http("GET", "/preview", token="0" * 64)[0], 401)
        # non-loopback Host -> 421 (T7), even with a valid token
        self.assertEqual(st.http("GET", "/preview", token=tok, host="evil.example")[0], 421)
        self.assertEqual(st.http("GET", "/preview", token=tok, host="127.0.0.1.evil.com")[0], 421)
        # unknown paths -> 404 (incl. / and /api/*)
        self.assertEqual(st.http("GET", "/", token=tok)[0], 404)
        self.assertEqual(st.http("GET", "/api/get", token=tok)[0], 404)
        self.assertEqual(st.http("GET", "/preview/nope", token=tok)[0], 404)
        # writes are not accepted: POST on the surface -> 405
        self.assertEqual(st.http("POST", "/preview", token=tok)[0], 405)
        self.assertEqual(st.http("POST", "/preview/api/state", token=tok)[0], 405)
        self.assertEqual(st.http("PUT", "/preview", token=tok)[0], 405)
        # the JSON routes
        status, _, body = st.http("GET", "/preview/api/state", token=tok)
        self.assertEqual(status, 200)
        snap = json.loads(body)
        for f in ("rev", "controls", "bindings", "plugins", "devices"):
            self.assertIn(f, snap)

    # -- 030 action metadata incl. the demo plugin --------------------------------

    def test_preview_030_action_metadata_lists_demo_plugin(self):
        st = self.stack(preview_flag=True)
        st.install_demo_plugin()
        status, _, body = st.http("GET", "/preview/api/action-metadata", token=st.token)
        self.assertEqual(status, 200, body[:400])
        doc = json.loads(body)
        self.assertEqual(doc["protocol"], 1)
        core = [c["do"] for c in doc["core"]]
        self.assertEqual(core, ["set", "emit", "plugin", "exec", "sleep"])
        for c in doc["core"]:
            self.assertIn("capability", c)
            self.assertIn("args", c)
        plugins = {p["id"]: p for p in doc["plugins"]}
        self.assertIn("demo", plugins, "the installed demo plugin must be listed: %s" % list(plugins))
        actions = {a["id"]: a for a in plugins["demo"]["actions"]}
        self.assertIn("bump", actions)
        self.assertIn("get_lamp", actions)
        self.assertEqual(actions["bump"]["params"]["properties"]["by"]["type"], "integer")
        self.assertIn("state.write", plugins["demo"]["capabilities"])
        # and the aggregate is schema-shaped: $id served by `ds schema action-metadata`
        p = st.ds("schema", "action-metadata", "--json")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(json.loads(p.stdout)["$id"],
                         "https://deckstack.dev/schemas/action-metadata.json/v1")

    # -- 040 WS live state + read-only --------------------------------------------

    def test_preview_040_ws_live_state_and_read_only(self):
        st = self.stack(preview_flag=True)
        # WS upgrade without any token -> 401
        ws = WsClient(st.port, token=None)
        self.assertEqual(ws.status, 401, ws.head)
        ws.close()
        # with the token (query form, as a browser would) -> 101 + hello + snapshot
        ws = WsClient(st.port, token=st.token)
        self.assertEqual(ws.status, 101, ws.head)
        self.assertIn("sec-websocket-accept", ws.head.lower())
        hello = ws.recv()
        self.assertEqual(hello["op"], "preview.hello")
        self.assertEqual(hello["read_only"], True)
        self.assertEqual(hello["protocol"], 1)
        snap = ws.recv()
        self.assertEqual(snap["op"], "preview.snapshot")
        self.assertIn("controls", snap["state"])
        self.assertEqual([c["do"] for c in snap["actions"]["core"]],
                         ["set", "emit", "plugin", "exec", "sleep"])
        # subscribe to control-state changes
        ws.send_text({"id": 1, "op": "subscribe", "path": "/controls/*"})
        ack = ws.recv()
        self.assertEqual(ack["op"], "preview.result")
        self.assertEqual(ack["subscribed"], "/controls/*")
        # a real CLI set must stream an event frame
        rev0 = st.rev()
        p = st.ds("set", "/controls/keys/0/label", '"hovered"')
        self.assertEqual(p.returncode, 0, p.stderr)
        ev = ws.recv()
        self.assertEqual(ev["op"], "event")
        self.assertEqual(ev["path"], "/controls/keys/0/label")
        self.assertEqual(ev["value"], "hovered")
        self.assertEqual(ev["rev"], rev0 + 1)
        # a mutating op on the WS is refused (E_SCOPE) and never executed
        ws.send_text({"id": 2, "op": "set", "ops": [
            {"op": "set", "path": "/controls/keys/1/label", "value": "nope"}]})
        err = ws.recv()
        self.assertEqual(err["op"], "error")
        self.assertEqual(err["error"]["code"], "E_SCOPE")
        g = st.ds("get", "/controls/keys/1/label", "--json")
        self.assertEqual(g.returncode, 2, "the refused WS set must not have created the path: %s" % g.stdout)
        rev1 = st.rev()
        self.assertEqual(rev1, rev0 + 1, "the refused WS op must not bump rev")
        # get + ping work on the WS
        ws.send_text({"id": 3, "op": "get", "path": "/controls/keys/0/label"})
        r = ws.recv()
        self.assertEqual(r["value"], "hovered")
        ws.send_text({"id": 4, "op": "ping"})
        self.assertEqual(ws.recv()["pong"], True)
        ws.close()

    # -- 050 negative: a preview session performs zero writes ----------------------

    def test_preview_050_preview_session_makes_no_writes(self):
        st = self.stack(preview_flag=True)
        time.sleep(1.0)  # let the boot handshake settle so the baseline is stable
        writes_before = st.write_log_len()
        rev0 = st.rev()
        # a full preview session: page + both JSON routes + a WS with subscribe/get/ping
        self.assertEqual(st.http("GET", "/preview", token=st.token)[0], 200)
        self.assertEqual(st.http("GET", "/preview/api/action-metadata", token=st.token)[0], 200)
        self.assertEqual(st.http("GET", "/preview/api/state", token=st.token)[0], 200)
        ws = WsClient(st.port, token=st.token)
        self.assertEqual(ws.status, 101)
        ws.recv(); ws.recv()  # hello + snapshot
        ws.send_text({"id": 1, "op": "subscribe", "path": "/controls/*"})
        self.assertEqual(ws.recv()["op"], "preview.result")
        ws.send_text({"id": 2, "op": "get", "path": "/controls"})
        self.assertEqual(ws.recv()["op"], "preview.result")
        ws.send_text({"id": 3, "op": "ping"})
        self.assertEqual(ws.recv()["pong"], True)
        time.sleep(0.5)
        ws.close()
        writes_after = st.write_log_len()
        rev1 = st.rev()
        self.assertEqual(rev1, rev0, "a preview session must not mutate state (rev)")
        self.assertEqual(writes_after, writes_before,
                         "ds-vdevctl writes must show no device-bound reports during a "
                         "preview session (%d -> %d)" % (writes_before, writes_after))

    # -- 060 /policy disable/enable live -------------------------------------------

    def test_preview_060_policy_disables_live(self):
        st = self.stack(preview_flag=True)
        tok = st.token
        self.assertEqual(st.http("GET", "/preview", token=tok)[0], 200)
        p = st.ds("set", "/policy/preview", "false")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(st.http("GET", "/preview", token=tok)[0], 404,
                         "disabled live via /policy/preview=false -> 404")
        self.assertEqual(st.http("GET", "/preview/api/state", token=tok)[0], 404)
        # re-enable
        p = st.ds("set", "/policy/preview", "true")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(st.http("GET", "/preview", token=tok)[0], 200)

    # -- 070 schemas served by CLI and daemon ---------------------------------------

    def test_preview_070_schemas_served(self):
        st = self.stack(preview_flag=True)
        for name, fname in (("action-metadata", "action-metadata.json"),
                            ("preview", "preview.json")):
            p = st.ds("schema", name, "--json")
            self.assertEqual(p.returncode, 0, "%s: %s" % (name, p.stderr))
            on_disk = json.loads((ROOT / "schemas" / fname).read_text())
            self.assertEqual(json.loads(p.stdout), on_disk)
            # the daemon's `schema` op serves the same document
            c = st.ipc()
            r = c.rpc({"id": 2, "op": "schema", "name": name})
            self.assertTrue(r.get("ok"), r)
            self.assertEqual(r["result"], on_disk)
            c.close()


def preview_suite():
    return unittest.defaultTestLoader.loadTestsFromTestCase(PreviewConformance)


def _parse_k(argv):
    patterns = []
    i = 1
    while i < len(argv):
        if argv[i] == "-k" and i + 1 < len(argv):
            patterns.append(argv[i + 1])
            i += 2
        else:
            i += 1
    return patterns


def main():
    patterns = _parse_k(sys.argv)
    suite = preview_suite()
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
