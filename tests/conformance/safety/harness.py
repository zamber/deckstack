"""Shared harness for the DS-029 safety conformance suite.

Black-box only: the real `ds`, `ds-daemon`, `ds-vdev`, `ds-vdevctl` binaries, the raw
NDJSON control socket, the raw ds-vdev device socket (frame codec of
`docs/components/virtual-device.md` §"Socket protocol", pinned in
`crates/ds-device/src/frame.rs`) and the `<socket>.ctl` control socket. No ds-client /
ds-device / ds-vdev library code is imported — everything is observed on the wire.

Two stacks are used:

- `Stack` on a real ds-vdev: vdev's write log (`ds-vdevctl writes`) and module reports
  (`ds-vdevctl reports`) are the device-side oracle (a forbidden report is refused with
  an error frame and recorded `refused`; zero bytes are ever acked for it).
- `Stack` on a `FakeVdev` (a Python stand-in speaking the same frame protocol): records
  every frame the daemon sends, so payload-level claims (handshake first, no forbidden
  pair, no backlight-0) can be asserted on the daemon's own traffic.
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
import threading
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]

BUILD_TIMEOUT_S = 900
OP_TIMEOUT_S = 20
BOOT_TIMEOUT_S = 30
KILL_TIMEOUT_S = 5

BINARIES = ("ds", "ds-daemon", "ds-vdev", "ds-vdevctl")

# --- ds-vdev device-socket frame codec (crates/ds-device/src/frame.rs) ----------
KIND_WRITE = 0x01        # host->device output report (report id 0x02 first)
KIND_FEATURE_SET = 0x02  # host->device feature report (report id 0x03 first)
KIND_FEATURE_GET = 0x03  # host->device feature read
KIND_CLOSE = 0x04
KIND_INPUT = 0x81        # device->host input report
KIND_ACK = 0x82          # device->host write accepted (u32 LE byte count)
KIND_ANSWER = 0x83       # device->host feature answer
KIND_ERROR = 0x84        # device->host refusal/error (UTF-8 message)

# USB product ids of the virtual deck layout (crates/ds-device/src/module.rs).
PID_BASE = 0x01F6
PID_KEYS = 0x01FE
PID_FADERS = 0x0200
PID_KNOBS = 0x0202
PID_ROLLERS = 0x0204
PID_DIAL = 0x0206

# Software-mode handshake reports (ds-protocol enter_software_mode): feature set
# `03 3A 01` on keys, `03 49 01` on dial.
HANDSHAKE_KEYS = bytes([0x03, 0x3A, 0x01])
HANDSHAKE_DIAL = bytes([0x03, 0x49, 0x01])

# Backlight feature-set commands per module family: `03 34 <pct>` keys, `03 44 <pct>` dial.
BACKLIGHT_CMDS = (0x34, 0x44)

# The forbidden (report id, command) pairs of the pinned upstream rev
# mh-protocol src/safety.rs @ 26364e2fdcf86df6c459ea0e940f1dd5ca84cde7. Mirrored on
# purpose (same rule as tests/protocol/safety.rs): the rev is pinned, so any drift here
# is a deliberate upstream change to review, not something to absorb silently.
FORBIDDEN = [
    # M0 family (base, faders, knobs, rollers), output reports
    (0x02, 0x2A),  # update firmware
    (0x02, 0x2B),  # set serial number
    (0x02, 0x31),  # save factory data
    (0x02, 0x49),  # test firmware recovery
    (0x02, 0x4A),  # test factory reset
    (0x02, 0x2F),  # recalibrate the base's tilt sensor
    # Keys (M4315)
    (0x02, 0x34),  # update firmware
    (0x03, 0x35),  # set serial number
    (0x03, 0x39),  # test firmware recovery
    (0x03, 0x3C),  # save factory data
    (0x03, 0x3D),  # test factory reset
    (0x03, 0x3E),  # test animation recovery
    (0x02, 0x38),  # save screen picture to flash
    # Dial (M131E)
    (0x02, 0x44),  # update firmware
    (0x03, 0x45),  # set serial number
    (0x03, 0x48),  # test firmware recovery
    (0x03, 0x4B),  # save factory data
    (0x03, 0x4C),  # test factory reset
    (0x03, 0x4D),  # test animation recovery
    (0x02, 0x47),  # save screen picture to flash
]
FORBIDDEN_SET = set(FORBIDDEN)

# Candidate "raw escape hatch" names for the CLI/IPC enumeration (case 4). Any verb or
# op matching this pattern (or appearing outside the known-safe sets) fails the suite.
RAWISH = re.compile(
    r"raw|hid|write|send|report|byte|hex|inject|feature|firmware|serial|factory|"
    r"reset|flash|dev|debug|exec|cmd|command",
    re.IGNORECASE,
)
# ds-vdevctl commands that exist today and are NOT host->device raw writes ("writes" is
# the read-only write LOG, "inject" broadcasts device->host INPUT frames only). DS-112
# adds the placement controls: "slots" (read-only), "move" (changes the virtual module's
# slot string, never host->device HID bytes), "orientation" (broadcasts a device->host
# orientation INPUT frame), "replug" (closes host connections so they reconnect).
VDEVCTL_SAFE = {"ping", "frames", "reports", "writes", "throttle", "inject",
               "slots", "move", "orientation", "replug"}

# The request ops schemas/ipc-message.json declares (walked at test time from the
# daemon's own `ds schema ipc`, so the assertion tracks the schema, not this list).
EXPECTED_IPC_OPS = {
    "auth", "bind", "daemon", "emit", "get", "hello", "ping", "plugin", "schema",
    "set", "stream-open", "stream-stats", "stream-stop", "subscribe", "token",
    "unbind", "unsubscribe",
}
EXPECTED_SET_OPS = {"set", "delete", "merge"}

# The phase-1 `ds` verbs (bins/ds/src/cli.rs VERBS) — the complete CLI surface.
EXPECTED_DS_VERBS = {
    "get", "set", "watch", "schema", "describe", "daemon", "status", "bind", "unbind",
}

WRITE_LINE_RE = re.compile(
    r"^#(\d+) pid=(0x[0-9a-fA-F]+) kind=(0x[0-9a-fA-F]+) len=(\d+) at_ms=(\d+)( refused)?$"
)
REPORT_LINE_RE = re.compile(
    r"^(?P<kind>\S+) pid=(0x[0-9a-fA-F]+).*software_mode=(?P<mode>\w+) "
    r"backlight=(?P<bl>\d+)"
)


def log(msg):
    if "--verbose" in sys.argv:
        print(msg, file=sys.stderr)


def bin_dir():
    env = os.environ.get("DS_BIN_DIR")
    if env:
        return Path(env)
    return ROOT / "target" / "debug"


def ensure_binaries():
    """Build the four phase-1 binaries if any is missing (bounded)."""
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
         "--bin", "ds-vdevctl"],
        cwd=str(ROOT), timeout=BUILD_TIMEOUT_S, check=True,
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    missing = [b for b in BINARIES if not (d / b).is_file()]
    if missing:
        raise RuntimeError("cargo build finished but still missing: %s" % ", ".join(missing))
    return d


def cargo_available():
    return shutil.which("cargo") is not None


def run_cargo_test(args, timeout_s=600, extra_env=None):
    """Run one bounded `cargo test` invocation at the workspace root; returns the
    CompletedProcess (stdout+stderr merged for easy parsing)."""
    env = dict(os.environ)
    env["CARGO_TERM_COLOR"] = "never"
    if extra_env:
        env.update(extra_env)
    return subprocess.run(
        ["cargo", "test"] + args,
        cwd=str(ROOT), timeout=timeout_s, env=env, check=False,
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
    )


# --- raw device-socket frame codec ---------------------------------------------

def encode_frame(kind, pid, payload):
    body = bytes([kind]) + int(pid).to_bytes(2, "little") + bytes(payload)
    return len(body).to_bytes(4, "little") + body


class VdevClient:
    """A raw host on the ds-vdev device socket (exactly what ds-device's virtual
    transport does): send frames, read reply frames."""

    def __init__(self, sock_path, timeout=OP_TIMEOUT_S):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(timeout)
        self.sock.connect(str(sock_path))
        self.buf = b""

    def send(self, kind, pid, payload):
        self.sock.sendall(encode_frame(kind, pid, payload))

    def _read_exact(self, n):
        while len(self.buf) < n:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise ConnectionError("device socket closed by vdev")
            self.buf += chunk
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def recv(self, timeout=None):
        """One reply frame -> (kind, pid, payload), or None on timeout."""
        if timeout is not None:
            self.sock.settimeout(timeout)
        try:
            header = self._read_exact(4)
        except socket.timeout:
            return None
        body_len = int.from_bytes(header, "little")
        body = self._read_exact(body_len)
        return body[0], int.from_bytes(body[1:3], "little"), body[3:]

    def rpc(self, kind, pid, payload, timeout=OP_TIMEOUT_S):
        self.send(kind, pid, payload)
        return self.recv(timeout)

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


def ctl_request(vdev_sock, line, timeout=OP_TIMEOUT_S):
    """One request on the `<socket>.ctl` line protocol: `OK` + body lines + `.`
    (or `ERR <msg>`). Returns the body lines."""
    path = str(vdev_sock) + ".ctl"
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(timeout)
    try:
        s.connect(path)
        s.sendall(line.encode() + b"\n")
        buf = b""
        while b"\n.\n" not in buf:
            chunk = s.recv(65536)
            if not chunk:
                break
            buf += chunk
    finally:
        s.close()
    text = buf.decode()
    if text.startswith("ERR"):
        raise AssertionError("vdev ctl %r -> %s" % (line, text.strip()))
    body = text.split("\n", 1)[1] if "\n" in text else ""
    return [l for l in body.splitlines() if l != "." and l.strip()]


def parse_writes(lines):
    out = []
    for l in lines:
        m = WRITE_LINE_RE.match(l.strip())
        if m:
            out.append({
                "seq": int(m.group(1)), "pid": int(m.group(2), 16),
                "kind": int(m.group(3), 16), "len": int(m.group(4)),
                "at_ms": int(m.group(5)), "refused": bool(m.group(6)),
            })
    return out


def parse_reports(lines):
    out = []
    for l in lines:
        m = REPORT_LINE_RE.match(l.strip())
        if m:
            out.append({"kind": m.group("kind"),
                        "mode": m.group("mode").lower() == "true",
                        "backlight": int(m.group("bl"))})
    return out


class FakeVdev:
    """A Python stand-in for ds-vdev on the device socket: records every frame the
    daemon sends (with monotonic index) and answers like the real hub (ACK for
    writes/feature-sets, ANSWER for feature-gets)."""

    def __init__(self, path):
        self.path = Path(path)
        for p in (self.path, Path(str(self.path) + ".ctl")):
            if p.exists():
                p.unlink()
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(str(self.path))
        self.listener.listen(8)
        self.listener.settimeout(0.2)
        self.frames = []  # (index, kind, pid, payload)
        self.lock = threading.Lock()
        self.stop_flag = False
        self.thread = threading.Thread(target=self._serve, name="fake-vdev", daemon=True)
        self.thread.start()

    def _serve(self):
        conns = {}  # socket -> bytearray buffer
        while not self.stop_flag:
            try:
                c, _ = self.listener.accept()
                conns[c] = bytearray()
            except socket.timeout:
                pass
            except OSError:
                break
            for c in list(conns):
                buf = conns[c]
                try:
                    c.settimeout(0.05)
                    chunk = c.recv(65536)
                except socket.timeout:
                    continue
                except OSError:
                    conns.pop(c, None)
                    continue
                if not chunk:
                    conns.pop(c, None)
                    try:
                        c.close()
                    except OSError:
                        pass
                    continue
                buf += chunk
                while len(buf) >= 4:
                    body_len = int.from_bytes(buf[:4], "little")
                    if body_len < 3 or body_len > 4096 or len(buf) < 4 + body_len:
                        break
                    body = bytes(buf[4:4 + body_len])
                    del buf[:4 + body_len]
                    kind, pid = body[0], int.from_bytes(body[1:3], "little")
                    payload = body[3:]
                    with self.lock:
                        self.frames.append((len(self.frames), kind, pid, payload))
                    try:
                        if kind == KIND_CLOSE:
                            c.sendall(encode_frame(KIND_ACK, pid, b""))
                            raise OSError("close")
                        if kind in (KIND_WRITE, KIND_FEATURE_SET):
                            c.sendall(encode_frame(
                                KIND_ACK, pid, len(payload).to_bytes(4, "little")))
                        elif kind == KIND_FEATURE_GET:
                            fid = payload[0] if payload else 0
                            ans = bytes([0x03, fid, 4, 0x29, 0x63, 0x1C, 0x5D])
                            ans = ans + b"\x00" * (32 - len(ans))
                            c.sendall(encode_frame(KIND_ANSWER, pid, ans))
                    except OSError:
                        conns.pop(c, None)
                        try:
                            c.close()
                        except OSError:
                            pass
                        break

    def recorded(self):
        with self.lock:
            return list(self.frames)

    def wait_for(self, pred, timeout_s=10.0, what="frame"):
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            for f in self.recorded():
                if pred(f):
                    return f
            time.sleep(0.05)
        raise AssertionError("timed out after %ss waiting for %s" % (timeout_s, what))

    def stop(self):
        self.stop_flag = True
        try:
            self.thread.join(timeout=KILL_TIMEOUT_S)
        except RuntimeError:
            pass
        try:
            self.listener.close()
        except OSError:
            pass
        for p in (self.path, Path(str(self.path) + ".ctl")):
            try:
                p.unlink()
            except OSError:
                pass


class Stack:
    """One booted stack on a fresh temp dir. `device` selects the device end:
    `vdev` (ds-vdev + ds-daemon), `fake` (FakeVdev + ds-daemon) or `bare` (ds-vdev only,
    no daemon — for raw socket probes that must not pollute a daemon's write log).
    Mirrors the boot/teardown discipline of tests/conformance/run.py: every wait has a
    timeout and all processes are always killed."""

    def __init__(self, device="vdev"):
        self.tmp = Path(tempfile.mkdtemp(prefix="ds-safety-"))
        self.device_kind = device
        self.vdev_sock = self.tmp / ("vdev.sock" if device == "bare" else
                                      ("vdev.sock" if device == "vdev" else "fake.sock"))
        self.data = self.tmp / "data"
        self.sock = self.data / "daemon.sock"
        self.token = ""
        self.vdev_proc = None
        self.fake = None
        self.daemon_pid = None
        self.bins = None

    def start(self, bins):
        self.bins = bins
        if self.device_kind == "fake":
            self.fake = FakeVdev(self.vdev_sock)
        else:
            self.vdev_proc = subprocess.Popen(
                [str(bins / "ds-vdev"), "--socket", str(self.vdev_sock)],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                stdin=subprocess.DEVNULL,
            )
        self._wait_for_file(self.vdev_sock, 10, "device socket")
        if self.device_kind == "bare":
            return
        env = dict(os.environ)
        env["DS_DAEMON_BIN"] = str(bins / "ds-daemon")
        p = subprocess.run(
            [str(bins / "ds"), "daemon", "start",
             "--device", "virtual:%s" % self.vdev_sock, "--data-dir", str(self.data)],
            capture_output=True, text=True, timeout=BOOT_TIMEOUT_S, env=env,
        )
        if p.returncode != 0:
            raise RuntimeError(
                "ds daemon start failed rc=%d\nstdout: %s\nstderr: %s"
                % (p.returncode, p.stdout, p.stderr)
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
    def _wait_for_file(path, timeout_s, what):
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

    def ipc(self):
        return Ipc(self.sock)

    def stop(self):
        problems = []
        if self.device_kind != "bare":
            try:
                self.ds("daemon", "stop", timeout=OP_TIMEOUT_S)
            except Exception as e:
                log("daemon stop op failed (continuing with signals): %r" % e)
        targets = []
        if self.vdev_proc is not None and self.vdev_proc.poll() is None:
            targets.append(self.vdev_proc)
        if self.daemon_pid is not None:
            try:
                os.kill(self.daemon_pid, 0)
                targets.append(self.daemon_pid)
            except OSError:
                pass
        for t in targets:
            try:
                if isinstance(t, subprocess.Popen):
                    t.terminate()
                else:
                    os.kill(t, signal.SIGTERM)
            except OSError:
                pass
        deadline = time.monotonic() + KILL_TIMEOUT_S
        while time.monotonic() < deadline and any(
            (t.poll() is None) if isinstance(t, subprocess.Popen)
            else _pid_alive(t) for t in targets
        ):
            time.sleep(0.05)
        for t in targets:
            try:
                if isinstance(t, subprocess.Popen):
                    if t.poll() is None:
                        t.kill()
                else:
                    if _pid_alive(t):
                        os.kill(t, signal.SIGKILL)
            except OSError:
                pass
        if self.fake is not None:
            self.fake.stop()
        try:
            shutil.rmtree(self.tmp, ignore_errors=True)
        except Exception as e:
            problems.append("temp dir removal failed: %r" % e)
        if self.tmp.exists():
            problems.append("temp dir still exists: %s" % self.tmp)
        return problems


def _pid_alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


class Ipc:
    """Raw NDJSON control-socket client (black box)."""

    def __init__(self, sock_path, timeout=OP_TIMEOUT_S):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(timeout)
        self.sock.connect(str(sock_path))
        self.f = self.sock.makefile("rw", encoding="utf-8", newline="\n")

    def send(self, obj):
        self.f.write(json.dumps(obj) + "\n")
        self.f.flush()

    def recv(self):
        line = self.f.readline()
        if not line:
            raise AssertionError("connection closed by daemon while waiting for a reply")
        return json.loads(line)

    def rpc(self, obj):
        self.send(obj)
        return self.recv()

    def hello_auth(self, token):
        r = self.rpc({"id": 0, "op": "hello", "protocol": 1, "client": "safety/1"})
        assert r["ok"], r
        r = self.rpc({"id": 1, "op": "auth", "token": token})
        assert r["ok"], r

    def close(self):
        try:
            self.f.close()
        finally:
            self.sock.close()
