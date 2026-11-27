#!/usr/bin/env python3
"""deckstack transactional focus-ingest conformance suite (ticket DS-044,
tests/conformance/focus_ingest/).

Black-box tests over the REAL binaries only: boots ds-vdev + ds-daemon on a fresh
temp dir and drives the system through the `ds` binary and the raw NDJSON control
socket — no ds-client/ds-state code involved. It proves the S7 contract of
docs/contracts/actions.md §2/§6 and docs/components/event-router.md "Atomic event
ingest" on the wire:

    010 emit app/focus/changed (raw IPC emit op) -> /focus {app,title,pid,at_rev}
        mirrored with at_rev == the committing rev, exactly ONE rev bump
    020 a binding on app/focus/changed (payload when) runs its set step in the SAME
        transaction: /focus and the set path carry the same rev in their events
    030 a `when` on path /focus/app sees the NEW value in the same dispatch (chain B);
        the payload-based when also fires; a later binding reads committed /focus
        with at_rev == the dispatch rev
    040 invalid focus payloads (typed pid / unknown field, non-object) -> E_SCHEMA
        binding-error event, rev unchanged, /focus unchanged, no binding step runs
    045 DS-114: a payload with pid absent or null is accepted and mirrored into /focus
        without a pid key (one rev bump); a present non-integer pid ({"pid":"x"}) is
        still rejected with E_SCHEMA and no state change
    050 two quick events are ingested in arrival order: /focus = the second, two rev
        bumps, two /focus events with consecutive revs, the payload-gated binding
        fired exactly once
    060 a schema-invalid binding set step does not roll back the ingest: /focus still
        commits (one rev), the valid set step shares that rev, the invalid path is
        untouched, one E_SCHEMA binding-error event
    070 a binding-triggered emit is queued AFTER the ingest transaction (no
        reentrancy): the emitted topic is published once on the bus and the binding
        on it commits at a strictly later rev than /focus/at_rev

Usage:
    python3 tests/conformance/focus_ingest/run.py [--verbose]

Exit code 0 = all green. Every wait carries a timeout; the daemon and vdev are
always killed (SIGTERM then SIGKILL) and the temp dir removed, so the run can
never hang or leak processes. check.sh runs tests/conformance/run.py only, so this
suite is run directly (like tests/conformance/events/run.py and bindings/run.py);
the main suite must stay green alongside it.
"""

import json
import os
import select
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
KILL_TIMEOUT_S = 5
SUITE_TIMEOUT_S = 300

BINARIES = ("ds", "ds-daemon", "ds-vdev", "ds-vdevctl")

FOCUS_TOPIC = "app/focus/changed"


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
        raise RuntimeError("cargo not found and binaries missing from %s: %s" % (d, ", ".join(missing)))
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
        self.tmp = Path(tempfile.mkdtemp(prefix="ds-focus-ingest-"))
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
        r = self.rpc({"id": 0, "op": "hello", "protocol": 1, "client": "focus-ingest/1"})
        assert r["ok"], r
        r = self.rpc({"id": 1, "op": "auth", "token": token, "scope": "admin"})
        assert r["ok"], r
        return self

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


