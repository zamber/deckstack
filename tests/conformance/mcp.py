#!/usr/bin/env python3
"""deckstack MCP-bridge conformance (ticket DS-061, tests/conformance/mcp.py).

Black-box, stdlib only: boots ds-vdev + ds-daemon on a fresh temp dir, spawns the
real `ds-mcp` binary on stdio, and drives it ONLY through JSON-RPC 2.0 lines
(initialize, tools/list, tools/call, notifications), asserting the JSON shapes of
docs/components/mcp-bridge.md:

    010 initialize handshake (protocolVersion, serverInfo, capabilities.tools)
    020 tools/list returns exactly the ten deck_* tools, each with an inputSchema
    030 deck_status → running daemon status object
    040 deck_set a color on /controls/keys/0/image, then deck_get reads it back
    050 deck_schema state → the state JSON Schema object
    060 unknown tool → JSON-RPC -32602; bad arguments → -32602; malformed line → -32700
    070 deck_watch streams an injected device event (ds-vdevctl inject) as a
        notifications/deck/event MCP notification; notifications/cancelled closes it
        with notifications/deck/watch_closed
    080 a second ds-mcp against a missing socket answers E_UNREACHABLE with the
        'ds daemon start' hint (actionable, isError tool error, exit 6 preserved)

Every stdout line of ds-mcp must parse as JSON (no logging on stdout). Every wait
carries a timeout; the daemon, vdev and ds-mcp are always killed (SIGTERM then
SIGKILL) and the temp dir removed, so CI can never hang or leak processes.

Usage:
    python3 tests/conformance/mcp.py [--verbose]

Exit code 0 = all green (prints RESULT: OK), nonzero = at least one failure.
"""

import json
import os
import queue
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
VERBOSE = "--verbose" in sys.argv

BUILD_TIMEOUT_S = 900
OP_TIMEOUT_S = 20
BOOT_TIMEOUT_S = 30
MSG_TIMEOUT_S = 15
KILL_TIMEOUT_S = 5
SUITE_TIMEOUT_S = 300

BINARIES = ("ds", "ds-daemon", "ds-vdev", "ds-vdevctl", "ds-mcp")

EXPECTED_TOOLS = [
    "deck_status",
    "deck_get",
    "deck_set",
    "deck_bind",
    "deck_unbind",
    "deck_emit",
    "deck_watch",
    "deck_stream_push",
    "deck_plugin_list",
    "deck_schema",
]


def log(msg):
    if VERBOSE:
        print(msg, file=sys.stderr)


def bin_dir():
    env = os.environ.get("DS_BIN_DIR")
    if env:
        return Path(env)
    return ROOT / "target" / "debug"


def ensure_binaries():
    """Build the phase-1 binaries plus ds-mcp if any is missing (bounded)."""
    d = bin_dir()
    missing = [b for b in BINARIES if not (d / b).is_file()]
    if not missing:
        return d
    cargo = shutil.which("cargo")
    if cargo is None:
        raise RuntimeError(
            "cargo not found and binaries missing from %s: %s" % (d, ", ".join(missing))
        )
    print("building binaries (bounded %ss): %s" % (BUILD_TIMEOUT_S, " ".join(missing)))
    subprocess.run(
        [cargo, "build", "--bin", "ds", "--bin", "ds-daemon", "--bin", "ds-vdev",
         "--bin", "ds-vdevctl", "--bin", "ds-mcp"],
        cwd=str(ROOT), timeout=BUILD_TIMEOUT_S, check=True,
        stdout=subprocess.DEVNULL if not VERBOSE else None,
        stderr=subprocess.DEVNULL if not VERBOSE else None,
    )
    missing = [b for b in BINARIES if not (d / b).is_file()]
    if missing:
        raise RuntimeError("cargo build finished but still missing: %s" % ", ".join(missing))
    return d


