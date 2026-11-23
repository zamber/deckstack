#!/usr/bin/env python3
"""deckstack agent-onboarding conformance (ticket DS-098,
tests/conformance/agent-onboarding/run.py).

The scripted proof behind docs/agents.md ("how an agent gets a working deck
session"): black-box, stdlib only. It boots ds-vdev + ds-daemon on a fresh temp
dir, spawns the real `ds-mcp` binary on stdio, and drives it ONLY through
JSON-RPC 2.0 lines — the exact route a coding agent follows:

    010 initialize handshake (protocolVersion, serverInfo, capabilities.tools)
    020 tools/list returns exactly the ten deck_* tools, each with an inputSchema
    030 deck_status  -> running daemon status object
    040 deck_set     -> a solid colour on /controls/keys/0/image (rev bumps)
    050 deck_get     -> reads the same colour back (round-trip)
    060 the same three calls through clients/ds-py (DeckClient status/set/get)
    070 every stdout line of ds-mcp parsed as JSON (no logging on stdout)
    900 cleanup: daemon stopped, socket + temp dir gone, no leaked processes

Usage:
    python3 tests/conformance/agent-onboarding/run.py [--verbose]
    python3 tests/conformance/run.py            # runs this suite too (registered there)
    python3 tests/conformance/run.py -k onboarding   # only these cases

Exit code 0 = all green (prints RESULT: OK), nonzero = at least one failure.
Every wait carries a timeout; the daemon, vdev and ds-mcp are always killed
(SIGTERM then SIGKILL) and the temp dir removed, so CI can never hang or leak
processes.
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

ROOT = Path(__file__).resolve().parents[3]
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

KEY_PATH = "/controls/keys/0/image"
KEY_COLOR = {"kind": "color", "value": "#3cc86e"}


def log(msg):
    if VERBOSE:
        print(msg, file=sys.stderr)


def bin_dir():
    env = os.environ.get("DS_BIN_DIR")
    if env:
        return Path(env)
    for candidate in (ROOT / "target" / "debug", ROOT / "target" / "release"):
        if all((candidate / b).is_file() for b in BINARIES):
            return candidate
    return ROOT / "target" / "debug"


def ensure_binaries():
    """Build the binaries (incl. ds-mcp) if any is missing (bounded)."""
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
        [cargo, "build"] + [a for b in BINARIES for a in ("--bin", b)],
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
    tests/conformance/run.py and tests/conformance/mcp.py)."""

    def __init__(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="ds-onboarding-conf-"))
        self.vdev_sock = self.tmp / "vdev.sock"
        self.data = self.tmp / "data"
        self.sock = self.data / "daemon.sock"
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
        pid_file = self.data / "daemon.pid"
        if pid_file.is_file():
            try:
                self.daemon_pid = int(pid_file.read_text().strip())
            except ValueError:
                self.daemon_pid = None

    def ds_env(self):
        env = dict(os.environ)
        env["DS_DAEMON_BIN"] = str(self.bins / "ds-daemon")
        return env

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
        """Mandatory cleanup; returns a list of problems (empty = clean). Idempotent."""
        problems = []
        if self.bins is None:
            return problems
        try:
            subprocess.run(
                [str(self.bins / "ds"), "--socket", str(self.sock),
                 "--data-dir", str(self.data), "daemon", "stop"],
                capture_output=True, text=True, timeout=OP_TIMEOUT_S, env=self.ds_env(),
            )
        except Exception as e:
            log("daemon stop op failed (continuing with signals): %r" % e)
        for name, proc, pid in (
            ("daemon", None, self.daemon_pid),
            ("vdev", self.vdev, self.vdev.pid if self.vdev else None),
        ):
            try:
                self._kill(name, proc, pid)
            except RuntimeError as e:
                problems.append(str(e))
        if self.sock.exists():
            problems.append("control socket still present: %s" % self.sock)
        shutil.rmtree(self.tmp, ignore_errors=True)
        if self.tmp.exists():
            problems.append("temp dir still exists: %s" % self.tmp)
        try:
            self.vdev_log.close()
        except Exception:
            pass
        self.bins = None
        return problems

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

    def __init__(self, bins, socket_path, data_dir):
        self.proc = subprocess.Popen(
            [str(bins / "ds-mcp"), "--socket", str(socket_path),
             "--data-dir", str(data_dir)],
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

    def send(self, obj):
        assert self.proc.stdin is not None
        self.proc.stdin.write(json.dumps(obj) + "\n")
        self.proc.stdin.flush()

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


class AgentOnboarding(unittest.TestCase):
    """Drives one booted stack + one ds-mcp over stdio, then clients/ds-py
    against the same daemon — the onboarding route of docs/agents.md."""

    stack = None
    mcp = None
    bins = None
    client = None
    _shutdown_done = False

    @classmethod
    def setUpClass(cls):
        cls.bins = ensure_binaries()
        sys.path.insert(0, str(ROOT / "clients" / "ds-py"))
        from deckstack import DeckClient  # noqa: PLC0415 - stdlib-only helper
        cls.DeckClient = DeckClient
        cls.stack = Stack()
        cls.stack.start(cls.bins)
        cls.mcp = McpClient(cls.bins, cls.stack.sock, cls.stack.data)
        cls.client = DeckClient(ds=str(cls.bins / "ds"), socket=cls.stack.sock,
                                data_dir=cls.stack.data, cwd=str(ROOT),
                                env=cls.stack.ds_env(), timeout=OP_TIMEOUT_S)

    @classmethod
    def tearDownClass(cls):
        cls.shutdown()
        if cls.stack is not None and VERBOSE:
            cls.stack.dump_logs()

    @classmethod
    def shutdown(cls):
        """Idempotent cleanup; returns the list of problems (empty = clean)."""
        if cls._shutdown_done:
            return []
        cls._shutdown_done = True
        problems = []
        if cls.mcp is not None:
            cls.mcp.close()
            cls.mcp = None
        if cls.stack is not None:
            problems.extend(cls.stack.stop())
            cls.stack = None
        return problems

    def assertCleanStdout(self):
        self.assertEqual(self.mcp.bad_lines, [],
                         "ds-mcp wrote non-JSON to stdout: %r" % self.mcp.bad_lines)

    # ------------------------------------------------------------------ tests

    def test_010_initialize_handshake(self):
        self.mcp.send({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                       "clientInfo": {"name": "agent-onboarding", "version": "0"}},
        })
        r = self.mcp.recv_response(1)
        self.assertNotIn("error", r, r)
        result = r["result"]
        self.assertEqual(result["protocolVersion"], "2024-11-05")
        self.assertEqual(result["serverInfo"]["name"], "ds-mcp")
        self.assertIn("version", result["serverInfo"])
        self.assertIn("tools", result["capabilities"])
        # the initialized notification gets no response; ping then works
        self.mcp.notify("notifications/initialized", {})
        self.mcp.send({"jsonrpc": "2.0", "id": 2, "method": "ping"})
        r = self.mcp.recv_response(2)
        self.assertEqual(r["result"], {})
        self.assertCleanStdout()

    def test_020_tools_list_has_the_ten_tools(self):
        self.mcp.send({"jsonrpc": "2.0", "id": 3, "method": "tools/list"})
        r = self.mcp.recv_response(3)
        self.assertNotIn("error", r, r)
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
        self.assertTrue(str(sc.get("device", "")).startswith("virtual:"), sc)
        self.assertTrue(str(sc["socket"]).endswith("daemon.sock"))
        self.assertCleanStdout()

    def test_040_deck_set_color_on_key0(self):
        r = self.mcp.call(5, "deck_set", {"path": KEY_PATH, "value": KEY_COLOR})
        self.assertNotIn("error", r, r)
        result = r["result"]
        self.assertEqual(result["isError"], False, result)
        sc = result["structuredContent"]
        self.assertEqual(sc["ok"], True)
        self.assertIsInstance(sc["rev"], int)
        self.assertGreaterEqual(sc["rev"], 1)
        self.assertCleanStdout()

    def test_050_deck_get_roundtrip(self):
        r = self.mcp.call(6, "deck_get", {"path": KEY_PATH})
        self.assertNotIn("error", r, r)
        sc = r["result"]["structuredContent"]
        self.assertEqual(sc["path"], KEY_PATH)
        self.assertEqual(sc["value"], KEY_COLOR)
        self.assertIsInstance(sc["rev"], int)
        self.assertCleanStdout()

    def test_060_same_three_calls_through_ds_py(self):
        # the ds-py route (clients/ds-py) against the same daemon
        st = self.client.status()
        self.assertEqual(st.get("running"), True, st)
        self.assertTrue(str(st.get("device", "")).startswith("virtual:"), st)

        rev = self.client.set(KEY_PATH, KEY_COLOR)
        self.assertIsInstance(rev, int)
        self.assertGreaterEqual(rev, 1, rev)

        g = self.client.get(KEY_PATH)
        self.assertEqual(g["path"], KEY_PATH)
        self.assertEqual(g["value"], KEY_COLOR)
        self.assertEqual(g["rev"], rev, (g, rev))

    def test_070_mcp_stdout_is_pure_json(self):
        # every line ds-mcp wrote so far parsed as JSON (no logging on stdout)
        self.assertCleanStdout()

    def test_900_cleanup(self):
        problems = self.shutdown()
        self.assertEqual(problems, [], "cleanup problems: %s" % "; ".join(problems))
        print("cleanup ok: ds-mcp exited, daemon+vdev dead, socket + temp dir removed")


def agent_onboarding_suite():
    """The suite object registered by tests/conformance/run.py."""
    suite = unittest.TestSuite()
    suite.addTests(unittest.defaultTestLoader.loadTestsFromTestCase(AgentOnboarding))
    return suite


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
    raise TimeoutError("agent-onboarding suite exceeded the %ss watchdog" % SUITE_TIMEOUT_S)


def main():
    signal.signal(signal.SIGALRM, _alarm)
    signal.alarm(SUITE_TIMEOUT_S)
    try:
        suite = agent_onboarding_suite()
        runner = unittest.TextTestRunner(stream=sys.stdout, verbosity=0,
                                         resultclass=LineResult)
        result = runner.run(suite)
    finally:
        signal.alarm(0)
    n = result.testsRun
    bad = len(result.failures) + len(result.errors)
    print("-" * 60)
    print("agent-onboarding: %d tests, %d passed, %d failed/errored, %d skipped"
          % (n, n - bad - len(result.skipped), bad, len(result.skipped)))
    if bad:
        print("RESULT: FAIL")
        return 1
    print("RESULT: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
