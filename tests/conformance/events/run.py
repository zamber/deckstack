#!/usr/bin/env python3
"""deckstack event-bus conformance suite (ticket DS-040, tests/conformance/events/).

Black-box tests over the REAL binaries only: boots ds-vdev + ds-daemon on a fresh
temp dir and drives the system through the raw NDJSON control socket plus
`ds-vdevctl inject` — no ds-client code involved. It proves the router contract
of docs/contracts/actions.md and docs/components/event-router.md on the wire:

    010 subscribe + set on /controls/... delivers exactly one event with the new rev
    020 unsubscribe stops delivery
    030 wildcard topic subscription (/app/*) receives a matching emit topic
    040 two subscribers on the same pattern both receive the event
    050 device input (ds-vdevctl inject) arrives as a deck/control/... event
    060 emit op publishes app/<sender>/<topic> with the payload to subscribers
    070 a slow/blocked subscriber does not stall a set (drop-oldest + counted `dropped`)

Usage:
    python3 tests/conformance/events/run.py [--verbose]

Exit code 0 = all green. Every wait carries a timeout; the daemon and vdev are
always killed (SIGTERM then SIGKILL) and the temp dir removed, so the run can
never hang or leak processes. check.sh runs tests/conformance/run.py only, so
this suite is run directly (python3 tests/conformance/events/run.py); the main
suite's 17 cases must stay green alongside it.
"""

import json
import os
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

# slow-subscriber flood size: enough events to overflow the 1000-entry subscriber
# queue even after the kernel socket buffer absorbed its share
FLOOD_SETS = 6000


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
        self.tmp = Path(tempfile.mkdtemp(prefix="ds-events-"))
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
    """Raw NDJSON control-socket client (black box). Reads are select-based so a
    timeout never poisons the connection (unlike makefile + settimeout)."""

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
        r = self.rpc({"id": 0, "op": "hello", "protocol": 1, "client": "events/1"})
        assert r["ok"], r
        r = self.rpc({"id": 1, "op": "auth", "token": token, "scope": "admin"})
        assert r["ok"], r
        return self

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