class FocusIngest(unittest.TestCase):
    """The DS-044 transactional focus-ingest cases (S7)."""

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
            self.fail("focus_ingest stack did not boot: %s" % self.boot_error)
        self.stack = FocusIngest.stack
        self.token = self.stack.token

    # ---- helpers -----------------------------------------------------------

    def jout(self, p):
        self.assertEqual(p.returncode, 0, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        return json.loads(p.stdout)

    def get_json(self, path):
        return self.jout(self.stack.ds("get", path, "--json"))

    def rev(self):
        return self.get_json("/meta")["rev"]

    def bind(self, topic, *dos, when=None):
        args = ["bind", topic]
        for d in dos:
            args += ["--do", d]
        if when is not None:
            args += ["--when", when]
        return self.stack.ds(*args, "--json")

    def conn(self):
        return Ipc(self.stack.sock).hello_auth(self.token)

    def emit(self, c, topic, payload, id=100):
        r = c.rpc({"id": id, "op": "emit", "topic": topic, "payload": payload})
        self.assertEqual(r["ok"], True, r)
        return r

    def focus_payload(self, app, title="Window", pid=4242):
        return {"app": app, "title": title, "pid": pid}

    def wait_until(self, pred, timeout=EVENT_TIMEOUT_S, what="condition"):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if pred():
                return
            time.sleep(0.05)
        self.fail("timed out after %ss waiting for %s" % (timeout, what))

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

    def watch(self, path, max_events):
        """Start `ds watch <path> --ndjson --max-events N` in the background."""
        return subprocess.Popen(
            [str(self.stack.bins / "ds"), "--socket", str(self.stack.sock),
             "--data-dir", str(self.stack.data),
             "watch", path, "--ndjson", "--max-events", str(max_events)],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            env={**os.environ, "DS_DAEMON_BIN": str(self.stack.bins / "ds-daemon")},
        )

    def watch_lines(self, w, timeout=EVENT_TIMEOUT_S):
        out, err = w.communicate(timeout=timeout)
        self.assertEqual(w.returncode, 0, "watch rc=%d stderr=%s" % (w.returncode, err))
        return [json.loads(l) for l in out.splitlines() if l.strip()]

    # ---- cases -------------------------------------------------------------

    def test_010_emit_focus_event_mirrors_into_focus_with_one_rev_bump(self):
        c = self.conn()
        try:
            rev0 = self.rev()
            r = self.emit(c, FOCUS_TOPIC, self.focus_payload("code"))
            self.assertEqual(r["result"]["emitted"], FOCUS_TOPIC)
            self.wait_until(lambda: self.get_json("/focus")["value"].get("app") == "code",
                            what="/focus mirror")
            rev1 = self.rev()
            self.assertEqual(rev1, rev0 + 1, "one focus event = exactly ONE rev bump")
            focus = self.get_json("/focus")["value"]
            self.assertEqual(focus, {"app": "code", "title": "Window", "pid": 4242,
                                     "at_rev": rev1})
            self.assertEqual(self.get_json("/focus")["rev"], rev1)
            # the topic event itself is still published on the bus for subscribers
            sub = self.conn()
            try:
                r = sub.rpc({"id": 2, "op": "subscribe", "path": "/app/focus/*"})
                self.assertEqual(r["ok"], True, r)
                self.emit(c, FOCUS_TOPIC, self.focus_payload("terminal"), id=101)
                ev = self.next_event(sub, want_topic=FOCUS_TOPIC)
                self.assertEqual(ev["payload"]["app"], "terminal")
                self.assertNotIn("rev", ev, "app events carry no rev (only state events do)")
                self.wait_until(lambda: self.get_json("/focus")["value"]["app"] == "terminal",
                                what="second mirror")
                self.assertEqual(self.rev(), rev1 + 1)
                self.assertEqual(self.get_json("/focus")["value"]["at_rev"], rev1 + 1)
            finally:
                sub.close()
        finally:
            c.close()

    def test_020_binding_set_step_commits_in_the_same_transaction_as_the_mirror(self):
        # chain B shape: bind app/focus/changed (payload when) -> set key 3 image
        p = self.bind(
            FOCUS_TOPIC,
            'set /controls/keys/3/image {"kind":"color","value":"#00ff00"}',
            when='{"payload":"app","op":"eq","value":"code"}',
        )
        bid = self.jout(p)["result"]["id"]
        self.assertTrue(bid.startswith("b"))
        rev0 = self.rev()
        w_focus = self.watch("/focus", 1)
        w_img = self.watch("/controls/keys/3", 1)
        time.sleep(1.0)  # let the watchers subscribe
        self.assertIsNone(w_focus.poll(), "focus watcher exited early")
        self.assertIsNone(w_img.poll(), "image watcher exited early")
        c = self.conn()
        try:
            self.emit(c, FOCUS_TOPIC, self.focus_payload("code"))
        finally:
            c.close()
        fevs = self.watch_lines(w_focus)
        ievs = self.watch_lines(w_img)
        rev1 = self.rev()
        self.assertEqual(rev1, rev0 + 1, "mirror + binding set step = ONE rev bump, not two")
        self.assertEqual(len(fevs), 1, "one /focus event: %r" % fevs)
        self.assertEqual(fevs[0]["path"], "/focus")
        self.assertEqual(fevs[0]["rev"], rev1)
        self.assertEqual(fevs[0]["value"]["at_rev"], rev1)
        img = [e for e in ievs if e["path"] == "/controls/keys/3/image"]
        self.assertEqual(len(img), 1, "the set step ran: %r" % ievs)
        self.assertEqual(img[0]["rev"], rev1,
                         "the binding's set step committed in the SAME transaction as /focus")
        self.assertEqual(img[0]["value"], {"kind": "color", "value": "#00ff00"})
        self.assertEqual(self.get_json("/controls/keys/3/image")["value"],
                         {"kind": "color", "value": "#00ff00"})

    def test_030_when_on_focus_path_sees_the_new_value_in_the_same_dispatch(self):
        # path-based when on /focus/app must fire on the triggering value (S7, not stale)
        p = self.bind(
            FOCUS_TOPIC,
            'set /controls/keys/7/label "path-when-fired"',
            when='{"path":"/focus/app","op":"eq","value":"code"}',
        )
        self.assertEqual(self.jout(p)["ok"], True)
        # payload-based when on the same topic fires too (chain B)
        p = self.bind(
            FOCUS_TOPIC,
            'set /controls/keys/8/label "payload-when-fired"',
            when='{"payload":"app","op":"eq","value":"code"}',
        )
        self.assertEqual(self.jout(p)["ok"], True)
        # a binding on another topic reads the COMMITTED /focus afterwards
        p = self.bind(
            "app/demo/level",
            'set /controls/keys/9/label "level-fired"',
            when='{"path":"/focus/app","op":"eq","value":"code"}',
        )
        self.assertEqual(self.jout(p)["ok"], True)
        c = self.conn()
        try:
            # non-matching payload: neither focus binding fires
            rev0 = self.rev()
            self.emit(c, FOCUS_TOPIC, self.focus_payload("terminal"), id=110)
            self.wait_until(lambda: self.get_json("/focus")["value"]["app"] == "terminal",
                            what="mirror for terminal")
            self.assertEqual(self.rev(), rev0 + 1, "mirror still updates on a non-matching payload")
            p = self.stack.ds("get", "/controls/keys/7/label", "--json")
            self.assertEqual(p.returncode, 2, "path-when must not fire for app=terminal")
            p = self.stack.ds("get", "/controls/keys/8/label", "--json")
            self.assertEqual(p.returncode, 2, "payload-when must not fire for app=terminal")
            # matching payload: both fire in the same dispatch, one rev bump
            rev1 = self.rev()
            self.emit(c, FOCUS_TOPIC, self.focus_payload("code"), id=111)
            self.wait_until(
                lambda: self.stack.ds("get", "/controls/keys/7/label", "--json").returncode == 0,
                what="path-based when fired in the same dispatch")
            self.assertEqual(self.get_json("/controls/keys/7/label")["value"], "path-when-fired")
            self.assertEqual(self.get_json("/controls/keys/8/label")["value"], "payload-when-fired")
            rev2 = self.rev()
            self.assertEqual(rev2, rev1 + 1,
                             "mirror + both binding set steps = still ONE rev bump")
            self.assertEqual(self.get_json("/focus")["value"]["at_rev"], rev2)
            # the later binding on app/demo/level sees the committed /focus (at_rev == dispatch rev)
            self.emit(c, "app/demo/level", {}, id=112)
            self.wait_until(
                lambda: self.stack.ds("get", "/controls/keys/9/label", "--json").returncode == 0,
                what="level binding fired on committed /focus")
            self.assertEqual(self.get_json("/controls/keys/9/label")["value"], "level-fired")
            self.assertEqual(self.get_json("/focus")["value"]["at_rev"], rev2,
                             "at_rev is the rev of the dispatch that wrote /focus")
        finally:
            c.close()

    def test_040_invalid_focus_payload_changes_nothing_and_reports_a_clear_error(self):
        c = self.conn()
        errs = self.conn()
        try:
            r = errs.rpc({"id": 2, "op": "subscribe", "path": "/deck/system/*"})
            self.assertEqual(r["ok"], True, r)
            rev0 = self.rev()
            focus_before = self.get_json("/focus")["value"]
            bad_payloads = [
                {"app": "code", "title": "W", "pid": "4242"},          # pid not an integer
                {"app": "code", "title": "W", "pid": 1, "extra": 0},   # unknown field
                "not-an-object",
                [1, 2, 3],
            ]
            for i, bad in enumerate(bad_payloads):
                self.emit(c, FOCUS_TOPIC, bad, id=200 + i)
            # every rejection is a clear error event on deck/system/binding-error
            for i in range(len(bad_payloads)):
                ev = self.next_event(errs, want_topic="deck/system/binding-error")
                self.assertEqual(ev["payload"]["stage"], "ingest")
                self.assertEqual(ev["payload"]["topic"], FOCUS_TOPIC)
                self.assertEqual(ev["payload"]["error"]["code"], "E_SCHEMA")
                self.assertIn("app/focus/changed", ev["payload"]["error"]["message"])
            self.assertEqual(self.rev(), rev0, "invalid payloads must not bump rev")
            self.assertEqual(self.get_json("/focus")["value"], focus_before,
                             "/focus must be untouched by rejected payloads")
            # no binding step ran either (the 030 bindings are still gated on app=code)
            # a valid payload still works right after the rejections
            self.emit(c, FOCUS_TOPIC, self.focus_payload("code"), id=299)
            self.wait_until(lambda: self.get_json("/focus")["value"]["at_rev"] == self.rev(),
                            what="valid payload after rejections")
            self.assertEqual(self.rev(), rev0 + 1)
        finally:
            c.close()
            errs.close()

    def test_045_focus_event_without_pid_is_accepted_and_mirrors(self):
        # DS-114: {"app":"steam","title":"Steam"} (pid absent) and pid:null are
        # accepted and update /focus (no pid key, one rev bump); {"pid":"x"} is
        # still rejected with E_SCHEMA and no state change.
        c = self.conn()
        errs = self.conn()
        try:
            r = errs.rpc({"id": 2, "op": "subscribe", "path": "/deck/system/*"})
            self.assertEqual(r["ok"], True, r)
            rev0 = self.rev()
            # pid absent -> accepted, mirrored without a pid key
            self.emit(c, FOCUS_TOPIC, {"app": "steam", "title": "Steam"}, id=250)
            self.wait_until(lambda: self.get_json("/focus")["value"].get("app") == "steam",
                            what="pid-less /focus mirror")
            rev1 = self.rev()
            self.assertEqual(rev1, rev0 + 1, "one pid-less event = exactly ONE rev bump")
            self.assertEqual(self.get_json("/focus")["value"],
                             {"app": "steam", "title": "Steam", "at_rev": rev1})
            self.assertNotIn("pid", self.get_json("/focus")["value"])
            # pid:null -> equally accepted
            self.emit(c, FOCUS_TOPIC,
                      {"app": "steam", "title": "Steam", "pid": None}, id=251)
            self.wait_until(lambda: self.get_json("/focus")["value"]["at_rev"] == rev1 + 1,
                            what="null-pid /focus mirror")
            self.assertEqual(self.get_json("/focus")["value"],
                             {"app": "steam", "title": "Steam", "at_rev": rev1 + 1})
            self.assertEqual(self.rev(), rev1 + 1)
            # a present non-integer pid is still rejected: E_SCHEMA, no state change
            self.emit(c, FOCUS_TOPIC,
                      {"app": "steam", "title": "Steam", "pid": "x"}, id=252)
            ev = self.next_event(errs, want_topic="deck/system/binding-error")
            self.assertEqual(ev["payload"]["stage"], "ingest")
            self.assertEqual(ev["payload"]["topic"], FOCUS_TOPIC)
            self.assertEqual(ev["payload"]["error"]["code"], "E_SCHEMA")
            self.assertEqual(self.rev(), rev1 + 1, "the rejected payload must not bump rev")
            self.assertEqual(self.get_json("/focus")["value"]["at_rev"], rev1 + 1,
                             "/focus untouched by the rejected payload")
        finally:
            c.close()
            errs.close()

    def test_050_two_quick_events_are_ingested_in_arrival_order(self):
        p = self.bind(
            FOCUS_TOPIC,
            'set /controls/keys/4/label "fired"',
            when='{"payload":"app","op":"eq","value":"code"}',
        )
        self.assertEqual(self.jout(p)["ok"], True)
        rev0 = self.rev()
        w_focus = self.watch("/focus", 2)
        sub = self.conn()
        try:
            r = sub.rpc({"id": 2, "op": "subscribe", "path": "/controls/keys/4"})
            self.assertEqual(r["ok"], True, r)
            time.sleep(1.0)
            self.assertIsNone(w_focus.poll(), "focus watcher exited early")
            c = self.conn()
            try:
                # two quick events on one connection: arrival order must be preserved
                self.emit(c, FOCUS_TOPIC, self.focus_payload("code"), id=300)
                self.emit(c, FOCUS_TOPIC, self.focus_payload("terminal"), id=301)
            finally:
                c.close()
            self.wait_until(lambda: self.get_json("/focus")["value"]["app"] == "terminal",
                            what="second event ingested")
            rev2 = self.rev()
            self.assertEqual(rev2, rev0 + 2, "two events = exactly two rev bumps")
            self.assertEqual(self.get_json("/focus")["value"]["app"], "terminal",
                             "the second event wins")
            self.assertEqual(self.get_json("/focus")["value"]["at_rev"], rev2)
            fevs = self.watch_lines(w_focus)
            self.assertEqual(len(fevs), 2, "one /focus event per ingested event: %r" % fevs)
            self.assertEqual(fevs[0]["rev"], rev0 + 1)
            self.assertEqual(fevs[1]["rev"], rev0 + 2)
            self.assertEqual(fevs[0]["value"]["app"], "code")
            self.assertEqual(fevs[1]["value"]["app"], "terminal")
            # the payload-gated binding fired exactly once (for the first event)
            evs = []
            deadline = time.monotonic() + 2.0
            while time.monotonic() < deadline:
                try:
                    line = sub.recv(timeout=0.3)
                except socket.timeout:
                    break
                if line.get("op") == "event" and line.get("path") == "/controls/keys/4/label":
                    evs.append(line)
            self.assertEqual(len(evs), 1, "the binding fired once, for the first event only: %r" % evs)
            self.assertEqual(evs[0]["rev"], rev0 + 1, "fired inside the first event's transaction")
            self.assertEqual(evs[0]["value"], "fired")
        finally:
            sub.close()

    def test_060_failing_binding_step_does_not_roll_back_the_ingest(self):
        # schema-invalid set step (brightness must be an integer 1..=100) + a valid one
        p = self.bind(
            FOCUS_TOPIC,
            'set /controls/keys/5/brightness "high"',
            'set /controls/keys/5/label "still-committed"',
            when='{"payload":"app","op":"eq","value":"code"}',
        )
        self.assertEqual(self.jout(p)["ok"], True)
        errs = self.conn()
        try:
            r = errs.rpc({"id": 2, "op": "subscribe", "path": "/deck/system/*"})
            self.assertEqual(r["ok"], True, r)
            time.sleep(0.5)
            rev0 = self.rev()
            c = self.conn()
            try:
                self.emit(c, FOCUS_TOPIC, self.focus_payload("code"), id=400)
            finally:
                c.close()
            self.wait_until(
                lambda: self.stack.ds("get", "/controls/keys/5/label", "--json").returncode == 0,
                what="the valid set step committed")
            rev1 = self.rev()
            self.assertEqual(rev1, rev0 + 1, "one transaction: mirror + valid set, one rev")
            self.assertEqual(self.get_json("/focus")["value"]["at_rev"], rev1,
                             "the mirror committed despite the failing step")
            self.assertEqual(self.get_json("/controls/keys/5/label")["value"], "still-committed")
            p = self.stack.ds("get", "/controls/keys/5/brightness", "--json")
            self.assertEqual(p.returncode, 2, "the schema-invalid set step did not run")
            ev = self.next_event(errs, want_topic="deck/system/binding-error")
            self.assertEqual(ev["payload"]["stage"], "ingest")
            self.assertEqual(ev["payload"]["error"]["code"], "E_SCHEMA")
            self.assertEqual(ev["payload"]["step"], 0)
            self.assertEqual(ev["payload"]["topic"], FOCUS_TOPIC)
        finally:
            errs.close()

    def test_070_binding_triggered_emit_is_queued_after_the_transaction(self):
        p = self.bind(FOCUS_TOPIC, 'emit app/demo/fromfocus {"n": 1}')
        self.assertEqual(self.jout(p)["ok"], True)
        p = self.bind("app/demo/fromfocus", 'set /controls/keys/6/label "after-emit"')
        self.assertEqual(self.jout(p)["ok"], True)
        app = self.conn()
        try:
            r = app.rpc({"id": 2, "op": "subscribe", "path": "/app/*"})
            self.assertEqual(r["ok"], True, r)
            time.sleep(0.5)
            w_label = self.watch("/controls/keys/6", 1)
            w_focus = self.watch("/focus", 1)
            time.sleep(1.0)
            self.assertIsNone(w_label.poll(), "label watcher exited early")
            self.assertIsNone(w_focus.poll(), "focus watcher exited early")
            rev0 = self.rev()
            c = self.conn()
            try:
                self.emit(c, FOCUS_TOPIC, self.focus_payload("code"), id=500)
            finally:
                c.close()
            # the emitted topic reached bus subscribers exactly once
            ev = self.next_event(app, want_topic="app/demo/fromfocus")
            self.assertEqual(ev["payload"], {"n": 1})
            try:
                self.next_event(app, want_topic="app/demo/fromfocus", timeout=1.0)
                self.fail("the emitted topic was published more than once")
            except (AssertionError, socket.timeout):
                pass
            # both committed, but the emit-triggered set is a LATER rev than the mirror
            fevs = self.watch_lines(w_focus)
            levs = self.watch_lines(w_label)
            self.assertEqual(len(fevs), 1)
            label = [e for e in levs if e["path"] == "/controls/keys/6/label"]
            self.assertEqual(len(label), 1, "%r" % levs)
            self.assertEqual(label[0]["value"], "after-emit")
            self.assertEqual(fevs[0]["rev"], rev0 + 1, "ingest transaction = one rev")
            self.assertEqual(fevs[0]["value"]["at_rev"], rev0 + 1)
            self.assertGreater(label[0]["rev"], rev0 + 1,
                               "the emit-triggered set must be queued AFTER the ingest transaction")
        finally:
            app.close()


class LineResult(unittest.TextTestResult):
    """One clear line per test."""

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
    raise TimeoutError("focus_ingest suite exceeded the %ss watchdog" % SUITE_TIMEOUT_S)


def main():
    signal.signal(signal.SIGALRM, _alarm)
    signal.alarm(SUITE_TIMEOUT_S)
    try:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(FocusIngest)
        runner = unittest.TextTestRunner(stream=sys.stdout, verbosity=0,
                                         resultclass=LineResult)
        result = runner.run(suite)
    finally:
        signal.alarm(0)
    n = result.testsRun
    bad = len(result.failures) + len(result.errors)
    print("-" * 60)
    print("focus_ingest: %d tests, %d passed, %d failed/errored, %d skipped"
          % (n, n - bad - len(result.skipped), bad, len(result.skipped)))
    if bad:
        print("RESULT: FAIL")
        return 1
    print("RESULT: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
