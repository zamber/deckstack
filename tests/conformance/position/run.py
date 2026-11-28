#!/usr/bin/env python3
"""deckstack DS-112 position/orientation conformance suite (tests/conformance/position/).

Black-box tests over the REAL binaries only: boots ds-vdev + ds-daemon on a fresh temp
dir and drives the system through `ds` and `ds-vdevctl` (slots / move / orientation /
replug / frames), asserting the DS-112 contract of docs/contracts/state.md:

    010 /devices/<serial> carries slot + position + orientation for every module
    020 `move` (reseat in another connector) updates /devices position, keeps the same
        serial key and logical control addressing, publishes deck/device/<serial>/moved,
        and does NOT restart the daemon (pid unchanged)
    030 `replug` (hot-unplug) -> daemon reconnects, re-handshakes, re-pushes the cached
        frame with an INTACT sha256 (ds-vdevctl frames), /devices back to connected,
        pid unchanged (no app restart)
    040 `orientation` (base rotation) updates /devices orientation for every module and
        publishes deck/device/<serial>/orientation, without a USB event or a restart
    050 move keys to another slot + replug: /devices position follows, frame sha intact

The permutation matrix itself (module kind x connector x base orientation) lives in
crates/ds-vdev/tests/ds112_permutations.rs (cargo test -p ds-vdev).

Usage:
    python3 tests/conformance/position/run.py [--verbose]

Exit code 0 = all green. Every wait carries a timeout; the daemon and vdev are always
killed (SIGTERM then SIGKILL) and the temp dir removed.
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
WAIT_TIMEOUT_S = 15
EVENT_TIMEOUT_S = 10
KILL_TIMEOUT_S = 5
SUITE_TIMEOUT_S = 300

BINARIES = ("ds", "ds-daemon", "ds-vdev", "ds-vdevctl")

# DS-112 identity expectations (SALON-001 rot0 anchors; position = base-4 decode of the
# port path tagged by its length): knobs 1-2-3 -> 70, keys 1-3-4 -> 75, base 1-4-4 -> 79
KNOBS_SERIAL = "virtual-0202"
KEYS_SERIAL = "virtual-01fe"


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
    """One booted ds-vdev + ds-daemon pair on a fresh temp dir."""

    def __init__(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="ds-position-"))
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
    """Raw NDJSON control-socket client (black box), select-based reads."""

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
        r = self.rpc({"id": 0, "op": "hello", "protocol": 1, "client": "position/1"})
        assert r["ok"], r
        r = self.rpc({"id": 1, "op": "auth", "token": token, "scope": "admin"})
        assert r["ok"], r
        return self

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


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


class Position(unittest.TestCase):
    """The DS-112 position/orientation conformance cases."""

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
            self.fail("position stack did not boot: %s" % self.boot_error)
        self.stack = Position.stack
        self.token = self.stack.token

    # ---- helpers -----------------------------------------------------------

    def jout(self, p):
        self.assertEqual(p.returncode, 0, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        return json.loads(p.stdout)

    def devices(self):
        return self.jout(self.stack.ds("get", "/devices", "--json"))["value"]

    def device(self, serial):
        return self.devices()[serial]

    def wait_for(self, what, timeout=WAIT_TIMEOUT_S, pred=None):
        deadline = time.monotonic() + timeout
        last = None
        while time.monotonic() < deadline:
            try:
                last = pred()
                if last:
                    return last
            except Exception as e:  # noqa: BLE001 - retry until the deadline
                last = e
            time.sleep(0.1)
        try:
            snap = {k: (v.get("orientation"), v.get("connection")) for k, v in self.devices().items()}
        except Exception as e:  # noqa: BLE001
            snap = repr(e)
        self.fail("timed out after %ss waiting for %s (last: %r; /devices now: %s)"
                  % (timeout, what, last, snap))

    def alive(self, pid):
        try:
            os.kill(pid, 0)
            return True
        except OSError:
            return False

    def frame_on_vdev(self, target="key:0"):
        p = self.stack.vdevctl("frames", target)
        return p.returncode == 0

    def frame_sha(self, target="key:0"):
        p = self.stack.vdevctl("frames", target)
        self.assertEqual(p.returncode, 0, p.stderr)
        # "<target> <sha256> <bytes>B seq=<n>"
        return p.stdout.split()[1]

    def conn(self):
        return Ipc(self.stack.sock).hello_auth(self.token)

    def next_event(self, c, want_topic=None, timeout=EVENT_TIMEOUT_S):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            line = c.recv(timeout=max(0.1, deadline - time.monotonic()))
            if line.get("op") == "event" and (want_topic is None or line.get("topic") == want_topic):
                return line
        raise AssertionError("no event with topic %r within %ss" % (want_topic, timeout))

    # ---- cases -------------------------------------------------------------

    def test_010_devices_carry_slot_position_orientation(self):
        # the registry thread commits /devices asynchronously right after connect
        devs = self.wait_for("6 device entries", pred=lambda: (
            self.devices() if len(self.devices()) == 6 else None
        ))
        self.assertEqual(len(devs), 6, devs)
        # SALON-001 rot0 anchors: slot + derived connector position + base orientation
        expect = {
            "virtual-01f6": ("m0-base", "1-4-4", 79),
            "virtual-01fe": ("keys", "1-3-4", 75),
            "virtual-0200": ("faders", "1-1-3", 66),
            "virtual-0202": ("knobs", "1-2-3", 70),
            "virtual-0204": ("rollers", "1-3-4", 75),
            "virtual-0206": ("dial", "1-1-3", 66),
        }
        for serial, (kind, slot, position) in expect.items():
            d = devs[serial]
            self.assertEqual(d["kind"], kind, serial)
            self.assertEqual(d["slot"], slot, serial)
            self.assertEqual(d["position"], position, serial)
            self.assertEqual(d["orientation"], "horizontal", serial)
            self.assertEqual(d["connection"], "connected", serial)
            self.assertIn("pid", d)
            self.assertIn("last_seen_rev", d)
        # the vdev's own slot view agrees
        p = self.stack.vdevctl("slots")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertIn("knobs pid=0x0202 slot=1-2-3 position=70", p.stdout)

    def test_020_move_updates_position_keeps_logical_addressing_no_restart(self):
        pid = self.stack.daemon_pid
        sub = self.conn()
        try:
            r = sub.rpc({"id": 2, "op": "subscribe", "path": "/deck/device/*"})
            self.assertTrue(r["ok"], r)
            # reseat the knobs module in another connector (1-4-3 -> position 78)
            p = self.stack.vdevctl("move", "knobs", "1-4-3")
            self.assertEqual(p.returncode, 0, p.stderr)
            self.assertIn("position=78", p.stdout)
            # /devices/<serial> follows the position; the serial key is retained
            d = self.wait_for("knobs /devices position=78", pred=lambda: (
                self.device(KNOBS_SERIAL)
                if self.device(KNOBS_SERIAL).get("slot") == "1-4-3" and self.device(KNOBS_SERIAL).get("position") == 78
                else None
            ))
            self.assertEqual(d["connection"], "connected")
            self.assertEqual(d["orientation"], "horizontal")
            # a deck/device/<serial>/moved event was published
            ev = self.next_event(sub, want_topic="deck/device/%s/moved" % KNOBS_SERIAL)
            self.assertEqual(ev["payload"]["slot"], "1-4-3")
            self.assertEqual(ev["payload"]["position"], 78)
            # logical addressing is retained: /controls/knobs/0 still drives the module
            p = self.stack.ds("set", "/controls/knobs/0/led", "--json",
                              '{"mode":"static","color":"#ff8800","brightness":50}')
            self.assertEqual(p.returncode, 0, p.stderr)
            # no app/daemon restart
            self.assertEqual(json.loads(
                self.stack.ds("daemon", "status", "--json").stdout)["pid"], pid)
            self.assertTrue(self.alive(pid))
        finally:
            sub.close()

    def test_030_replug_repushes_frame_with_intact_sha_no_restart(self):
        pid = self.stack.daemon_pid
        # push a frame to key:0 and record its sha on the vdev (wait for THIS push to land,
        # not for any older frame a previous test may have left on the device)
        sha_prev = self.frame_sha("key:0") if self.frame_on_vdev() else None
        p = self.stack.ds("set", "/controls/keys/0/image", "--json",
                          '{"kind":"color","value":"#3cc86e"}')
        self.assertEqual(p.returncode, 0, p.stderr)
        sha_a = self.wait_for("key:0 frame on vdev", pred=lambda: (
            self.frame_sha("key:0")
            if self.frame_on_vdev() and self.frame_sha("key:0") != sha_prev
            else None
        ))

        # hot-unplug every host connection; the daemon must reconnect + re-push
        p = self.stack.vdevctl("replug")
        self.assertEqual(p.returncode, 0, p.stderr)
        # /devices goes back to connected for every module and the frame sha is intact
        self.wait_for("all devices connected after replug", pred=lambda: (
            self.devices()
            if all(d.get("connection") == "connected" for d in self.devices().values())
            else None
        ))
        sha_b = self.wait_for("re-pushed frame after replug", pred=lambda: self.frame_sha("key:0") or None)
        self.assertEqual(sha_a, sha_b, "frame sha256 intact after the re-push")
        # the re-push actually re-sent the frame (seq advanced past the first push)
        # and the daemon never restarted
        self.assertEqual(json.loads(self.stack.ds("daemon", "status", "--json").stdout)["pid"], pid)
        self.assertTrue(self.alive(pid))

    def test_040_orientation_updates_devices_without_usb_event_or_restart(self):
        pid = self.stack.daemon_pid
        sub = self.conn()
        try:
            r = sub.rpc({"id": 2, "op": "subscribe", "path": "/deck/device/*"})
            self.assertTrue(r["ok"], r)
            p = self.stack.vdevctl("orientation", "vertical")
            self.assertEqual(p.returncode, 0, p.stderr)
            # every /devices entry flips to vertical (orientation is deck-wide)
            def all_vertical():
                devs = self.devices()
                if devs and all(d.get("orientation") == "vertical" for d in devs.values()):
                    return devs
                return None
            devs = self.wait_for("orientation=vertical on all devices", pred=all_vertical)
            self.assertEqual(len(devs), 6)
            # the base orientation event arrived on the bus (no USB hot-plug involved)
            ev = self.next_event(sub, want_topic="deck/device/virtual-01f6/orientation")
            self.assertEqual(ev["payload"]["orientation"], "vertical")
            self.assertEqual(json.loads(self.stack.ds("daemon", "status", "--json").stdout)["pid"], pid)
            self.assertTrue(self.alive(pid))
            # and back to flat
            p = self.stack.vdevctl("orientation", "horizontal")
            self.assertEqual(p.returncode, 0, p.stderr)
            self.wait_for("orientation=horizontal again", pred=lambda: (
                self.devices()
                if all(d.get("orientation") == "horizontal" for d in self.devices().values())
                else None
            ))
        finally:
            sub.close()

    def test_050_move_keys_then_replug_keeps_frame_sha_and_updates_position(self):
        pid = self.stack.daemon_pid
        sha_prev = self.frame_sha("key:0") if self.frame_on_vdev() else None
        p = self.stack.ds("set", "/controls/keys/0/image", "--json",
                          '{"kind":"color","value":"#ff8800"}')
        self.assertEqual(p.returncode, 0, p.stderr)
        sha_a = self.wait_for("key:0 frame (B) on vdev", pred=lambda: (
            self.frame_sha("key:0")
            if self.frame_on_vdev() and self.frame_sha("key:0") != sha_prev
            else None
        ))

        # move the keys module to connector 3-1 (position 24), then replug
        p = self.stack.vdevctl("move", "keys", "3-1")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertIn("position=24", p.stdout)
        d = self.wait_for("keys /devices slot=3-1", pred=lambda: (
            self.device(KEYS_SERIAL)
            if self.device(KEYS_SERIAL).get("slot") == "3-1" and self.device(KEYS_SERIAL).get("position") == 24
            else None
        ))
        self.assertEqual(d["connection"], "connected")
        p = self.stack.vdevctl("replug")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.wait_for("keys connected at 3-1 after replug", pred=lambda: (
            self.device(KEYS_SERIAL)
            if self.device(KEYS_SERIAL).get("connection") == "connected"
            and self.device(KEYS_SERIAL).get("slot") == "3-1"
            else None
        ))
        sha_b = self.wait_for("re-pushed frame after move+replug", pred=lambda: self.frame_sha("key:0") or None)
        self.assertEqual(sha_a, sha_b, "frame sha256 intact after move+replug re-push")
        self.assertEqual(json.loads(self.stack.ds("daemon", "status", "--json").stdout)["pid"], pid)
        self.assertTrue(self.alive(pid))


def position_suite():
    """Registered into tests/conformance/run.py so the DS-112 position/orientation cases
    run with the main conformance gate (`-k position` selects only these)."""
    return unittest.defaultTestLoader.loadTestsFromTestCase(Position)


def _alarm(signum, frame):
    raise TimeoutError("position suite exceeded the %ss watchdog" % SUITE_TIMEOUT_S)


def main():
    signal.signal(signal.SIGALRM, _alarm)
    signal.alarm(SUITE_TIMEOUT_S)
    try:
        suite = position_suite()
        runner = unittest.TextTestRunner(stream=sys.stdout, verbosity=0,
                                         resultclass=LineResult)
        result = runner.run(suite)
    finally:
        signal.alarm(0)
    n = result.testsRun
    bad = len(result.failures) + len(result.errors)
    print("-" * 60)
    print("position conformance: %d tests, %d passed, %d failed/errored" % (n, n - bad, bad))
    if bad:
        print("RESULT: FAIL")
        return 1
    print("RESULT: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
