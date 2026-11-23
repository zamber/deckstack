#!/usr/bin/env python3
"""deckstack action-chain conformance suite (ticket DS-045, tests/conformance/actions/).

Black-box tests over the REAL binaries only: boots ds-vdev + ds-daemon on a fresh
temp dir and drives the system through the `ds` binary, the raw NDJSON control
socket and `ds-vdevctl inject` — no ds-client/ds-state code involved. It proves
the two worked chains of docs/contracts/actions.md §6 plus the refused-step
behaviour of §2/§4 on the wire:

  chain A (device input -> binding -> visual):
    010 inject key-down on ds-vdev -> one deck/control/keys.0/down event on its one
        topic path (no duplicates, nothing on state-path subscriptions) -> the
        binding's set/set/emit steps run in order (label then image, one rev bump
        per set) -> the visual change is visible via `ds get` within 2 s
    020 repeated press/release cycles produce exactly one down + one up event per
        cycle and one rev bump per press (no double dispatch)

  chain B (focus -> binding -> refused step, S7 atomicity):
    030 emit app/focus/changed -> a payload-when binding AND a /focus/app-path-when
        binding both fire in the SAME dispatch (one rev bump for mirror + both
        sets), /controls/keys/3/image recoloured; a non-matching app fires neither
    040 a binding with a refused plugin step (E_CAP stub until DS-050) and a refused
        exec step (/bin/sh is deny-listed, E_CAP) publishes deck/system/binding-error
        per refused step (plus the deck/system/exec refusal audit) while the
        remaining set + emit + sleep steps still run

Usage:
    python3 tests/conformance/actions/run.py [--verbose]

Exit code 0 = all green. Every wait carries a timeout; the daemon and vdev are
always killed (SIGTERM then SIGKILL) and the temp dir removed, so the run can
never hang or leak processes. check.sh runs tests/conformance/run.py only, so
this suite is run directly (like tests/conformance/events/run.py,
bindings/run.py and focus_ingest/run.py); the main suite must stay green too.
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
CHAIN_A_BUDGET_S = 2.0          # actions.md §5/§6: key input -> visual <= 2 s end to end
QUIET_S = 0.8
KILL_TIMEOUT_S = 5
SUITE_TIMEOUT_S = 300

BINARIES = ("ds", "ds-daemon", "ds-vdev", "ds-vdevctl")

FOCUS_TOPIC = "app/focus/changed"

COLOR_A = {"kind": "color", "value": "#ff8800"}
COLOR_B = {"kind": "color", "value": "#00ff00"}
COLOR_C = {"kind": "color", "value": "#0000ff"}


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
        self.tmp = Path(tempfile.mkdtemp(prefix="ds-actions-"))
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
        """ds-vdevctl inject <ndjson>: replay scripted input events to the daemon."""
        f = self.tmp / ("inject-%d.ndjson" % time.monotonic_ns())
        f.write_text("".join(json.dumps(e) + "\n" for e in events))
        return self.vdevctl("inject", str(f), timeout=timeout)

    def ipc(self, timeout=OP_TIMEOUT_S):
        return Ipc(self.sock, timeout)

    def daemon_log_text(self):
        return self._read(self.daemon_log)

    def stop(self):
        """Mandatory cleanup: stop the daemon, SIGTERM then SIGKILL both processes,
        remove the temp dir. Returns a list of problems (empty = clean)."""
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
        # the vdev socket file inside the temp dir is removed with the dir itself
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
    """Raw NDJSON control-socket client (black box). Select-based reads so a
    timeout never poisons the connection."""

    def __init__(self, sock_path, timeout=OP_TIMEOUT_S):
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
            r, _, _ = select.select([self.sock], [], [], remaining)
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
        r = self.rpc({"id": 0, "op": "hello", "protocol": 1, "client": "actions/1"})
        assert r["ok"], r
        r = self.rpc({"id": 1, "op": "auth", "token": token, "scope": "admin"})
        assert r["ok"], r
        return self

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


class Actions(unittest.TestCase):
    """The DS-045 action-chain cases (actions.md §6 chains A/B)."""

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
            self.fail("actions stack did not boot: %s" % self.boot_error)
        self.stack = Actions.stack
        self.token = self.stack.token

    # ---- helpers -----------------------------------------------------------

    def jout(self, p):
        self.assertEqual(p.returncode, 0, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        return json.loads(p.stdout)

    def get_json(self, path):
        return self.jout(self.stack.ds("get", path, "--json"))

    def get_rc(self, path):
        return self.stack.ds("get", path, "--json")

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

    def subscribe(self, c, path, cid=2):
        r = c.rpc({"id": cid, "op": "subscribe", "path": path})
        self.assertEqual(r["ok"], True, r)
        return r["result"]["id"]

    def emit(self, c, topic, payload, id=100):
        r = c.rpc({"id": id, "op": "emit", "topic": topic, "payload": payload})
        self.assertEqual(r["ok"], True, r)
        return r

    def focus_payload(self, app, title="Window", pid=4242):
        return {"app": app, "title": title, "pid": pid}

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

    def assert_quiet(self, c, quiet=QUIET_S):
        try:
            line = c.recv(timeout=quiet)
        except (socket.timeout, TimeoutError):
            return
        raise AssertionError("unexpected line while expecting silence: %r" % line)

    def wait_until(self, pred, timeout=EVENT_TIMEOUT_S, what="condition"):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if pred():
                return
            time.sleep(0.02)
        self.fail("timed out after %ss waiting for %s" % (timeout, what))

    # ---- chain A: device input -> binding -> visual -------------------------

    def test_010_chain_a_key_press_to_visual_one_event_path_in_order(self):
        # chain A (actions.md §6): bind keys.0/down to set label, set image, emit;
        # inject a key-down on the vdev; assert ONE event on its one topic path,
        # the steps run in order (one rev bump per set), and the visual change is
        # visible via `ds get` within the 2 s budget
        p = self.bind(
            "deck/control/keys.0/down",
            'set /controls/keys/0/label "chainA"',
            'set /controls/keys/0/image %s' % json.dumps(COLOR_A),
            'emit app/actions/chainA {"n": 1}',
        )
        bid = self.jout(p)["result"]["id"]
        rev0 = self.rev()

        dev = self.conn()      # topic subscription: the one event path of the input
        state = self.conn()    # state-path subscription: the binding's set effects
        app = self.conn()      # the emit step's topic
        sys = self.conn()      # deck/system/*: any binding-error
        try:
            self.subscribe(dev, "/deck/control/keys/*")
            self.subscribe(state, "/controls/*")
            self.subscribe(app, "/app/*")
            self.subscribe(sys, "/deck/system/*")
            time.sleep(0.3)
            self.assert_quiet(dev, 0.3)
            self.assert_quiet(state, 0.3)

            t0 = time.monotonic()
            inj = self.stack.inject([{"module": "keys", "keys": [0]}])
            self.assertEqual(inj.returncode, 0, inj.stderr)

            # ONE event path: exactly one deck/control/keys.0/down, nothing else
            # on the topic subscription (no duplicate dispatch)
            ev = self.next_event(dev, want_topic="deck/control/keys.0/down")
            self.assertEqual(ev["op"], "event")
            self.assertEqual(ev["path"], "/deck/control/keys.0/down")
            self.assertEqual(ev["payload"]["key"], 0)
            self.assertEqual(ev["payload"]["pressed"], True)
            self.assertIn("ts_ms", ev)
            self.assertNotIn("rev", ev, "device events carry rev: null / no rev")
            self.assert_quiet(dev)

            # the binding's steps ran in order: label first, then image, one rev
            # bump per set effect, FIFO on the state subscription
            ev_label = self.next_event(state, want_path="/controls/keys/0/label")
            self.assertEqual(ev_label["value"], "chainA")
            self.assertIsInstance(ev_label["rev"], int)
            ev_img = self.next_event(state, want_path="/controls/keys/0/image")
            self.assertEqual(ev_img["value"], COLOR_A)
            self.assertEqual(ev_img["rev"], ev_label["rev"] + 1,
                             "one rev bump per set step, in binding order")

            # the emit step published its topic with the payload
            ev_emit = self.next_event(app, want_topic="app/actions/chainA")
            self.assertEqual(ev_emit["payload"], {"n": 1})

            # no refused step: no binding-error on deck/system/*
            self.assert_quiet(sys)

            # the visual change is visible via `ds get` within the 2 s budget
            def image_is_color():
                p = self.get_rc("/controls/keys/0/image")
                return p.returncode == 0 and json.loads(p.stdout)["value"] == COLOR_A
            self.wait_until(image_is_color, timeout=CHAIN_A_BUDGET_S,
                            what="the visual change via ds get")
            elapsed = time.monotonic() - t0
            self.assertLessEqual(elapsed, CHAIN_A_BUDGET_S,
                                 "key input -> visual took %.3fs, budget 2s" % elapsed)
            self.assertEqual(self.get_json("/controls/keys/0/label")["value"], "chainA")
            self.assertEqual(self.get_json("/controls/keys/0/image")["value"], COLOR_A)
            self.assertEqual(self.rev(), rev0 + 2, "exactly two set effects = two rev bumps")
            # the binding is still stored verbatim
            stored = self.get_json("/bindings/%s" % bid)["value"]
            self.assertEqual([s["do"] for s in stored["steps"]], ["set", "set", "emit"])
            print("chain A: key-down -> visual visible via get in %.1f ms (budget 2000 ms), "
                  "revs %d->%d, one event path" % (elapsed * 1000, rev0, self.rev()))
        finally:
            for c in (dev, state, app, sys):
                c.close()

    def test_020_chain_a_repeated_presses_one_event_per_press(self):
        # each press/release cycle produces exactly one down + one up topic event
        # and one rev bump (no double dispatch of the binding)
        p = self.bind("deck/control/keys.1/down",
                      'set /controls/keys/1/label "hit"')
        self.jout(p)
        rev0 = self.rev()
        dev = self.conn()
        try:
            self.subscribe(dev, "/deck/control/keys/*")
            time.sleep(0.3)
            for _ in range(2):
                inj = self.stack.inject([{"module": "keys", "keys": [1]}])
                self.assertEqual(inj.returncode, 0, inj.stderr)
                ev = self.next_event(dev, want_topic="deck/control/keys.1/down")
                self.assertEqual(ev["payload"]["pressed"], True)
                inj = self.stack.inject([{"module": "keys", "keys": []}])
                self.assertEqual(inj.returncode, 0, inj.stderr)
                ev = self.next_event(dev, want_topic="deck/control/keys.1/up")
                self.assertEqual(ev["payload"]["pressed"], False)
            self.assert_quiet(dev, 1.0)
            self.assertEqual(self.get_json("/controls/keys/1/label")["value"], "hit")
            self.assertEqual(self.rev(), rev0 + 2, "two presses = exactly two rev bumps")
        finally:
            dev.close()

    # ---- chain B: focus -> binding -> refused step (S7) ---------------------

    def test_030_chain_b_focus_payload_and_focus_path_when_fire_atomically(self):
        # chain B (actions.md §6, S7): on app/focus/changed BOTH a payload-when
        # binding and a /focus/app path-when binding fire in the same dispatch
        p = self.bind(
            FOCUS_TOPIC,
            'set /controls/keys/3/image %s' % json.dumps(COLOR_B),
            when='{"payload":"app","op":"eq","value":"code"}',
        )
        self.jout(p)
        p = self.bind(
            FOCUS_TOPIC,
            'set /controls/keys/3/label "focus-path-when"',
            when='{"path":"/focus/app","op":"eq","value":"code"}',
        )
        self.jout(p)
        c = self.conn()
        try:
            # non-matching app: neither binding fires, the mirror still updates
            rev0 = self.rev()
            self.emit(c, FOCUS_TOPIC, self.focus_payload("terminal"), id=110)
            self.wait_until(lambda: self.get_json("/focus")["value"]["app"] == "terminal",
                            what="the /focus mirror for terminal")
            self.assertEqual(self.rev(), rev0 + 1, "mirror = one rev bump")
            self.assertEqual(self.get_rc("/controls/keys/3/image").returncode, 2,
                             "payload-when must not fire for app=terminal")
            self.assertEqual(self.get_rc("/controls/keys/3/label").returncode, 2,
                             "path-when must not fire for app=terminal")

            # matching app: BOTH fire in the SAME dispatch (S7 atomic), one rev
            # bump for mirror + both set steps, and the visual is recoloured
            rev1 = self.rev()
            t0 = time.monotonic()
            self.emit(c, FOCUS_TOPIC, self.focus_payload("code"), id=111)
            self.wait_until(
                lambda: self.get_rc("/controls/keys/3/image").returncode == 0
                        and json.loads(self.get_rc("/controls/keys/3/image").stdout)["value"] == COLOR_B,
                timeout=CHAIN_A_BUDGET_S, what="the chain B recolor via ds get")
            elapsed = time.monotonic() - t0
            self.assertLessEqual(elapsed, CHAIN_A_BUDGET_S)
            self.assertEqual(self.get_json("/controls/keys/3/image")["value"], COLOR_B)
            self.assertEqual(self.get_json("/controls/keys/3/label")["value"], "focus-path-when")
            rev2 = self.rev()
            self.assertEqual(rev2, rev1 + 1,
                             "mirror + both binding set steps = ONE rev bump (S7 atomic)")
            focus = self.get_json("/focus")["value"]
            self.assertEqual(focus["app"], "code")
            self.assertEqual(focus["at_rev"], rev2,
                             "the /focus mirror commits in the dispatching rev")
            print("chain B: focus event -> payload-when + /focus-path-when both fired "
                  "atomically (one rev bump), keys/3 recoloured in %.1f ms"
                  % (elapsed * 1000))
        finally:
            c.close()

    def test_040_chain_b_refused_steps_binding_error_while_remaining_steps_run(self):
        # chain B refused-step behaviour: a binding on app/focus/changed whose first
        # steps are refused (plugin: E_CAP stub until DS-050; exec: fails closed
        # with E_CAP standing in for E_POLICY on the ingest path) must publish
        # deck/system/binding-error per refused step while the remaining
        # set + emit + sleep steps still run
        ctl = self.conn()
        try:
            binding = {
                "id": "b0000",  # placeholder; the daemon assigns the real id
                "topic": FOCUS_TOPIC,
                "allow_exec": True,
                "steps": [
                    {"do": "plugin", "id": "demo", "action": "bump", "args": {"by": 1}},
                    {"do": "exec", "argv": ["/bin/sh", "-c", "true"], "cwd": "/tmp"},
                    {"do": "set", "path": "/controls/keys/4/image",
                     "value": COLOR_C},
                    {"do": "emit", "topic": "app/actions/chainB", "payload": {"ok": True}},
                    {"do": "sleep", "ms": 50},
                ],
            }
            r = ctl.rpc({"id": 2, "op": "bind", "binding": binding})
            self.assertEqual(r["ok"], True, r)
            bid = r["result"]["id"]
            stored = self.get_json("/bindings/%s" % bid)["value"]
            self.assertEqual(stored["allow_exec"], True)
            self.assertEqual([s["do"] for s in stored["steps"]],
                             ["plugin", "exec", "set", "emit", "sleep"])
        finally:
            ctl.close()

        rev0 = self.rev()
        sys = self.conn()      # deck/system/*: binding-error + exec audit events
        app = self.conn()      # the emit step of the surviving steps
        state = self.conn()    # the set step of the surviving steps
        c = self.conn()
        try:
            self.subscribe(sys, "/deck/system/*")
            self.subscribe(app, "/app/*")
            self.subscribe(state, "/controls/keys/4")
            time.sleep(0.3)
            self.emit(c, FOCUS_TOPIC, self.focus_payload("code"), id=120)

            # refused plugin step (index 0): E_CAP stub until DS-050; refused exec
            # step (index 1): the ingest path fails exec closed (E_CAP standing in
            # for E_POLICY) — one deck/system/binding-error per refused step
            errs = []
            deadline = time.monotonic() + EVENT_TIMEOUT_S
            while time.monotonic() < deadline and len(errs) < 2:
                line = sys.recv(timeout=max(0.1, deadline - time.monotonic()))
                if line.get("op") == "event" and line.get("topic") == "deck/system/binding-error":
                    errs.append(line)
            self.assertEqual(len(errs), 2,
                             "one binding-error per refused step, got %r" % errs)
            for i, code_hint in ((0, "DS-050"), (1, "E_POLICY")):
                self.assertEqual(errs[i]["payload"]["binding"], bid)
                self.assertEqual(errs[i]["payload"]["step"], i)
                self.assertEqual(errs[i]["payload"]["error"]["code"], "E_CAP")
                self.assertIn(code_hint, errs[i]["payload"]["error"]["message"])
                self.assertEqual(errs[i]["payload"]["topic"], FOCUS_TOPIC)

            # the remaining steps still ran: the set step committed (one rev bump)
            # and the emit step published its topic
            ev_img = self.next_event(state, want_path="/controls/keys/4/image")
            self.assertEqual(ev_img["value"], COLOR_C)
            ev_emit = self.next_event(app, want_topic="app/actions/chainB")
            self.assertEqual(ev_emit["payload"], {"ok": True})
            self.wait_until(lambda: self.rev() == rev0 + 1,
                            what="exactly one rev bump for the surviving set step")
            self.assertEqual(self.get_json("/controls/keys/4/image")["value"], COLOR_C)
            # the /focus mirror of the triggering event still committed
            self.assertEqual(self.get_json("/focus")["value"]["app"], "code")
            # the daemon is healthy afterwards
            p = self.stack.ds("daemon", "status", "--json")
            self.assertEqual(self.jout(p)["running"], True)
            print("chain B refused steps: 2 binding-error events (E_CAP plugin stub "
                  "+ E_CAP exec fails closed) while set+emit+sleep still ran "
                  "(one rev bump), keys/4 = %s" % COLOR_C)
        finally:
            for c in (sys, app, state, c):
                c.close()


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
    raise TimeoutError("actions suite exceeded the %ss watchdog" % SUITE_TIMEOUT_S)


def main():
    signal.signal(signal.SIGALRM, _alarm)
    signal.alarm(SUITE_TIMEOUT_S)
    try:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(Actions)
        runner = unittest.TextTestRunner(stream=sys.stdout, verbosity=0,
                                         resultclass=LineResult)
        result = runner.run(suite)
    finally:
        signal.alarm(0)
    n = result.testsRun
    bad = len(result.failures) + len(result.errors)
    print("-" * 60)
    print("actions conformance: %d tests, %d passed, %d failed/errored, %d skipped"
          % (n, n - bad - len(result.skipped), bad, len(result.skipped)))
    if bad:
        print("RESULT: FAIL")
        return 1
    print("RESULT: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