class Stack:
    """One booted ds-vdev + ds-daemon pair on a fresh temp dir (mirrors
    tests/conformance/run.py)."""

    def __init__(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="ds-mcp-conf-"))
        self.vdev_sock = self.tmp / "vdev.sock"
        self.data = self.tmp / "data"
        self.sock = self.data / "daemon.sock"
        self.token = ""
        self.vdev = None
        self.daemon_pid = None
        self.bins = None
        self.vdev_log = open(self.tmp / "vdev.log", "w+")
        self.daemon_log = self.data / "daemon.log"

    def start(self, bins):
        self.bins = bins
        self.vdev = subprocess.Popen(
            [str(bins / "ds-vdev"), "--socket", str(self.vdev_sock)],
            stdout=self.vdev_log, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL,
        )
        self._wait_for_file(self.vdev_sock, 10, "ds-vdev device socket")
        env = dict(os.environ)
        env["DS_DAEMON_BIN"] = str(bins / "ds-daemon")
        p = subprocess.run(
            [str(bins / "ds"), "daemon", "start",
             "--device", "virtual:%s" % self.vdev_sock, "--data-dir", str(self.data)],
            capture_output=True, text=True, timeout=BOOT_TIMEOUT_S, env=env,
        )
        if p.returncode != 0:
            raise RuntimeError(
                "ds daemon start failed rc=%d\nstdout: %s\nstderr: %s\ndaemon.log: %s"
                % (p.returncode, p.stdout, p.stderr, self._read(self.daemon_log))
            )
        self._wait_for_file(self.sock, 10, "daemon control socket")
        self.token = (self.data / "token").read_text().strip()
        if len(self.token) < 16:
            raise RuntimeError("token file at %s is too short" % (self.data / "token"))
        pid_file = self.data / "daemon.pid"
        if pid_file.is_file():
            try:
                self.daemon_pid = int(pid_file.read_text().strip())
            except ValueError:
                self.daemon_pid = None

    def inject(self, events, timeout=OP_TIMEOUT_S):
        """ds-vdevctl inject <ndjson>: replay scripted input events to the daemon."""
        f = self.tmp / ("inject-%d.ndjson" % time.monotonic_ns())
        f.write_text("".join(json.dumps(e) + "\n" for e in events))
        return subprocess.run(
            [str(self.bins / "ds-vdevctl"), "--socket", str(self.vdev_sock),
             "inject", str(f)],
            capture_output=True, text=True, timeout=timeout,
        )

    @staticmethod
    def _read(p):
        try:
            return Path(p).read_text()[-4000:]
        except OSError:
            return "<unreadable>"

    def _wait_for_file(self, path, timeout_s, what):
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            if path.exists():
                return
            time.sleep(0.05)
        raise RuntimeError("timed out after %ss waiting for %s (%s)" % (timeout_s, what, path))

    def stop(self):
        """Mandatory cleanup: stop the daemon, SIGTERM then SIGKILL both processes,
        remove the temp dir."""
        try:
            subprocess.run(
                [str(self.bins / "ds"), "--socket", str(self.sock),
                 "--data-dir", str(self.data), "daemon", "stop"],
                capture_output=True, text=True, timeout=OP_TIMEOUT_S,
                env={**os.environ, "DS_DAEMON_BIN": str(self.bins / "ds-daemon")},
            )
        except Exception as e:
            log("daemon stop op failed (continuing with signals): %r" % e)
        for name, proc, pid in (
            ("daemon", None, self.daemon_pid),
            ("vdev", self.vdev, self.vdev.pid if self.vdev else None),
        ):
            self._kill(name, proc, pid)
        shutil.rmtree(self.tmp, ignore_errors=True)
        try:
            self.vdev_log.close()
        except Exception:
            pass

    def _kill(self, name, proc, pid):
        targets = []
        if proc is not None and proc.poll() is None:
            targets.append(("popen", proc))
        if pid is not None:
            try:
                os.kill(pid, 0)
                targets.append(("pid", pid))
            except OSError:
                pass
        for kind, t in targets:
            try:
                if kind == "popen":
                    t.terminate()
                else:
                    os.kill(t, signal.SIGTERM)
            except OSError:
                pass
        deadline = time.monotonic() + KILL_TIMEOUT_S
        while time.monotonic() < deadline:
            if not any(self._alive(t) for _, t in targets):
                return
            time.sleep(0.05)
        for kind, t in targets:
            try:
                if kind == "popen":
                    t.kill()
                else:
                    os.kill(t, signal.SIGKILL)
            except OSError:
                pass
        deadline = time.monotonic() + KILL_TIMEOUT_S
        while time.monotonic() < deadline:
            if not any(self._alive(t) for _, t in targets):
                return
            time.sleep(0.05)
        raise RuntimeError("%s process did not die after SIGKILL" % name)

    @staticmethod
    def _alive(t):
        if isinstance(t, subprocess.Popen):
            return t.poll() is None
        try:
            os.kill(t, 0)
            return True
        except OSError:
            return False

    def dump_logs(self):
        for label, p in (("vdev", self.tmp / "vdev.log"), ("daemon", self.daemon_log)):
            print("---- %s log (%s) ----" % (label, p))
            print(self._read(p))


