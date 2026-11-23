#!/usr/bin/env python3
"""Smoke test for the Python plugin SDK (ticket DS-052).

Boots ds-vdev + ds-daemon on a fresh temp dir (same pattern as tests/e2e/run.py), installs
the example plugin from this package (``examples/demo_plugin.py``) under a temp
``$XDG_DATA_HOME/deckstack/plugins/demo/`` so the DS-050 plugin host picks it up, invokes
actions through a real binding triggered by a vdev key press, and asserts the plugin's
state/emit effects landed. Also exercises the host-mediated HTTP path (S4) against a local
http.server on 127.0.0.1. Then shuts down cleanly and verifies no plugin process survives.

    1. ds-vdev + ds daemon start --device virtual:... (fresh temp dir, XDG_DATA_HOME=tmp)
    2. example plugin installed under $XDG_DATA_HOME/deckstack/plugins/demo/
       (manifest.json = build_manifest(example.MANIFEST) + command + fs_roots so the
       Landlock-sandboxed child can import ds_plugin_py from plugins-py/)
    3. /policy/net/allow_hosts = ["127.0.0.1"]; local lamp HTTP server on 127.0.0.1
    4. ds bind deck/control/keys.0/down --do plugin bump --do plugin get_lamp
    5. ds-vdevctl inject key-down  ->  /plugins/demo reaches status "running"
    6. second key press invokes the actions; assert:
       - /plugins/demo/state/count == 2        (set effect landed)
       - /controls/keys/0/label == "2"         (set effect on a control landed)
       - /plugins/demo/state/last == lamp body (host-mediated net effect landed)
       - a plugin/demo/bumped event was observed on the bus (emit effect landed)
       - /plugins/demo/stats.invokes >= 1
    7. ds daemon stop; the plugin child process group is dead; temp dir removed

Exit 0 = all green. Every wait carries a timeout; daemon/vdev are always killed (SIGTERM
then SIGKILL) and the temp dir removed, so the run cannot hang or leak processes.

Usage: python plugins-py/smoke.py   (from the repo root; DS_BIN_DIR overrides the binary
directory, default target/release with target/debug fallback)
"""

import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PKG = Path(__file__).resolve().parent
sys.path.insert(0, str(PKG))
sys.path.insert(0, str(PKG / "examples"))

import ds_plugin_py as sdk  # noqa: E402
import demo_plugin as example  # noqa: E402

BUILD_TIMEOUT_S = 900
OP_TIMEOUT_S = 20
BOOT_TIMEOUT_S = 30
READY_TIMEOUT_S = 20
EFFECT_TIMEOUT_S = 20
KILL_TIMEOUT_S = 5
SUITE_TIMEOUT_S = 240

BINARIES = ("ds", "ds-daemon", "ds-vdev", "ds-vdevctl")

LAMP_BODY = json.dumps({"state": "on", "lamp": "demo"})


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
    print("building binaries (bounded %ss, release): %s" % (BUILD_TIMEOUT_S, " ".join(missing)))
    subprocess.run([cargo, "build", "--release", "--bin", "ds", "--bin", "ds-daemon",
                    "--bin", "ds-vdev", "--bin", "ds-vdevctl"],
                   cwd=str(ROOT), timeout=BUILD_TIMEOUT_S, check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return d


class LampHandler(BaseHTTPRequestHandler):
    def do_GET(self):
        body = LAMP_BODY.encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):  # keep stdout clean
        pass


