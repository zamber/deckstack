#!/usr/bin/env python3
"""End-to-end demo of the example plugin (ticket DS-054, contract plugins.md §7/§10).

Black-box: drives the system only through the real binaries. No camera, no real
hardware — ds-vdev stands in for the deck.

    1. build ds/ds-daemon/ds-vdev/ds-vdevctl if missing (bounded; DS_BIN_DIR overrides)
    2. fresh temp dir: ds-vdev --socket <tmp>/vdev.sock, then
       ds daemon start --device virtual:<tmp>/vdev.sock --data-dir <tmp>/data
       with XDG_DATA_HOME=<tmp>/xdg (fresh => empty state, rev 0)
    3. install examples/plugins/demo into $XDG_DATA_HOME/deckstack/plugins/demo/
       (copy plugin.py + generate manifest.json via demo/make_manifest.py)
    4. ds bind deck/control/keys.0/down --do 'plugin demo.bump {"by":1}'
    5. ds-vdevctl inject a key press -> the plugin host starts demo; wait for
       /plugins/demo status == "running"
    6. subscribe /plugin/* on the control socket; inject two more key presses
       (invoke demo.bump twice) and assert the effects landed:
         - /controls/keys/0/label incremented by exactly 2
         - /controls/keys/0/image == {"kind":"color","value":"#ff8800"}   (visual)
         - a plugin/demo/bumped bus event with payload {"n": <label>}       (emit)
         - /plugins/demo/stats.invokes >= 2 and stats.violations == 0
    7. ds daemon stop; vdev + plugin child dead; temp dir removed

Exit 0 = all green. Every wait carries a timeout; daemon/vdev are always killed
(SIGTERM then SIGKILL) and the temp dir removed, so the run cannot hang or leak.

Usage: python3 examples/plugins/run_demo.py   (from the repo root)
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
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
DEMO = Path(__file__).resolve().parent / "demo"

BUILD_TIMEOUT_S = 900
OP_TIMEOUT_S = 20
BOOT_TIMEOUT_S = 30
READY_TIMEOUT_S = 20
EFFECT_TIMEOUT_S = 20
KILL_TIMEOUT_S = 5

BINARIES = ("ds", "ds-daemon", "ds-vdev", "ds-vdevctl")


def bin_dir():
    env = os.environ.get("DS_BIN_DIR")
    if env:
        return Path(env)
    for candidate in (ROOT / "target" / "release", ROOT / "target" / "debug"):
        if all((candidate / b).is_file() for b in BINARIES):
            return candidate
    return ROOT / "target" / "release"


def ensure_binaries():
    d = bin_dir()
    missing = [b for b in BINARIES if not (d / b).is_file()]
    if not missing:
        return d
    cargo = shutil.which("cargo")
    if cargo is None:
        raise RuntimeError("cargo not found and binaries missing from %s" % d)
    print("building binaries (bounded %ss): %s" % (BUILD_TIMEOUT_S, " ".join(missing)))
    subprocess.run([cargo, "build", "--release", "--bin", "ds", "--bin", "ds-daemon",
                    "--bin", "ds-vdev", "--bin", "ds-vdevctl"],
                   cwd=str(ROOT), timeout=BUILD_TIMEOUT_S, check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return d


class Ipc:
    """Raw NDJSON control-socket client (hello/auth/subscribe/recv)."""

    def __init__(self, sock_path, timeout=OP_TIMEOUT_S):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(timeout)
        self.sock.connect(str(sock_path))
        self.f = self.sock.makefile("rw", encoding="utf-8", newline="\n")

    def rpc(self, obj):
        self.f.write(json.dumps(obj) + "\n")
        self.f.flush()
        line = self.f.readline()
        if not line:
            raise AssertionError("daemon closed the control connection")
        return json.loads(line)

    def recv(self, timeout):
        self.sock.settimeout(timeout)
        line = self.f.readline()
        if not line:
            raise AssertionError("daemon closed the connection while waiting for an event")
        return json.loads(line)

    def hello_auth(self, token):
        r = self.rpc({"id": 0, "op": "hello", "protocol": 1, "client": "ds-054-demo/1"})
        assert r["ok"], r
        r = self.rpc({"id": 1, "op": "auth", "token": token, "scope": "admin"})
        assert r["ok"], r
        return self

    def close(self):
        try:
            self.f.close()
        finally:
            self.sock.close()


class Stack:
    def __init__(self, bins):
        self.bins = bins
        self.tmp = Path(tempfile.mkdtemp(prefix="ds-054-demo-"))
        self.xdg = self.tmp / "xdg"
        self.plugin_dir = self.xdg / "deckstack" / "plugins" / "demo"
        self.vdev_sock = self.tmp / "vdev.sock"
        self.data = self.tmp / "data"
        self.sock = self.data / "daemon.sock"
        self.vdev = None
        self.daemon_pid = None

    def env(self):
        env = dict(os.environ)
        env["DS_DAEMON_BIN"] = str(self.bins / "ds-daemon")
        env["XDG_DATA_HOME"] = str(self.xdg)
        return env

    def install_demo_plugin(self):
        self.plugin_dir.mkdir(parents=True)
        shutil.copy(DEMO / "plugin.py", self.plugin_dir / "plugin.py")
        p = subprocess.run([sys.executable, str(DEMO / "make_manifest.py"),
                            str(self.plugin_dir / "manifest.json")],
                           capture_output=True, text=True, timeout=OP_TIMEOUT_S, cwd=str(ROOT))
        if p.returncode != 0:
            raise RuntimeError("make_manifest failed rc=%d\n%s\n%s"
                               % (p.returncode, p.stdout, p.stderr))

    def start(self):
        self.vdev = subprocess.Popen(
            [str(self.bins / "ds-vdev"), "--socket", str(self.vdev_sock)],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            stdin=subprocess.DEVNULL, cwd=str(ROOT))
        self._wait_for_file(self.vdev_sock, 10, "ds-vdev device socket")
        p = subprocess.run(
            [str(self.bins / "ds"), "--socket", str(self.sock), "--data-dir", str(self.data),
             "daemon", "start", "--device", "virtual:%s" % self.vdev_sock,
             "--data-dir", str(self.data)],
            capture_output=True, text=True, timeout=BOOT_TIMEOUT_S, env=self.env(), cwd=str(ROOT))
        if p.returncode != 0:
            raise RuntimeError("ds daemon start failed rc=%d\n%s\n%s\ndaemon.log: %s"
                               % (p.returncode, p.stdout, p.stderr, self._read(self.data / "daemon.log")))
        self._wait_for_file(self.sock, 10, "daemon control socket")
        pid_file = self.data / "daemon.pid"
        if pid_file.is_file():
            try:
                self.daemon_pid = int(pid_file.read_text().strip())
            except ValueError:
                self.daemon_pid = None
        return self

    @staticmethod
    def _read(p):
        try:
            return Path(p).read_text()[-4000:]
        except OSError:
            return "<unreadable>"

    @staticmethod
    def _wait_for_file(path, timeout_s, what):
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            if path.exists():
                return
            time.sleep(0.05)
        raise RuntimeError("timed out after %ss waiting for %s (%s)" % (timeout_s, what, path))

    def ds(self, *args, timeout=OP_TIMEOUT_S):
        return subprocess.run(
            [str(self.bins / "ds"), "--socket", str(self.sock), "--data-dir", str(self.data)]
            + list(args),
            capture_output=True, text=True, timeout=timeout, env=self.env(), cwd=str(ROOT))

    def vdevctl(self, *args, timeout=OP_TIMEOUT_S):
        return subprocess.run(
            [str(self.bins / "ds-vdevctl"), "--socket", str(self.vdev_sock)] + list(args),
            capture_output=True, text=True, timeout=timeout, cwd=str(ROOT))

    def inject_key(self, key=0):
        # press + release: the vdev only emits on the false->true transition
        f = self.tmp / ("inject-%d.ndjson" % time.monotonic_ns())
        f.write_text(json.dumps({"module": "keys", "keys": [key]}) + "\n"
                     + json.dumps({"module": "keys", "keys": []}) + "\n")
        return self.vdevctl("inject", str(f))

    def get_json(self, path):
        p = self.ds("get", path, "--json")
        if p.returncode != 0:
            raise RuntimeError("ds get %s rc=%d stderr=%s" % (path, p.returncode, p.stderr))
        return json.loads(p.stdout)

    def get_value(self, path, default=None):
        try:
            return self.get_json(path)["value"]
        except Exception:
            return default

    def wait_plugin_running(self, timeout=READY_TIMEOUT_S):
        deadline = time.monotonic() + timeout
        last = None
        while time.monotonic() < deadline:
            try:
                v = self.get_json("/plugins/demo")["value"]
                last = v
                if v.get("status") == "running":
                    return v
            except Exception as e:
                last = e
            time.sleep(0.1)
        raise RuntimeError("plugin demo did not reach status=running within %ss (last: %r; "
                           "daemon.log tail: %s)" % (timeout, last, self._read(self.data / "daemon.log")))

    def stop(self):
        problems = []
        try:
            subprocess.run([str(self.bins / "ds"), "--socket", str(self.sock),
                            "--data-dir", str(self.data), "daemon", "stop"],
                           capture_output=True, text=True, timeout=OP_TIMEOUT_S,
                           env=self.env(), cwd=str(ROOT))
        except Exception:
            pass
        targets = []
        if self.vdev is not None and self.vdev.poll() is None:
            targets.append(self.vdev)
        if self.daemon_pid is not None and _pid_alive(self.daemon_pid):
            targets.append(self.daemon_pid)
        for t in targets:
            try:
                (t.terminate if isinstance(t, subprocess.Popen) else os.kill(t, signal.SIGTERM))
            except OSError:
                pass
        deadline = time.monotonic() + KILL_TIMEOUT_S
        while time.monotonic() < deadline and any(
                (t.poll() is None) if isinstance(t, subprocess.Popen) else _pid_alive(t)
                for t in targets):
            time.sleep(0.05)
        for t in targets:
            try:
                (t.kill if isinstance(t, subprocess.Popen) else os.kill(t, signal.SIGKILL))
            except OSError:
                pass
        time.sleep(0.2)
        leftover = _pgrep(str(self.plugin_dir / "plugin.py"))
        if leftover:
            problems.append("plugin child still alive after daemon stop: %s" % leftover)
            for pid in leftover:
                try:
                    os.kill(pid, signal.SIGKILL)
                except OSError:
                    pass
        shutil.rmtree(self.tmp, ignore_errors=True)
        if self.tmp.exists():
            problems.append("temp dir still exists: %s" % self.tmp)
        return problems


def _pid_alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


def _pgrep(pattern):
    try:
        out = subprocess.run(["pgrep", "-f", pattern], capture_output=True, text=True,
                             timeout=10).stdout
        return [int(x) for x in out.split()]
    except Exception:
        return []


def main():
    bins = ensure_binaries()
    failures = []

    def check(name, fn):
        try:
            fn()
            print("PASS  %s" % name)
        except Exception as e:  # report and continue to cleanup
            failures.append((name, "%s: %s" % (type(e).__name__, e)))
            print("FAIL  %s: %s: %s" % (name, type(e).__name__, e))

    stack = Stack(bins)
    bus = None
    try:
        stack.install_demo_plugin()

        def step_boot():
            stack.start()

        def step_bind():
            p = stack.ds("bind", "deck/control/keys.0/down", "--json",
                         "--do", 'plugin demo.bump {"by":1}')
            assert p.returncode == 0, "bind rc=%d stderr=%s" % (p.returncode, p.stderr)
            out = json.loads(p.stdout)
            assert out["ok"] is True, out

        def step_subscribe():
            nonlocal bus
            token = (stack.data / "token").read_text().strip()
            bus = Ipc(stack.sock).hello_auth(token)
            r = bus.rpc({"id": 2, "op": "subscribe", "path": "/plugin/*"})
            assert r["ok"] is True, r

        def step_plugin_starts():
            p = stack.inject_key(0)
            assert p.returncode == 0, "inject rc=%d stderr=%s" % (p.returncode, p.stderr)
            v = stack.wait_plugin_running()
            assert v["status"] == "running", v

        def step_invoke_twice_and_assert():
            before = stack.get_value("/controls/keys/0/label", "0")
            n0 = int(before) if isinstance(before, str) and before.isdigit() else 0
            for _ in range(2):
                p = stack.inject_key(0)
                assert p.returncode == 0, "inject rc=%d stderr=%s" % (p.returncode, p.stderr)
            # effects landed: label +2, image colour set (the visual)
            deadline = time.monotonic() + EFFECT_TIMEOUT_S
            label = None
            while time.monotonic() < deadline:
                label = stack.get_value("/controls/keys/0/label")
                if label == str(n0 + 2):
                    break
                time.sleep(0.1)
            assert label == str(n0 + 2), "label %r != %r" % (label, str(n0 + 2))
            image = stack.get_value("/controls/keys/0/image")
            assert image == {"kind": "color", "value": "#ff8800"}, image
            # emit effect observed on the bus
            deadline = time.monotonic() + EFFECT_TIMEOUT_S
            got = None
            while time.monotonic() < deadline:
                ev = bus.recv(timeout=max(0.5, deadline - time.monotonic()))
                if ev.get("op") == "event" and ev.get("topic") == "plugin/demo/bumped":
                    got = ev
                    if got["payload"] == {"n": n0 + 2}:
                        break
            assert got is not None, "no plugin/demo/bumped event observed"
            assert got["payload"] == {"n": n0 + 2}, got
            # lifecycle/stats at /plugins/demo
            v = stack.get_json("/plugins/demo")["value"]
            assert v["status"] == "running", v
            assert v["stats"]["invokes"] >= 2, v["stats"]
            assert v["stats"]["violations"] == 0, v["stats"]

        for name, fn in (
            ("boot ds-vdev + ds-daemon on a fresh temp dir", step_boot),
            ("bind key-down to 'plugin demo.bump {\"by\":1}'", step_bind),
            ("subscribe /plugin/* on the control socket", step_subscribe),
            ("key press brings plugin demo to status=running", step_plugin_starts),
            ("two invokes: label+2, image=#ff8800, plugin/demo/bumped event, stats clean",
             step_invoke_twice_and_assert),
        ):
            check(name, fn)
    except Exception as e:  # boot failure
        failures.append(("boot", "%s: %s" % (type(e).__name__, e)))
        print("ERROR boot: %s: %s" % (type(e).__name__, e))
    finally:
        if bus is not None:
            bus.close()
        problems = stack.stop()
        if problems:
            failures.append(("cleanup", "; ".join(problems)))
            print("FAIL  cleanup: %s" % "; ".join(problems))
        else:
            print("cleanup ok: daemon+vdev dead, plugin child dead, temp dir removed")

    print("-" * 72)
    if failures:
        print("RESULT: FAIL (%d failing step(s))" % len(failures))
        for name, msg in failures:
            print("  - %s: %s" % (name, msg))
        return 1
    print("RESULT: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
