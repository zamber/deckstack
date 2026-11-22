#!/usr/bin/env python3
"""deckstack bindings conformance suite (ticket DS-041, tests/conformance/bindings/).

Black-box tests over the REAL binaries only: boots ds-vdev + ds-daemon on a fresh
temp dir and drives the system through the `ds` binary, the raw NDJSON control socket
and `ds-vdevctl inject` — no ds-client/ds-state code involved. It proves the DS-041
contract on the wire (schemas/binding.json, docs/contracts/actions.md §2,
docs/contracts/state.md §2, docs/contracts/cli.md §1):

    010 help at every level for bind/unbind exits 0
    020 ds bind appends and prints the daemon-assigned id (bNNNN), rev +1 per bind
    030 ds get /bindings lists all, ds get /bindings/<id> reads one (stored shape verbatim)
    040 ds watch /bindings --ndjson streams the committed change (one event, with rev)
    050 unbind by id removes it (rev +1); unknown id -> exit 2 E_PATH
    060 unbind --topic removes all bindings with that exact topic
    070 negative cases: code-ish step, bad topic, malformed `when` -> exit 1 E_SCHEMA;
        malformed unbind id -> exit 1
    080 a `when` condition is stored verbatim; evaluation is DS-042, so a non-matching
        (and any) `when` fires nothing: the topic event is published, no state changes
    090 raw IPC bind/unbind round trip (placeholder id in the request, result.id assigned)
    100 binding ids are never reused after an unbind
    110 DS-042: bind -> trigger (key inject) -> set/set/emit steps execute in order
        (watch shows first/second in order, one rev bump per set, emit visible on /app/*)
    120 DS-042: `when` on a payload field gates the binding (eq 3 fires, eq 9 does not)
    130 DS-042: a schema-invalid set value is rejected (E_SCHEMA binding-error event)
        without touching state (E_PATH on read, no rev bump)
    140 DS-042: a self-emitting binding hits the emit recursion cap (depth 10) with a
        clear binding-error event; the daemon stays healthy afterwards
    150 DS-042: a plugin step fails closed (E_CAP stub until DS-050) with a
        binding-error event and the remaining steps still run

Usage:
    python3 tests/conformance/bindings/run.py [--verbose]

Exit code 0 = all green. Every wait carries a timeout; the daemon and vdev are always
killed (SIGTERM then SIGKILL) and the temp dir removed, so the run can never hang or
leak processes. check.sh runs tests/conformance/run.py only, so this suite is run
directly (like tests/conformance/events/run.py); the main suite must stay green too.
"""

import json
import os
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
VERBOSE = "--verbose" in sys.argv

BUILD_TIMEOUT_S = 900
OP_TIMEOUT_S = 20
BOOT_TIMEOUT_S = 30
EVENT_TIMEOUT_S = 10
QUIET_S = 1.0
KILL_TIMEOUT_S = 5
SUITE_TIMEOUT_S = 300

BINARIES = ("ds", "ds-daemon", "ds-vdev", "ds-vdevctl")
ID_RE = re.compile(r"^b[0-9]{4,}$")


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
    missing = [b for b in BINARIES if not (d / b).is_file()]
    if missing:
        raise RuntimeError("cargo build finished but still missing: %s" % ", ".join(missing))
    return d