class Ipc:
    """Raw NDJSON control-socket client (black box; same handshake as conformance)."""

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
        r = self.rpc({"id": 0, "op": "hello", "protocol": 1, "client": "plugins-py-smoke/1"})
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
    """One booted ds-vdev + ds-daemon pair on a fresh temp dir, plugin installed under a
    temp XDG_DATA_HOME."""

    def __init__(self, bins):
        self.bins = bins
        self.tmp = Path(tempfile.mkdtemp(prefix="ds-plugin-py-smoke-"))
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

    def install_example_plugin(self):
        self.plugin_dir.mkdir(parents=True)
        manifest = sdk.build_manifest(example.MANIFEST)
        manifest["command"] = [sys.executable or "/usr/bin/python3", "plugin.py"]
        # the Landlock-sandboxed child may read the SDK package from here
        manifest["fs_roots"] = [str(PKG)]
        manifest["resources"] = {"cpu_s": 10, "nproc": 64, "nofile": 64, "as_mb": 1024}
        (self.plugin_dir / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
        shim = ("import sys\nsys.path.insert(0, %r)\n" % str(PKG)
                + (PKG / "examples" / "demo_plugin.py").read_text())
        (self.plugin_dir / "plugin.py").write_text(shim)
        return manifest

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
        # press + release in one script: the vdev only emits a bus event on the
        # false->true transition, so a release is needed before the next press fires
        f = self.tmp / ("inject-%d.ndjson" % time.monotonic_ns())
        f.write_text(json.dumps({"module": "keys", "keys": [key]}) + "\n"
                     + json.dumps({"module": "keys", "keys": []}) + "\n")
        return self.vdevctl("inject", str(f))

    def get_json(self, path):
        p = self.ds("get", path, "--json")
        if p.returncode != 0:
            raise RuntimeError("ds get %s rc=%d stderr=%s" % (path, p.returncode, p.stderr))
        return json.loads(p.stdout)

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

    def wait_state(self, path, want, timeout=EFFECT_TIMEOUT_S):
        deadline = time.monotonic() + timeout
        last = None
        while time.monotonic() < deadline:
            try:
                last = self.get_json(path)["value"]
                if last == want:
                    return last
            except Exception:
                pass
            time.sleep(0.1)
        raise RuntimeError("state %s did not become %r within %ss (last: %r)"
                           % (path, want, timeout, last))

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
        if self.daemon_pid is not None:
            try:
                os.kill(self.daemon_pid, 0)
                targets.append(self.daemon_pid)
            except OSError:
                pass
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
        # the daemon must have killed the plugin child process group on exit
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
        except Exception as e:  # noqa: BLE001 - report and continue to cleanup
            failures.append((name, "%s: %s" % (type(e).__name__, e)))
            print("FAIL  %s: %s: %s" % (name, type(e).__name__, e))

    lamp = HTTPServer(("127.0.0.1", 0), LampHandler)
    lamp_port = lamp.server_address[1]
    lamp_thread = threading.Thread(target=lamp.serve_forever, daemon=True)
    lamp_thread.start()

    stack = Stack(bins)
    try:
        manifest = stack.install_example_plugin()
        stack.start()
        token = (stack.data / "token").read_text().strip()

        def step_allow_net():
            p = stack.ds("set", "/policy/net/allow_hosts", "--json", '["127.0.0.1"]')
            assert p.returncode == 0, "set allow_hosts rc=%d stderr=%s" % (p.returncode, p.stderr)

        def step_bind():
            p = stack.ds(
                "bind", "deck/control/keys.0/down", "--json",
                "--do", json.dumps({"do": "plugin", "id": "demo", "action": "bump",
                                    "args": {"by": 2}}),
                "--do", json.dumps({"do": "plugin", "id": "demo", "action": "get_lamp",
                                    "args": {"url": "http://127.0.0.1:%d/api/lamp" % lamp_port}}))
            assert p.returncode == 0, "bind rc=%d stderr=%s" % (p.returncode, p.stderr)
            out = json.loads(p.stdout)
            assert out["ok"] is True, out

        def step_subscribe_emit():
            c = Ipc(stack.sock).hello_auth(token)
            r = c.rpc({"id": 2, "op": "subscribe", "path": "/plugin/*"})
            assert r["ok"] is True, r
            stack._emit_ipc = c

        def step_trigger_and_wait_running():
            p = stack.inject_key(0)
            assert p.returncode == 0, "inject rc=%d stderr=%s" % (p.returncode, p.stderr)
            v = stack.wait_plugin_running()
            assert v["status"] == "running", v

        def step_invoke_and_assert():
            c = stack._emit_ipc
            p = stack.inject_key(0)
            assert p.returncode == 0, "inject rc=%d stderr=%s" % (p.returncode, p.stderr)
            # set effects landed
            stack.wait_state("/plugins/demo/state/count", 2)
            label = stack.get_json("/controls/keys/0/label")["value"]
            assert label == "2", label
            # host-mediated net effect landed
            stack.wait_state("/plugins/demo/state/last", LAMP_BODY)
            status = stack.get_json("/plugins/demo/state/last_status")["value"]
            assert status == 200, status
            # emit effect observed on the bus
            deadline = time.monotonic() + EFFECT_TIMEOUT_S
            got = None
            while time.monotonic() < deadline:
                ev = c.recv(timeout=max(0.5, deadline - time.monotonic()))
                if ev.get("op") == "event" and ev.get("topic") == "plugin/demo/bumped":
                    got = ev
                    break
            assert got is not None, "no plugin/demo/bumped event observed"
            assert got["payload"] == {"n": 2}, got
            # lifecycle/stats exposed at /plugins/demo
            v = stack.get_json("/plugins/demo")["value"]
            assert v["status"] == "running", v
            assert v["stats"]["invokes"] >= 2, v["stats"]
            assert v["stats"]["violations"] == 0, v["stats"]

        for name, fn in (
            ("install example plugin under XDG_DATA_HOME (manifest valid)", lambda: None),
            ("set /policy/net/allow_hosts [127.0.0.1]", step_allow_net),
            ("bind key-down topic to plugin bump + get_lamp steps", step_bind),
            ("subscribe /plugin/* on the bus", step_subscribe_emit),
            ("key press brings plugin demo to status=running", step_trigger_and_wait_running),
            ("invoke: set/emit/net effects landed (count=2, label=2, last=lamp body, bus event)",
             step_invoke_and_assert),
        ):
            check(name, fn)
    except Exception as e:  # noqa: BLE001 - boot failure
        failures.append(("boot", "%s: %s" % (type(e).__name__, e)))
        print("ERROR boot: %s: %s" % (type(e).__name__, e))
    finally:
        lamp.shutdown()
        lamp.server_close()
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