class McpClient:
    """One spawned ds-mcp process spoken to over stdio (one JSON-RPC message per
    line). A reader thread guarantees every stdout line is parsed as JSON — any
    non-JSON output (logging) fails the test."""

    def __init__(self, bins, socket_path, data_dir, extra_args=()):
        self.proc = subprocess.Popen(
            [str(bins / "ds-mcp"), "--socket", str(socket_path),
             "--data-dir", str(data_dir), *extra_args],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, bufsize=1,
        )
        self.messages = queue.Queue()
        self.bad_lines = []
        self._reader = threading.Thread(target=self._read_loop, daemon=True)
        self._reader.start()

    def _read_loop(self):
        assert self.proc.stdout is not None
        for line in self.proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                self.messages.put(json.loads(line))
            except json.JSONDecodeError:
                self.bad_lines.append(line)
                self.messages.put({"__bad_line__": line})
        self.messages.put({"__eof__": True})

    def send_raw(self, line):
        assert self.proc.stdin is not None
        self.proc.stdin.write(line + "\n")
        self.proc.stdin.flush()

    def send(self, obj):
        self.send_raw(json.dumps(obj))

    def recv(self, timeout=MSG_TIMEOUT_S):
        try:
            return self.messages.get(timeout=timeout)
        except queue.Empty:
            raise AssertionError("ds-mcp did not answer within %ss" % timeout)

    def recv_response(self, rid, timeout=MSG_TIMEOUT_S):
        """Next message with the given response id (notifications may interleave)."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            msg = self.recv(max(0.1, deadline - time.monotonic()))
            if msg.get("id") == rid and ("result" in msg or "error" in msg):
                return msg
            log("interleaved message while waiting for id=%s: %s" % (rid, msg))
        raise AssertionError("no response for id=%s" % rid)

    def call(self, rid, name, arguments, timeout=MSG_TIMEOUT_S):
        self.send({
            "jsonrpc": "2.0", "id": rid, "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        })
        return self.recv_response(rid, timeout)

    def notify(self, method, params):
        self.send({"jsonrpc": "2.0", "method": method, "params": params})

    def wait_notification(self, method, timeout=MSG_TIMEOUT_S):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            msg = self.recv(max(0.1, deadline - time.monotonic()))
            if msg.get("method") == method:
                return msg
            log("interleaved message while waiting for %s: %s" % (method, msg))
        raise AssertionError("no notification %s within %ss" % (method, timeout))

    def close(self, timeout=KILL_TIMEOUT_S):
        try:
            if self.proc.stdin and not self.proc.stdin.closed:
                self.proc.stdin.close()
        except Exception:
            pass
        try:
            self.proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait(timeout=timeout)


class McpConformance(unittest.TestCase):
    """Drives one booted stack + one ds-mcp over stdio."""

    stack = None
    mcp = None
    bins = None

    @classmethod
    def setUpClass(cls):
        cls.bins = ensure_binaries()
        cls.stack = Stack()
        cls.stack.start(cls.bins)
        cls.mcp = McpClient(cls.bins, cls.stack.sock, cls.stack.data)

    @classmethod
    def tearDownClass(cls):
        if cls.mcp is not None:
            cls.mcp.close()
            cls.mcp = None
        if cls.stack is not None:
            cls.stack.stop()
            cls.stack = None

    def assertCleanStdout(self):
        self.assertEqual(self.mcp.bad_lines, [],
                         "ds-mcp wrote non-JSON to stdout: %r" % self.mcp.bad_lines)

    # ------------------------------------------------------------------ tests

    def test_010_initialize_handshake(self):
        self.mcp.send({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                       "clientInfo": {"name": "mcp-conformance", "version": "0"}},
        })
        r = self.mcp.recv_response(1)
        self.assertNotIn("error", r, r)
        result = r["result"]
        self.assertEqual(result["protocolVersion"], "2024-11-05")
        self.assertEqual(result["serverInfo"]["name"], "ds-mcp")
        self.assertIn("version", result["serverInfo"])
        self.assertIn("tools", result["capabilities"])
        # the initialized notification gets no response
        self.mcp.notify("notifications/initialized", {})
        # ping works after the handshake
        self.mcp.send({"jsonrpc": "2.0", "id": 2, "method": "ping"})
        r = self.mcp.recv_response(2)
        self.assertEqual(r["result"], {})
        self.assertCleanStdout()

    def test_020_tools_list_has_the_ten_tools(self):
        self.mcp.send({"jsonrpc": "2.0", "id": 3, "method": "tools/list"})
        r = self.mcp.recv_response(3)
        tools = r["result"]["tools"]
        names = [t["name"] for t in tools]
        self.assertEqual(names, EXPECTED_TOOLS)
        for t in tools:
            self.assertEqual(t["inputSchema"]["type"], "object", t)
            self.assertIn("description", t)
        self.assertCleanStdout()

    def test_030_deck_status(self):
        r = self.mcp.call(4, "deck_status", {})
        self.assertNotIn("error", r, r)
        result = r["result"]
        self.assertEqual(result["isError"], False)
        sc = result["structuredContent"]
        self.assertEqual(sc["running"], True)
        self.assertIn("rev", sc)
        self.assertTrue(str(sc["socket"]).endswith("daemon.sock"))
        self.assertTrue(str(sc["client"]).startswith("ds-mcp/"), sc["client"])
        self.assertCleanStdout()

    def test_040_deck_set_then_deck_get_roundtrip(self):
        color = {"kind": "color", "value": "#3cc86e"}
        r = self.mcp.call(5, "deck_set",
                          {"path": "/controls/keys/0/image", "value": color})
        self.assertNotIn("error", r, r)
        result = r["result"]
        self.assertEqual(result["isError"], False, result)
        self.assertEqual(result["structuredContent"]["ok"], True)
        self.assertGreaterEqual(result["structuredContent"]["rev"], 1)

        r = self.mcp.call(6, "deck_get", {"path": "/controls/keys/0/image"})
        self.assertNotIn("error", r, r)
        sc = r["result"]["structuredContent"]
        self.assertEqual(sc["path"], "/controls/keys/0/image")
        self.assertEqual(sc["value"], color)
        self.assertIsInstance(sc["rev"], int)
        self.assertCleanStdout()

    def test_050_deck_schema_state(self):
        r = self.mcp.call(7, "deck_schema", {"name": "state"})
        self.assertNotIn("error", r, r)
        sc = r["result"]["structuredContent"]
        self.assertIn("state.json", sc["$id"])
        self.assertEqual(sc["type"], "object")
        self.assertCleanStdout()

    def test_060_unknown_tool_bad_params_and_malformed_line(self):
        # unknown tool -> JSON-RPC -32602 (an error response, not a crash)
        r = self.mcp.call(8, "deck_nope", {})
        self.assertEqual(r["error"]["code"], -32602, r)
        self.assertIn("unknown tool", r["error"]["message"])
        # missing required argument -> -32602
        r = self.mcp.call(9, "deck_get", {})
        self.assertEqual(r["error"]["code"], -32602, r)
        self.assertIn("path", r["error"]["message"])
        # malformed line -> -32700 with id null
        self.mcp.send_raw("this is not json")
        r = self.mcp.recv()
        self.assertEqual(r.get("error", {}).get("code"), -32700, r)
        self.assertIsNone(r.get("id"))
        # unknown method -> -32601
        self.mcp.send({"jsonrpc": "2.0", "id": 10, "method": "resources/list"})
        r = self.mcp.recv_response(10)
        self.assertEqual(r["error"]["code"], -32601, r)
        self.assertCleanStdout()

    def test_070_deck_watch_streams_injected_event_and_cancels(self):
        # subscribe to device input events
        r = self.mcp.call(11, "deck_watch", {"path": "/deck/control/keys/*"})
        self.assertNotIn("error", r, r)
        result = r["result"]
        self.assertEqual(result["isError"], False, result)
        sub = result["structuredContent"]["subscription"]
        self.assertTrue(sub.startswith("w"), sub)
        self.assertEqual(result["structuredContent"]["events"],
                         "notifications/deck/event")

        # inject a key-0 press on the virtual device -> daemon routes it as
        # deck/control/keys.0/down -> must arrive as an MCP notification
        p = self.stack.inject([{"module": "keys", "keys": [0]}])
        self.assertEqual(p.returncode, 0, p.stderr)
        n = self.mcp.wait_notification("notifications/deck/event", timeout=MSG_TIMEOUT_S)
        self.assertEqual(n["params"]["subscription"], sub)
        ev = n["params"]["event"]
        self.assertEqual(ev["op"], "event")
        self.assertEqual(ev["path"], "/deck/control/keys.0/down")
        self.assertEqual(ev["payload"]["pressed"], True)

        # cancel via notifications/cancelled with the tools/call request id (11)
        self.mcp.notify("notifications/cancelled", {"requestId": 11, "reason": "test done"})
        c = self.mcp.wait_notification("notifications/deck/watch_closed",
                                       timeout=MSG_TIMEOUT_S)
        self.assertEqual(c["params"]["subscription"], sub)
        self.assertIn("cancel", c["params"]["reason"])
        self.assertCleanStdout()

    def test_080_daemon_down_is_an_actionable_tool_error(self):
        missing = self.stack.tmp / "missing" / "daemon.sock"
        down = McpClient(self.bins, missing, self.stack.tmp / "missing")
        try:
            down.send({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                           "clientInfo": {"name": "mcp-conformance", "version": "0"}},
            })
            r = down.recv_response(1)
            self.assertIn("result", r, r)
            r = down.call(2, "deck_status", {})
            self.assertNotIn("error", r, r)  # a tool error is a result, not a JSON-RPC error
            result = r["result"]
            self.assertEqual(result["isError"], True)
            text = result["content"][0]["text"]
            self.assertIn("E_UNREACHABLE", text)
            self.assertIn("ds daemon start", text)
            self.assertIn("[exit 6]", text)
            self.assertEqual(down.bad_lines, [])
        finally:
            down.close()
        self.assertCleanStdout()


class LineResult(unittest.TextTestResult):
    def addSuccess(self, test):
        super().addSuccess(test)
        print("PASS  %s" % test.id())

    def addFailure(self, test, err):
        super().addFailure(test, err)
        print("FAIL  %s" % test.id())

    def addError(self, test, err):
        super().addError(test, err)
        print("ERROR %s" % test.id())

    def addSkip(self, test, reason):
        super().addSkip(test, reason)
        print("SKIP  %s (%s)" % (test.id(), reason))


def _alarm(signum, frame):
    raise TimeoutError("mcp conformance suite exceeded the %ss watchdog" % SUITE_TIMEOUT_S)


def main():
    signal.signal(signal.SIGALRM, _alarm)
    signal.alarm(SUITE_TIMEOUT_S)
    try:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(McpConformance)
        runner = unittest.TextTestRunner(stream=sys.stdout, verbosity=0,
                                         resultclass=LineResult)
        result = runner.run(suite)
    finally:
        signal.alarm(0)
    n = result.testsRun
    bad = len(result.failures) + len(result.errors)
    print("-" * 60)
    print("mcp conformance: %d tests, %d passed, %d failed/errored, %d skipped"
          % (n, n - bad - len(result.skipped), bad, len(result.skipped)))
    if bad:
        print("RESULT: FAIL")
        return 1
    print("RESULT: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