class Stack:
    """One booted ds-vdev + ds-daemon pair on a fresh temp dir."""

    def __init__(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="ds-bindings-"))
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
        pid_file = self.data / "daemon.pid"
        if pid_file.is_file():
            try:
                self.daemon_pid = int(pid_file.read_text().strip())
            except ValueError:
                self.daemon_pid = None

    @staticmethod
    def _read(p):
        try:
            return Path(p).read_text()[-6000:]
        except OSError:
            return "<unreadable>"

    def _wait_for_file(self, path, timeout_s, what):
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            if path.exists():
                return
            time.sleep(0.05)
        raise RuntimeError("timed out after %ss waiting for %s (%s)" % (timeout_s, what, path))

    def ds(self, *args, timeout=OP_TIMEOUT_S):
        env = dict(os.environ)
        env["DS_DAEMON_BIN"] = str(self.bins / "ds-daemon")
        return subprocess.run(
            [str(self.bins / "ds"), "--socket", str(self.sock), "--data-dir", str(self.data)]
            + list(args),
            capture_output=True, text=True, timeout=timeout, env=env,
        )

    def vdevctl(self, *args, timeout=OP_TIMEOUT_S):
        return subprocess.run(
            [str(self.bins / "ds-vdevctl"), "--socket", str(self.vdev_sock)] + list(args),
            capture_output=True, text=True, timeout=timeout,
        )

    def inject(self, events, timeout=OP_TIMEOUT_S):
        f = self.tmp / ("inject-%d.ndjson" % time.monotonic_ns())
        f.write_text("".join(json.dumps(e) + "\n" for e in events))
        return self.vdevctl("inject", str(f), timeout=timeout)

    def ipc(self, timeout=OP_TIMEOUT_S):
        return Ipc(self.sock, timeout)

    def stop(self):
        problems = []
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
            try:
                self._kill(name, proc, pid)
            except RuntimeError as e:
                problems.append(str(e))
        if self.sock.exists():
            problems.append("control socket still present: %s" % self.sock)
        try:
            shutil.rmtree(self.tmp, ignore_errors=True)
        except Exception as e:
            problems.append("temp dir removal failed: %r" % e)
        if self.tmp.exists():
            problems.append("temp dir still exists: %s" % self.tmp)
        try:
            self.vdev_log.close()
        except Exception:
            pass
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


class Ipc:
    """Raw NDJSON control-socket client (black box). Select-based reads so a timeout
    never poisons the connection."""

    def __init__(self, sock_path, timeout=OP_TIMEOUT_S):
        import select
        self._select = select
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(timeout)
        self.sock.connect(str(sock_path))
        self.timeout = timeout
        self.buf = bytearray()

    def send(self, obj):
        self.sock.sendall((json.dumps(obj) + "\n").encode("utf-8"))

    def recv(self, timeout=None):
        timeout = self.timeout if timeout is None else timeout
        deadline = time.monotonic() + timeout
        while True:
            nl = self.buf.find(b"\n")
            if nl >= 0:
                line = bytes(self.buf[:nl])
                del self.buf[: nl + 1]
                if not line.strip():
                    continue
                return json.loads(line)
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise socket.timeout("recv timed out after %ss" % timeout)
            r, _, _ = self._select.select([self.sock], [], [], remaining)
            if not r:
                raise socket.timeout("recv timed out after %ss" % timeout)
            chunk = self.sock.recv(65536)
            if not chunk:
                raise AssertionError("connection closed by daemon while waiting for a reply")
            self.buf.extend(chunk)

    def rpc(self, obj, timeout=None):
        self.send(obj)
        return self.recv(timeout)

    def hello_auth(self, token):
        r = self.rpc({"id": 0, "op": "hello", "protocol": 1, "client": "bindings/1"})
        assert r["ok"], r
        r = self.rpc({"id": 1, "op": "auth", "token": token, "scope": "admin"})
        assert r["ok"], r
        return self

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