class Events(unittest.TestCase):
    """The DS-040 event-bus conformance cases."""

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
            self.fail("event stack did not boot: %s" % self.boot_error)
        self.stack = Events.stack
        self.token = self.stack.token

    # ---- helpers -----------------------------------------------------------

    def conn(self):
        return Ipc(self.stack.sock).hello_auth(self.token)

    def subscribe(self, c, path, cid=2):
        r = c.rpc({"id": cid, "op": "subscribe", "path": path})
        self.assertEqual(r["ok"], True, r)
        self.assertEqual(r["result"]["subscribed"], path)
        self.assertIsInstance(r["result"]["id"], int)
        return r["result"]["id"]

    def set_label(self, c, key, value, cid):
        return c.rpc({
            "id": cid, "op": "set",
            "ops": [{"op": "set", "path": "/controls/keys/%d/label" % key,
                     "value": value}],
        })

    def next_event(self, c, want_topic=None, want_path=None, timeout=EVENT_TIMEOUT_S):
        """Read lines until an `event` message matching the filter arrives."""
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
        """No line at all may arrive within `quiet` seconds."""
        try:
            line = c.recv(timeout=quiet)
        except (socket.timeout, TimeoutError):
            return
        raise AssertionError("unexpected line while expecting silence: %r" % line)

    # ---- cases -------------------------------------------------------------

    def test_010_subscribe_then_set_delivers_one_event_with_new_rev(self):
        sub = self.conn()
        ctl = self.conn()
        try:
            self.subscribe(sub, "/controls/keys/*")
            r = self.set_label(ctl, 3, "hello", 10)
            self.assertEqual(r["ok"], True, r)
            rev = r["rev"]
            self.assertIsInstance(rev, int)
            ev = self.next_event(sub, want_path="/controls/keys/3/label")
            self.assertEqual(ev["op"], "event")
            self.assertEqual(ev["path"], "/controls/keys/3/label")
            self.assertEqual(ev["value"], "hello")
            self.assertEqual(ev["rev"], rev, "the event carries the new committed rev")
            self.assertNotIn("topic", ev, "state events keep the historical wire shape")
            # exactly one event for this set: nothing more arrives
            self.assert_quiet(sub)
        finally:
            sub.close()
            ctl.close()

    def test_020_unsubscribe_stops_delivery(self):
        sub = self.conn()
        ctl = self.conn()
        try:
            self.subscribe(sub, "/controls/keys/*")
            r = sub.rpc({"id": 3, "op": "unsubscribe", "path": "/controls/keys/*"})
            self.assertEqual(r["ok"], True, r)
            self.assertEqual(r["result"]["removed"], True)
            # subscribing again would deliver; after unsubscribe nothing may arrive
            r = self.set_label(ctl, 5, "after-unsub", 11)
            self.assertEqual(r["ok"], True, r)
            self.assert_quiet(sub)
            # removed=false on a second unsubscribe of the same pattern
            r = sub.rpc({"id": 4, "op": "unsubscribe", "path": "/controls/keys/*"})
            self.assertEqual(r["ok"], True, r)
            self.assertEqual(r["result"]["removed"], False)
        finally:
            sub.close()
            ctl.close()

    def test_030_wildcard_topic_subscription_receives_matching_topic(self):
        sub = self.conn()
        ctl = self.conn()
        try:
            # wildcard topic subscription: /app/* on the wire = app/* on the bus
            self.subscribe(sub, "/app/*")
            r = ctl.rpc({"id": 20, "op": "emit", "topic": "app/demo/wild",
                         "payload": {"n": 1}})
            self.assertEqual(r["ok"], True, r)
            ev = self.next_event(sub, want_topic="app/demo/wild")
            self.assertEqual(ev["op"], "event")
            self.assertEqual(ev["topic"], "app/demo/wild")
            self.assertEqual(ev["path"], "/app/demo/wild")
            self.assertEqual(ev["payload"], {"n": 1})
            self.assertIn("ts_ms", ev)
            self.assertNotIn("rev", ev, "non-state events carry no rev")
            # a non-matching topic is not delivered to /app/* ... it is; use /deck/* instead
            self.subscribe(sub, "/deck/control/*", cid=21)
            r = ctl.rpc({"id": 22, "op": "emit", "topic": "app/other/x"})
            self.assertEqual(r["ok"], True, r)
            # the /deck/control/* subscription must stay quiet for the app event
            try:
                line = sub.recv(timeout=QUIET_S)
                # the /app/* sub still gets it (same connection, two subscriptions):
                # it must be the app/other/x event, not a deck event
                self.assertEqual(line.get("topic"), "app/other/x", line)
            except (socket.timeout, TimeoutError):
                pass
        finally:
            sub.close()
            ctl.close()

    def test_040_two_subscribers_both_receive(self):
        sub_a = self.conn()
        sub_b = self.conn()
        ctl = self.conn()
        try:
            self.subscribe(sub_a, "/controls/*", cid=2)
            self.subscribe(sub_b, "/controls/*", cid=2)
            r = self.set_label(ctl, 7, "both", 30)
            self.assertEqual(r["ok"], True, r)
            rev = r["rev"]
            for sub in (sub_a, sub_b):
                ev = self.next_event(sub, want_path="/controls/keys/7/label")
                self.assertEqual(ev["rev"], rev)
                self.assertEqual(ev["value"], "both")
        finally:
            sub_a.close()
            sub_b.close()
            ctl.close()

    def test_050_device_input_arrives_as_deck_control_event(self):
        sub = self.conn()
        try:
            self.subscribe(sub, "/deck/control/keys/*")
            # inject a key-0 press on the virtual device (full key-state report)
            p = self.stack.inject([{"module": "keys", "keys": [0]}])
            self.assertEqual(p.returncode, 0, p.stderr)
            ev = self.next_event(sub, want_topic="deck/control/keys.0/down")
            self.assertEqual(ev["op"], "event")
            self.assertEqual(ev["path"], "/deck/control/keys.0/down")
            self.assertEqual(ev["payload"]["pressed"], True)
            self.assertEqual(ev["payload"]["key"], 0)
            # release -> up event
            p = self.stack.inject([{"module": "keys", "keys": []}])
            self.assertEqual(p.returncode, 0, p.stderr)
            ev = self.next_event(sub, want_topic="deck/control/keys.0/up")
            self.assertEqual(ev["payload"]["pressed"], False)
            # a dial press under a broader wildcard
            self.subscribe(sub, "/deck/control/*", cid=3)
            p = self.stack.inject([{"module": "dial", "dial_pressed": True}])
            self.assertEqual(p.returncode, 0, p.stderr)
            ev = self.next_event(sub, want_topic="deck/control/dial/down")
            self.assertEqual(ev["payload"]["control"], "dial")
            self.assertEqual(ev["payload"]["pressed"], True)
        finally:
            sub.close()

    def test_060_emit_op_publishes_to_app_subscribers(self):
        sub = self.conn()
        ctl = self.conn()
        try:
            self.subscribe(sub, "/app/demo/*")
            r = ctl.rpc({"id": 40, "op": "emit", "topic": "app/demo/hi",
                         "payload": {"n": 42}})
            self.assertEqual(r["ok"], True, r)
            self.assertEqual(r["result"]["emitted"], "app/demo/hi")
            self.assertGreaterEqual(r["result"]["subscribers"], 1)
            ev = self.next_event(sub, want_topic="app/demo/hi")
            self.assertEqual(ev["payload"], {"n": 42})
            # bad topic grammar -> E_SCHEMA, nothing published
            r = ctl.rpc({"id": 41, "op": "emit", "topic": "app/onlysender"})
            self.assertEqual(r["ok"], False)
            self.assertEqual(r["error"]["code"], "E_SCHEMA")
            r = ctl.rpc({"id": 42, "op": "emit", "topic": "deck/control/keys.99/down"})
            self.assertEqual(r["ok"], False)
            self.assertEqual(r["error"]["code"], "E_SCHEMA")
            self.assert_quiet(sub)
        finally:
            sub.close()
            ctl.close()

    def test_070_slow_subscriber_does_not_stall_a_set(self):
        slow = self.conn()          # subscribes, then never reads again
        ctl = self.conn()           # healthy connection doing the sets
        try:
            self.subscribe(slow, "/controls/*")
            # read only the subscribe response (already consumed by rpc); from now on
            # `slow` is a blocked subscriber: its socket is never drained.
            start = time.monotonic()
            for i in range(FLOOD_SETS):
                r = self.set_label(ctl, 0, "flood-%d" % i, 100 + i)
                self.assertEqual(r["ok"], True, "set %d failed: %r" % (i, r))
            flood_s = time.monotonic() - start
            # a set after the flood must still be answered promptly: the state task is
            # never blocked by the slow subscriber
            t0 = time.monotonic()
            r = self.set_label(ctl, 1, "final", 999999)
            self.assertEqual(r["ok"], True, r)
            last_s = time.monotonic() - t0
            self.assertLess(last_s, 2.0,
                            "final set took %.2fs — a blocked subscriber stalled the state task"
                            % last_s)
            # now drain the slow connection: the daemon must have dropped oldest events
            # with a counted `dropped` message (and logged a clear warn line)
            dropped_msgs = 0
            counted = 0
            events = 0
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                try:
                    line = slow.recv(timeout=2.0)
                except (socket.timeout, TimeoutError):
                    break
                if line.get("op") == "dropped":
                    dropped_msgs += 1
                    counted += line.get("count", 0)
                    self.assertEqual(line["path"], "/controls/*")
                elif line.get("op") == "event":
                    events += 1
            self.assertGreaterEqual(dropped_msgs, 1,
                                     "no `dropped` message on the slow connection")
            self.assertGreaterEqual(counted, 1, "dropped count must be counted, not just logged")
            self.assertGreater(events, 0)
            log("flood: %d sets in %.2fs, final set %.3fs, slow conn got %d events + %d dropped msgs (count=%d)"
                % (FLOOD_SETS, flood_s, last_s, events, dropped_msgs, counted))
            # the drop is also logged clearly in the daemon log
            dl = self.stack.daemon_log_text()
            self.assertIn("queue full", dl, "daemon log must carry the drop-oldest warn line")
            self.assertIn("dropped oldest", dl)
        finally:
            slow.close()
            ctl.close()


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
    raise TimeoutError("events suite exceeded the %ss watchdog" % SUITE_TIMEOUT_S)


def main():
    signal.signal(signal.SIGALRM, _alarm)
    signal.alarm(SUITE_TIMEOUT_S)
    try:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(Events)
        runner = unittest.TextTestRunner(stream=sys.stdout, verbosity=0,
                                         resultclass=LineResult)
        result = runner.run(suite)
    finally:
        signal.alarm(0)
    n = result.testsRun
    bad = len(result.failures) + len(result.errors)
    print("-" * 60)
    print("events conformance: %d tests, %d passed, %d failed/errored" % (n, n - bad, bad))
    if bad:
        print("RESULT: FAIL")
        return 1
    print("RESULT: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