class Bindings(unittest.TestCase):
    """The DS-041 bindings conformance cases."""

    stack = None
    boot_error = None

    @classmethod
    def setUpClass(cls):
        try:
            bins = ensure_binaries()
            cls.stack = Stack()
            cls.stack.start(bins)
        except Exception as e:
            cls.boot_error = "%s: %s" % (type(e).__name__, e)
            if cls.stack is not None and VERBOSE:
                cls.stack.dump_logs()

    @classmethod
    def tearDownClass(cls):
        if cls.stack is not None:
            problems = cls.stack.stop()
            if VERBOSE:
                cls.stack.dump_logs()
            if problems:
                print("CLEANUP PROBLEMS: %s" % "; ".join(problems), file=sys.stderr)
            else:
                print("cleanup ok: daemon+vdev dead, temp dir removed, no leaked sockets")

    def setUp(self):
        if self.boot_error:
            self.fail("bindings stack did not boot: %s" % self.boot_error)
        self.stack = Bindings.stack
        self.token = self.stack.token

    # ---- helpers -----------------------------------------------------------

    def jout(self, p):
        self.assertEqual(p.returncode, 0, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        return json.loads(p.stdout)

    def get_json(self, path):
        return self.jout(self.stack.ds("get", path, "--json"))

    def rev(self):
        return self.get_json("/meta")["rev"]

    def bindings(self):
        return self.get_json("/bindings")["value"]

    def bind(self, topic, *dos, when=None, extra=()):
        args = ["bind", topic]
        for d in dos:
            args += ["--do", d]
        if when is not None:
            args += ["--when", when]
        args += list(extra)
        return self.stack.ds(*args, "--json")

    def conn(self):
        return Ipc(self.stack.sock).hello_auth(self.token)

    def next_event(self, c, want_topic=None, want_path=None, timeout=EVENT_TIMEOUT_S):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            line = c.recv(timeout=max(0.1, deadline - time.monotonic()))
            if line.get("op") == "event":
                if want_topic is not None and line.get("topic") != want_topic:
                    continue
                if want_path is not None and line.get("path") != want_path:
                    continue
                return line
        raise AssertionError("no matching event within %ss (want topic=%r path=%r)"
                             % (timeout, want_topic, want_path))

    # ---- cases -------------------------------------------------------------

    def test_010_help_at_every_level_for_bind_unbind(self):
        for args in (["--help"], ["bind", "--help"], ["unbind", "--help"]):
            p = self.stack.ds(*args)
            self.assertEqual(p.returncode, 0, "%s rc=%d stderr=%s" % (args, p.returncode, p.stderr))
            self.assertIn("Usage:", p.stdout)
        top = self.stack.ds("--help")
        self.assertIn("bind", top.stdout)
        self.assertIn("unbind", top.stdout)

    def test_020_bind_appends_and_prints_the_daemon_assigned_id(self):
        r0 = self.rev()
        # first bind on the fresh stack gets b0001
        p = self.bind("deck/control/keys.0/down", 'plugin demo.bump {"by":1}')
        out = self.jout(p)
        self.assertEqual(out["ok"], True)
        self.assertEqual(out["result"]["id"], "b0001")
        self.assertEqual(out["rev"], r0 + 1, "one bind = exactly one rev bump")
        self.assertTrue(ID_RE.match(out["result"]["id"]))
        # second bind: shorthand set + emit steps, human output
        p = self.stack.ds(
            "bind", "deck/control/keys.1/down",
            "--do", 'set /controls/keys/1/image {"kind":"color","value":"#ff0000"}',
            "--do", "emit app/demo/red {}",
        )
        self.assertEqual(p.returncode, 0, p.stderr)
        m = re.match(r"^ok rev=(\d+) id=(b\d{4,})$", p.stdout.strip())
        self.assertIsNotNone(m, "human bind output must be 'ok rev=N id=bNNNN', got %r" % p.stdout)
        self.assertEqual(m.group(2), "b0002")
        self.assertEqual(int(m.group(1)), r0 + 2)
        self.assertEqual(self.rev(), r0 + 2)

    def test_030_get_bindings_lists_all_and_get_by_id_reads_one(self):
        arr = self.bindings()
        self.assertIsInstance(arr, list)
        ids = [b["id"] for b in arr]
        self.assertEqual(ids, ["b0001", "b0002"])
        b1 = [b for b in arr if b["id"] == "b0001"][0]
        self.assertEqual(b1["topic"], "deck/control/keys.0/down")
        # the steps are stored verbatim, in order
        self.assertEqual(b1["steps"], [{"do": "plugin", "id": "demo", "action": "bump", "args": {"by": 1}}])
        b2 = [b for b in arr if b["id"] == "b0002"][0]
        self.assertEqual(b2["steps"][0]["do"], "set")
        self.assertEqual(b2["steps"][0]["path"], "/controls/keys/1/image")
        self.assertEqual(b2["steps"][1], {"do": "emit", "topic": "app/demo/red", "payload": {}})
        # one binding by id (S2: addressed by id, topic stays a field)
        one = self.get_json("/bindings/b0001")
        self.assertEqual(one["path"], "/bindings/b0001")
        self.assertEqual(one["value"]["id"], "b0001")
        self.assertEqual(one["value"]["topic"], "deck/control/keys.0/down")
        self.assertIn("rev", one)
        # unknown id -> exit 2 E_PATH
        p = self.stack.ds("get", "/bindings/b0099", "--json")
        self.assertEqual(p.returncode, 2, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        env = json.loads(p.stderr)
        self.assertEqual(env["error"]["code"], "E_PATH")

    def test_040_watch_bindings_ndjson_streams_the_committed_change(self):
        watch = subprocess.Popen(
            [str(self.stack.bins / "ds"), "--socket", str(self.stack.sock),
             "--data-dir", str(self.stack.data),
             "watch", "/bindings", "--ndjson", "--max-events", "1"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            env={**os.environ, "DS_DAEMON_BIN": str(self.stack.bins / "ds-daemon")},
        )
        try:
            time.sleep(1.0)
            self.assertIsNone(watch.poll(), "watch exited before any bind")
            p = self.bind("deck/control/dial/rotate", "sleep 5")
            self.assertEqual(p.returncode, 0, p.stderr)
            bid = self.jout(p)["result"]["id"]
            out, err = watch.communicate(timeout=EVENT_TIMEOUT_S)
            self.assertEqual(watch.returncode, 0, "watch rc=%d stderr=%s" % (watch.returncode, err))
            lines = [l for l in out.splitlines() if l.strip()]
            self.assertEqual(len(lines), 1, "exactly one event per committed change: %r" % out)
            ev = json.loads(lines[0])
            self.assertEqual(ev["op"], "event")
            self.assertEqual(ev["path"], "/bindings")
            self.assertIsInstance(ev["rev"], int)
            self.assertEqual(ev["rev"], self.rev())
            self.assertIn(bid, json.dumps(ev["value"]))
            self.assertTrue(any(b["id"] == bid for b in ev["value"]))
        finally:
            if watch.poll() is None:
                watch.kill()
                watch.communicate(timeout=KILL_TIMEOUT_S)

    def test_050_unbind_by_id_removes_it_and_unknown_id_exits_2(self):
        r0 = self.rev()
        p = self.stack.ds("unbind", "b0001", "--json")
        out = self.jout(p)
        self.assertEqual(out["ok"], True)
        self.assertEqual(out["result"]["removed"], ["b0001"])
        self.assertEqual(out["rev"], r0 + 1, "one unbind = exactly one rev bump")
        ids = [b["id"] for b in self.bindings()]
        self.assertNotIn("b0001", ids)
        # human output
        p = self.stack.ds("unbind", "b0002")
        self.assertEqual(p.returncode, 0, p.stderr)
        m = re.match(r"^ok rev=(\d+) removed=(b\d{4,})$", p.stdout.strip())
        self.assertIsNotNone(m, "got %r" % p.stdout)
        self.assertEqual(m.group(2), "b0002")
        # unknown id -> exit 2, E_PATH envelope with --json
        p = self.stack.ds("unbind", "b0099", "--json")
        self.assertEqual(p.returncode, 2, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        env = json.loads(p.stderr)
        self.assertEqual(env["ok"], False)
        self.assertEqual(env["error"]["code"], "E_PATH")
        self.assertIn("b0099", env["error"]["message"])
        self.assertEqual(self.rev(), r0 + 2, "a failed unbind must not bump rev")

    def test_060_unbind_by_topic_removes_all_matching(self):
        # two bindings on the same topic plus one on another
        p = self.bind("app/demo/same", "sleep 1")
        id_a = self.jout(p)["result"]["id"]
        p = self.bind("app/demo/same", "sleep 2")
        id_b = self.jout(p)["result"]["id"]
        p = self.bind("app/demo/other", "sleep 3")
        id_c = self.jout(p)["result"]["id"]
        r0 = self.rev()
        p = self.stack.ds("unbind", "--topic", "app/demo/same", "--json")
        out = self.jout(p)
        self.assertEqual(out["result"]["removed"], sorted([id_a, id_b]))
        self.assertEqual(out["rev"], r0 + 1, "one unbind op = one rev bump even for several removals")
        remaining = {b["id"]: b["topic"] for b in self.bindings()}
        self.assertNotIn(id_a, remaining)
        self.assertNotIn(id_b, remaining)
        self.assertEqual(remaining.get(id_c), "app/demo/other")
        # a topic with no binding -> exit 2 E_PATH
        p = self.stack.ds("unbind", "--topic", "app/demo/none", "--json")
        self.assertEqual(p.returncode, 2)
        self.assertEqual(json.loads(p.stderr)["error"]["code"], "E_PATH")

    def test_070_negative_cases_exit_1_with_e_schema(self):
        # code-ish step (T2/R8) — rejected client-side before connecting
        p = self.bind("deck/control/keys.2/down", '{"do":"eval","code":"1"}')
        self.assertEqual(p.returncode, 1, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        env = json.loads(p.stderr)
        self.assertEqual(env["error"]["code"], "E_SCHEMA")
        # bad topic
        p = self.bind("not-a-topic", "sleep 1")
        self.assertEqual(p.returncode, 1)
        self.assertEqual(json.loads(p.stderr)["error"]["code"], "E_SCHEMA")
        # topic with an embedded (non-segment) wildcard is rejected by the grammar
        p = self.bind("deck/control/keys*/down", "sleep 1")
        self.assertEqual(p.returncode, 1, p.stderr)
        # empty steps
        p = self.stack.ds("bind", "deck/x", "--json")
        self.assertEqual(p.returncode, 1, "bind without --do must exit 1")
        # `when` without value for eq
        p = self.bind("deck/x", "sleep 1", when='{"payload":"app","op":"eq"}')
        self.assertEqual(p.returncode, 1)
        self.assertEqual(json.loads(p.stderr)["error"]["code"], "E_SCHEMA")
        # `when` with both path and payload
        p = self.bind("deck/x", "sleep 1",
                      when='{"path":"/focus","payload":"app","op":"exists"}')
        self.assertEqual(p.returncode, 1)
        # `when` with an unknown op
        p = self.bind("deck/x", "sleep 1", when='{"payload":"app","op":"regex","value":".*"}')
        self.assertEqual(p.returncode, 1)
        # malformed unbind id -> exit 1 (usage), not 2
        p = self.stack.ds("unbind", "x1", "--json")
        self.assertEqual(p.returncode, 1)
        # nothing was stored by any of the failed binds
        for b in self.bindings():
            self.assertNotEqual(b["topic"], "deck/x")
            self.assertNotEqual(b["topic"], "not-a-topic")

    def test_080_when_is_stored_verbatim_and_does_not_fire(self):
        # a `when` condition that does not match must not fire; evaluation itself is
        # DS-042, so we assert the stored shape and the events, not execution
        when = '{"payload":"key","op":"eq","value":9}'
        p = self.bind("deck/control/keys.0/down",
                      'set /controls/keys/5/image {"kind":"color","value":"#123456"}',
                      when=when)
        bid = self.jout(p)["result"]["id"]
        stored = [b for b in self.bindings() if b["id"] == bid][0]
        self.assertEqual(stored["when"], {"payload": "key", "op": "eq", "value": 9})
        # the topic event IS published on the bus (router, DS-040) ...
        sub = self.conn()
        ctl = self.conn()
        try:
            r = sub.rpc({"id": 2, "op": "subscribe", "path": "/deck/control/keys/*"})
            self.assertEqual(r["ok"], True, r)
            inj = self.stack.inject([{"module": "keys", "keys": [0]}])
            self.assertEqual(inj.returncode, 0, inj.stderr)
            ev = self.next_event(sub, want_topic="deck/control/keys.0/down")
            self.assertEqual(ev["payload"]["key"], 0)
            self.assertEqual(ev["payload"]["pressed"], True)
            # ... but the binding's set step did not run: /controls/keys/5/image was
            # never written (the `when` payload key=0 != 9; and execution is DS-042)
            p = self.stack.ds("get", "/controls/keys/5/image", "--json")
            self.assertEqual(p.returncode, 2, "no binding step may have run: %s" % p.stderr)
            # the binding itself is still stored, unchanged
            stored2 = self.get_json("/bindings/%s" % bid)["value"]
            self.assertEqual(stored2["when"], {"payload": "key", "op": "eq", "value": 9})
            self.assertEqual(len(stored2["steps"]), 1)
        finally:
            sub.close()
            ctl.close()

    def test_090_raw_ipc_bind_unbind_round_trip(self):
        c = self.conn()
        try:
            r0 = c.rpc({"id": 2, "op": "get", "path": "/meta"})["rev"]
            # bind: the request id is a placeholder, the daemon assigns the real one
            r = c.rpc({
                "id": 3, "op": "bind",
                "binding": {"id": "b0000", "topic": "deck/system/rev",
                            "when": {"payload": "n", "op": "gt", "value": 0},
                            "steps": [{"do": "sleep", "ms": 1}]},
            })
            self.assertEqual(r["ok"], True, r)
            self.assertEqual(r["rev"], r0 + 1)
            bid = r["result"]["id"]
            self.assertTrue(ID_RE.match(bid), bid)
            self.assertNotEqual(bid, "b0000", "the placeholder must never be stored")
            r = c.rpc({"id": 4, "op": "get", "path": "/bindings/%s" % bid})
            self.assertEqual(r["ok"], True, r)
            self.assertEqual(r["result"]["value"]["id"], bid)
            self.assertEqual(r["result"]["value"]["when"]["op"], "gt")
            # unbind by id over raw IPC
            r = c.rpc({"id": 5, "op": "unbind", "binding": bid})
            self.assertEqual(r["ok"], True, r)
            self.assertEqual(r["result"]["removed"], [bid])
            self.assertEqual(r["rev"], r0 + 2)
            # unknown id over raw IPC -> E_PATH
            r = c.rpc({"id": 6, "op": "unbind", "binding": "b0099"})
            self.assertEqual(r["ok"], False)
            self.assertEqual(r["error"]["code"], "E_PATH")
            # unbind by topic over raw IPC
            r = c.rpc({"id": 7, "op": "bind", "binding": {
                "id": "b0000", "topic": "app/ipc/t",
                "steps": [{"do": "emit", "topic": "app/ipc/x", "payload": {}}]}})
            self.assertEqual(r["ok"], True, r)
            bid2 = r["result"]["id"]
            r = c.rpc({"id": 8, "op": "unbind", "topic": "app/ipc/t"})
            self.assertEqual(r["ok"], True, r)
            self.assertEqual(r["result"]["removed"], [bid2])
            # a code-ish step over raw IPC -> E_SCHEMA
            r = c.rpc({"id": 9, "op": "bind", "binding": {
                "id": "b0000", "topic": "app/ipc/bad",
                "steps": [{"do": "eval", "code": "1"}]}})
            self.assertEqual(r["ok"], False)
            self.assertEqual(r["error"]["code"], "E_SCHEMA")
        finally:
            c.close()

    def test_100_binding_ids_are_never_reused_after_unbind(self):
        p = self.bind("app/reuse/x", "sleep 1")
        bid = self.jout(p)["result"]["id"]
        p = self.stack.ds("unbind", bid, "--json")
        self.assertEqual(self.jout(p)["result"]["removed"], [bid])
        p = self.bind("app/reuse/y", "sleep 1")
        bid2 = self.jout(p)["result"]["id"]
        self.assertNotEqual(bid2, bid, "an unbound id must never be re-allocated")
        self.assertGreater(int(bid2[1:]), int(bid[1:]), "ids are monotonic")

    # ---- DS-042: step execution + when evaluation (black box) ---------------

    def test_110_bind_trigger_runs_set_emit_steps_in_order(self):
        # bind keys.2/down: set label "first", set label "second", emit app/demo/order
        p = self.bind(
            "deck/control/keys.2/down",
            'set /controls/keys/2/label "first"',
            'set /controls/keys/2/label "second"',
            'emit app/demo/order {"n": 1}',
        )
        bid = self.jout(p)["result"]["id"]
        rev0 = self.rev()
        # watch the two committed label changes (order observable on the wire)
        watch = subprocess.Popen(
            [str(self.stack.bins / "ds"), "--socket", str(self.stack.sock),
             "--data-dir", str(self.stack.data),
             "watch", "/controls/keys/2", "--ndjson", "--max-events", "2"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            env={**os.environ, "DS_DAEMON_BIN": str(self.stack.bins / "ds-daemon")},
        )
        # IPC subscriptions for the emitted topic and for system error events
        sub = self.conn()
        try:
            r = sub.rpc({"id": 2, "op": "subscribe", "path": "/app/*"})
            self.assertEqual(r["ok"], True, r)
            time.sleep(1.0)
            self.assertIsNone(watch.poll(), "watch exited before the trigger")
            inj = self.stack.inject([{"module": "keys", "keys": [2]}])
            self.assertEqual(inj.returncode, 0, inj.stderr)
            # the two set steps committed in binding order: "first" then "second"
            out, err = watch.communicate(timeout=EVENT_TIMEOUT_S)
            self.assertEqual(watch.returncode, 0, "watch rc=%d stderr=%s" % (watch.returncode, err))
            lines = [l for l in out.splitlines() if l.strip()]
            self.assertEqual(len(lines), 2, "two set steps -> two events: %r" % out)
            evs = [json.loads(l) for l in lines]
            self.assertEqual([e["value"] for e in evs], ["first", "second"],
                             "steps must run sequentially in binding order: %r" % evs)
            self.assertEqual([e["path"] for e in evs],
                             ["/controls/keys/2/label", "/controls/keys/2/label"])
            self.assertEqual(evs[1]["rev"] - evs[0]["rev"], 1, "one rev bump per set step")
            # final state carries the LAST step's value; exactly two rev bumps happened
            self.assertEqual(self.get_json("/controls/keys/2/label")["value"], "second")
            deadline = time.monotonic() + EVENT_TIMEOUT_S
            while self.rev() < rev0 + 2 and time.monotonic() < deadline:
                time.sleep(0.05)
            self.assertEqual(self.rev(), rev0 + 2, "two set steps = exactly two rev bumps")
            # the emit step published app/demo/order on the bus with its payload
            ev = self.next_event(sub, want_topic="app/demo/order")
            self.assertEqual(ev["payload"], {"n": 1})
            # the binding itself is still stored verbatim
            stored = self.get_json("/bindings/%s" % bid)["value"]
            self.assertEqual([s["do"] for s in stored["steps"]], ["set", "set", "emit"])
        finally:
            if watch.poll() is None:
                watch.kill()
                watch.communicate(timeout=KILL_TIMEOUT_S)
            sub.close()

    def test_120_when_on_payload_gates_the_binding(self):
        # matching payload field fires; a non-matching one does not (S7)
        p = self.bind("deck/control/keys.3/down",
                      'set /controls/keys/3/label "matched"',
                      when='{"payload":"key","op":"eq","value":3}')
        bid_ok = self.jout(p)["result"]["id"]
        p = self.bind("deck/control/keys.3/down",
                      'set /controls/keys/3/label "wrong"',
                      when='{"payload":"key","op":"eq","value":9}')
        bid_no = self.jout(p)["result"]["id"]
        rev0 = self.rev()
        inj = self.stack.inject([{"module": "keys", "keys": [3]}])
        self.assertEqual(inj.returncode, 0, inj.stderr)
        deadline = time.monotonic() + EVENT_TIMEOUT_S
        while time.monotonic() < deadline:
            p = self.stack.ds("get", "/controls/keys/3/label", "--json")
            if p.returncode == 0:
                break
            time.sleep(0.05)
        self.assertEqual(self.jout(p)["value"], "matched",
                         "only the binding whose when matches may fire")
        self.assertEqual(self.rev(), rev0 + 1, "exactly one set step ran")
        # the non-matching binding is still stored, untouched
        stored = self.get_json("/bindings/%s" % bid_no)["value"]
        self.assertEqual(stored["when"], {"payload": "key", "op": "eq", "value": 9})
        self.assertEqual(stored["id"], bid_no)
        self.assertNotEqual(bid_ok, bid_no)

    def test_130_schema_invalid_set_step_is_rejected_without_touching_state(self):
        # brightness must be an integer 1..=100 (schemas/state.json); "high" is invalid
        p = self.bind("deck/control/keys.4/down",
                      'set /controls/keys/4/brightness "high"')
        bid = self.jout(p)["result"]["id"]
        rev0 = self.rev()
        sub = self.conn()
        try:
            r = sub.rpc({"id": 2, "op": "subscribe", "path": "/deck/system/*"})
            self.assertEqual(r["ok"], True, r)
            time.sleep(0.5)
            inj = self.stack.inject([{"module": "keys", "keys": [4]}])
            self.assertEqual(inj.returncode, 0, inj.stderr)
            ev = self.next_event(sub, want_topic="deck/system/binding-error")
            self.assertEqual(ev["payload"]["binding"], bid)
            self.assertEqual(ev["payload"]["step"], 0)
            self.assertEqual(ev["payload"]["error"]["code"], "E_SCHEMA")
            # state untouched: the path was never written and rev did not move
            p = self.stack.ds("get", "/controls/keys/4/brightness", "--json")
            self.assertEqual(p.returncode, 2, "schema-invalid set must not touch state: %s" % p.stderr)
            self.assertEqual(json.loads(p.stderr)["error"]["code"], "E_PATH")
            deadline = time.monotonic() + 2
            while time.monotonic() < deadline:
                if self.rev() != rev0:
                    self.fail("a rejected set step must not bump rev")
                time.sleep(0.05)
        finally:
            sub.close()

    def test_140_emit_loop_hits_the_depth_cap_and_the_daemon_stays_healthy(self):
        # a self-emitting binding must be capped at depth 10 with a clear error event
        p = self.bind("app/demo/loop", 'emit app/demo/loop {}')
        bid = self.jout(p)["result"]["id"]
        sub = self.conn()
        ctl = self.conn()
        try:
            r = sub.rpc({"id": 2, "op": "subscribe", "path": "/deck/system/*"})
            self.assertEqual(r["ok"], True, r)
            time.sleep(0.5)
            r = ctl.rpc({"id": 3, "op": "emit", "topic": "app/demo/loop", "payload": {}})
            self.assertEqual(r["ok"], True, r)
            ev = self.next_event(sub, want_topic="deck/system/binding-error")
            self.assertEqual(ev["payload"]["binding"], bid)
            self.assertEqual(ev["payload"]["step"], 0)
            self.assertEqual(ev["payload"]["error"]["code"], "E_LIMIT")
            self.assertIn("cap 10", ev["payload"]["error"]["message"])
            self.assertEqual(ev["payload"]["depth"], 9, "depth 9 dispatches, its emit is refused")
            # the daemon is still healthy: status answers and a set still commits
            p = self.stack.ds("daemon", "status", "--json")
            self.assertEqual(p.returncode, 0, p.stderr)
            self.assertTrue(self.jout(p)["running"])
            r0 = self.rev()
            p = self.stack.ds("set", "/controls/keys/5/label", "--json", json.dumps("still-alive"))
            self.assertEqual(p.returncode, 0, p.stderr)
            env = self.jout(p)
            self.assertEqual(env["rev"], r0 + 1)
            self.assertEqual(self.get_json("/controls/keys/5/label")["value"], "still-alive")
        finally:
            sub.close()
            ctl.close()

    def test_150_plugin_step_stub_fails_closed_with_binding_error(self):
        # until DS-050 the plugin step returns a typed error and the router survives;
        # remaining steps still run
        p = self.bind("deck/control/keys.6/down",
                      'plugin demo.bump {"by":1}',
                      'set /controls/keys/6/label "after-plugin"')
        bid = self.jout(p)["result"]["id"]
        rev0 = self.rev()
        sub = self.conn()
        try:
            r = sub.rpc({"id": 2, "op": "subscribe", "path": "/deck/system/*"})
            self.assertEqual(r["ok"], True, r)
            time.sleep(0.5)
            inj = self.stack.inject([{"module": "keys", "keys": [6]}])
            self.assertEqual(inj.returncode, 0, inj.stderr)
            ev = self.next_event(sub, want_topic="deck/system/binding-error")
            self.assertEqual(ev["payload"]["binding"], bid)
            self.assertEqual(ev["payload"]["step"], 0)
            self.assertEqual(ev["payload"]["error"]["code"], "E_CAP")
            self.assertIn("DS-050", ev["payload"]["error"]["message"])
            # the remaining step still ran (failing step does not abort the binding)
            deadline = time.monotonic() + EVENT_TIMEOUT_S
            while time.monotonic() < deadline:
                p = self.stack.ds("get", "/controls/keys/6/label", "--json")
                if p.returncode == 0:
                    break
                time.sleep(0.05)
            self.assertEqual(self.jout(p)["value"], "after-plugin")
            self.assertEqual(self.rev(), rev0 + 1)
        finally:
            sub.close()


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
    raise TimeoutError("bindings suite exceeded the %ss watchdog" % SUITE_TIMEOUT_S)


def main():
    signal.signal(signal.SIGALRM, _alarm)
    signal.alarm(SUITE_TIMEOUT_S)
    try:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(Bindings)
        runner = unittest.TextTestRunner(stream=sys.stdout, verbosity=0,
                                         resultclass=LineResult)
        result = runner.run(suite)
    finally:
        signal.alarm(0)
    n = result.testsRun
    bad = len(result.failures) + len(result.errors)
    print("-" * 60)
    print("bindings conformance: %d tests, %d passed, %d failed/errored" % (n, n - bad, bad))
    if bad:
        print("RESULT: FAIL")
        return 1
    print("RESULT: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
