#!/usr/bin/env python3
"""Shared harness for the DS-034 streaming conformance suite (tests/conformance/streaming/).

Black-box only: the real `ds`, `ds-daemon`, `ds-vdev`, `ds-vdevctl` binaries, the raw
NDJSON control socket, the ds-vdev device socket (frame codec of
`crates/ds-device/src/frame.rs`, pinned in tests/conformance/safety/harness.py) and
`ds-vdevctl frames/writes` (the reassembly oracle).

The phase-1 daemon serves the control socket but does not bind `<data>/stream.sock` yet
(the DS-031 service is exercised by its own tests), so this harness provides the
daemon side of the stream plane itself — exactly what `bins/ds/src/stream/it.rs` does
in-process in Rust: a `StreamBridge` listening on `<data>/stream.sock` speaking the
`DSctl\\n` control lines and the binary DSF1 frames (`docs/contracts/ipc.md` §2,
`schemas/stream-frame.json`), with the same gates the real DS-013 server has
(session, credits, 20 ms per-target rate limit, 16 MB wire cap, CRC32, schema bounds
on `frames`/`fps`), answering `stream-ack`/`stream-nack`, and forwarding every accepted
frame to the ds-vdev as chunked image reports so `ds-vdevctl frames <target>` reassembles
them (raw mode) — or through the real daemon's encoder via `ds set … --file` (encode
mode), which is how the "final frame == encoder output of the last frame" case is
observed against the golden vectors.

Images are generated deterministically in-process (PNG via zlib, GIF via a tiny
literal-only LZW writer) — no binary fixtures.
"""

import json
import os
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]

BUILD_TIMEOUT_S = 900
OP_TIMEOUT_S = 30
BOOT_TIMEOUT_S = 30
KILL_TIMEOUT_S = 5

BINARIES = ("ds", "ds-daemon", "ds-vdev", "ds-vdevctl")

# ds-vdev device-socket frame codec (crates/ds-device/src/frame.rs)
KIND_WRITE = 0x01  # host->device output report (report id 0x02 first)
KIND_ACK = 0x82    # device->host write accepted (u32 LE byte count)

PID_KEYS = 0x01FE
PID_DIAL = 0x0206

# image chunk reports (docs/contracts/streaming.md §2, crates/ds-daemon/src/frames/chunk.rs)
REPORT_LEN = 320
CHUNK_HEADER_LEN = 8
MAX_CHUNK_PAYLOAD = REPORT_LEN - CHUNK_HEADER_LEN  # 312

# stream-plane caps (docs/contracts/streaming.md §2/§5, schemas/stream-frame.json)
MAX_PAYLOAD_BYTES = 16 * 1024 * 1024
MAX_ANIMATION_FRAMES = 300
MAX_FPS = 30
MIN_FRAME_INTERVAL_S = 0.02
MAX_CREDITS_PER_OPEN = 1024

DSF1_MAGIC = b"DSF1"
DSCTL_MAGIC = b"DSctl\n"

TARGET_RE = __import__("re").compile(r"^(?:keys|dial|key:(?:[0-9]|1[0-4]))$")

# target -> (device pid, image cmd, chunk index)
TARGET_WIRE = {
    "keys": (PID_KEYS, 0x35, 0),
    "dial": (PID_DIAL, 0x45, 0),
}


def target_wire(target):
    """(pid, cmd, index) for a stream target; key:N -> (keys pid, 0x36, N)."""
    if target.startswith("key:"):
        return (PID_KEYS, 0x36, int(target.split(":", 1)[1]))
    return TARGET_WIRE[target]


def control_path_for_image(target):
    """The /controls path whose `set --file` pushes to this stream target."""
    if target == "keys":
        return "/controls/keys/image"
    if target == "dial":
        return "/controls/dial/image"
    return "/controls/keys/%s/image" % target.split(":", 1)[1]


# ---------------------------------------------------------------------------
# deterministic image builders (no binary fixtures)
# ---------------------------------------------------------------------------


def make_png(rgb, w, h):
    """A minimal 8-bit RGB PNG (filter 0 per row), built with zlib only."""
    assert len(rgb) == w * h * 3
    raw = b"".join(b"\x00" + bytes(rgb[r * w * 3:(r + 1) * w * 3]) for r in range(h))

    def chunk(ty, data):
        return struct.pack(">I", len(data)) + ty + data + struct.pack(">I", zlib.crc32(ty + data) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)  # 8-bit truecolor
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b"")


def solid_rgb(c, w, h):
    return bytes(c) * (w * h)


def make_gif(colors, delays, w, h, loop=True):
    """A tiny animated GIF89a: global palette, NETSCAPE2.0 loop ext, one graphic control
    ext + image descriptor per frame. LZW data is literal-only: a clear code before every
    literal keeps the code size fixed at min_code_size+1, so no table-growth bookkeeping
    is needed (the decoder accepts a clear at any time). `delays` are in 10 ms units."""
    e = 2
    while e < max(2, len(colors)):
        e *= 2
    palette = b"".join(bytes(c) for c in colors) + b"\x00" * (e * 3 - len(colors) * 3)
    mcs = max(2, e.bit_length() - 1)
    clear = 1 << mcs
    eoi = clear + 1
    code_size = mcs + 1
    out = bytearray(b"GIF89a")
    out += struct.pack("<HH", w, h)
    out.append(0x80 | (e.bit_length() - 2))  # GCT present, table size 2^(n+1)
    out += b"\x00\x00"  # background index, aspect
    out += palette
    if loop:
        out += b"\x21\xff\x0bNETSCAPE2.0\x03\x01" + struct.pack("<H", 0) + b"\x00"  # loop forever
    for i, delay in enumerate(delays):
        idx = i % len(colors)
        out += b"\x21\xf9\x04" + b"\x00" + struct.pack("<H", delay) + b"\x00" + b"\x00"  # GCE
        out += b"\x2c" + struct.pack("<HHHH", 0, 0, w, h) + b"\x00"  # image descriptor
        codes = []
        for _ in range(w * h):
            codes.append(clear)
            codes.append(idx)
        codes.append(eoi)
        bits = 0
        nbits = 0
        data = bytearray()
        for c in codes:
            bits |= c << nbits
            nbits += code_size
            while nbits >= 8:
                data.append(bits & 0xFF)
                bits >>= 8
                nbits -= 8
        if nbits:
            data.append(bits & 0xFF)
        out.append(mcs)
        for i in range(0, len(data), 255):
            blk = data[i:i + 255]
            out.append(len(blk))
            out += blk
        out.append(0)
    out += b"\x3b"
    return bytes(out)


# ---------------------------------------------------------------------------
# the stream-plane bridge (test-side DSF1/DSctl server on <data>/stream.sock)
# ---------------------------------------------------------------------------


class StreamBridge:
    """Speaks DSctl/DSF1 on `<data>/stream.sock` with the real server's gates and forwards
    accepted frames to ds-vdev (raw chunked mode) or through the daemon's encoder
    (`ds set … --file`, encode mode). Records every accepted frame for assertions."""

    def __init__(self, sock_path, ds_runner, vdev_sock, scratch):
        self.sock_path = Path(sock_path)
        self.ds = ds_runner  # callable(args) -> CompletedProcess
        self.vdev_sock = Path(vdev_sock)
        self.scratch = Path(scratch)  # allowed fs root: encode-mode temp files live here
        self.mode = "raw"  # or "encode"
        self.lock = threading.RLock()  # _handle_frame nacks while holding the lock
        self.frames = []  # accepted frames: dicts with meta + arrival time
        self.nacks = []
        self.sessions = {}  # (conn_id, target) -> {"credits": int, "last_at": float}
        self._srv = None
        self._thread = None
        self._stop = threading.Event()
        self._next_conn = 0

    # -- lifecycle -----------------------------------------------------------

    def start(self):
        if self.sock_path.exists():
            self.sock_path.unlink()
        srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        srv.bind(str(self.sock_path))
        os.chmod(self.sock_path, 0o600)
        srv.listen(16)
        srv.settimeout(0.5)
        self._srv = srv
        self._thread = threading.Thread(target=self._accept_loop, daemon=True)
        self._thread.start()

    def stop(self):
        self._stop.set()
        if self._srv is not None:
            try:
                self._srv.close()
            except OSError:
                pass
        if self._thread is not None:
            self._thread.join(timeout=2)
        if self.sock_path.exists():
            self.sock_path.unlink()

    def reset_records(self):
        with self.lock:
            self.frames = []
            self.nacks = []

    def accepted(self):
        with self.lock:
            return list(self.frames)

    # -- accept loop ---------------------------------------------------------

    def _accept_loop(self):
        while not self._stop.is_set():
            try:
                conn, _ = self._srv.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            with self.lock:
                self._next_conn += 1
                cid = self._next_conn
            threading.Thread(target=self._serve, args=(cid, conn), daemon=True).start()

    def _send_dsctl(self, conn, obj):
        conn.sendall(DSCTL_MAGIC + json.dumps(obj).encode() + b"\n")

    def _nack(self, conn, target, seq, code, message):
        with self.lock:
            self.nacks.append({"target": target, "seq": seq, "code": code, "message": message})
        try:
            self._send_dsctl(conn, {"op": "stream-nack", "target": target, "seq": seq,
                                     "error": {"code": code, "message": message}})
        except OSError:
            pass

    def _serve(self, cid, conn):
        conn.settimeout(OP_TIMEOUT_S)
        f = conn.makefile("rb")
        try:
            while not self._stop.is_set():
                magic = f.read(4)
                if not magic:
                    return
                if magic == DSF1_MAGIC:
                    if not self._handle_frame(cid, conn, f):
                        return  # fatal: connection closed after the nack
                elif magic == b"DSct":
                    rest = f.read(2)
                    if rest != b"l\n":
                        return
                    line = f.readline()
                    if not line:
                        return
                    self._handle_dsctl(cid, conn, line)
                else:
                    return
        except (OSError, ValueError):
            return
        except Exception:
            import traceback
            traceback.print_exc()
            return
        finally:
            try:
                conn.close()
            except OSError:
                pass

    def _handle_dsctl(self, cid, conn, line):
        try:
            v = json.loads(line.decode())
        except ValueError:
            return
        op = v.get("op")
        target = str(v.get("target", ""))
        if op == "stream-open":
            credits = int(v.get("credits", MAX_CREDITS_PER_OPEN))
            credits = max(1, min(MAX_CREDITS_PER_OPEN, credits))
            with self.lock:
                self.sessions[(cid, target)] = {"credits": credits,
                                                "last_at": time.monotonic() - MIN_FRAME_INTERVAL_S}
            self._send_dsctl(conn, {"op": "stream-credit", "target": target, "credits": credits})
        elif op == "stream-stop":
            with self.lock:
                self.sessions.pop((cid, target), None)
            self._send_dsctl(conn, {"op": "stream-credit", "target": target, "credits": 0})

    def _handle_frame(self, cid, conn, f):
        """Returns False when the connection must be closed (fatal framing error)."""
        try:
            (meta_len,) = struct.unpack("<I", f.read(4))
            if meta_len > 64 * 1024:
                self._nack(conn, "", 0, "E_LIMIT", "frame meta exceeds the 64 KiB meta cap")
                return False
            meta = json.loads(f.read(meta_len).decode())
            (payload_len,) = struct.unpack("<Q", f.read(8))
        except (OSError, ValueError, struct.error):
            return False
        target = str(meta.get("target", ""))
        seq = int(meta.get("seq", 0))
        # the 16 MB wire cap is enforced before the payload is read (fatal, like the server)
        if payload_len > MAX_PAYLOAD_BYTES:
            self._nack(conn, target, seq, "E_LIMIT", "frame payload exceeds the 16 MB wire cap")
            return False
        payload = f.read(payload_len)
        if payload is None or len(payload) != payload_len:
            return False
        # meta bounds (schemas/stream-frame.json)
        if not TARGET_RE.match(target):
            self._nack(conn, target, seq, "E_SCHEMA", "unknown stream target")
            return True
        if int(meta.get("frames", 1)) > MAX_ANIMATION_FRAMES:
            self._nack(conn, target, seq, "E_SCHEMA", "frames over the 300 cap")
            return True
        if meta.get("fps") is not None and not (1 <= float(meta["fps"]) <= MAX_FPS):
            self._nack(conn, target, seq, "E_SCHEMA", "fps over the 30 cap")
            return True
        if int(meta.get("crc32", -1)) != (zlib.crc32(payload) & 0xFFFFFFFF):
            self._nack(conn, target, seq, "E_SCHEMA", "payload CRC32 mismatch")
            return True
        with self.lock:
            sess = self.sessions.get((cid, target))
            if sess is None:
                self._nack(conn, target, seq, "E_SCOPE", "no stream-open for target")
                return True
            if sess["credits"] <= 0:
                self._nack(conn, target, seq, "E_LIMIT", "credit window exhausted")
                return True
            now = time.monotonic()
            if now - sess["last_at"] < MIN_FRAME_INTERVAL_S:
                self._nack(conn, target, seq, "E_LIMIT",
                           "rate limit: min frame interval 0.02 s (30 fps)")
                return True
            sess["credits"] -= 1
            sess["last_at"] = now
            record = {
                "target": target,
                "seq": seq,
                "ts_ms": int(meta.get("ts_ms", 0)),
                "format": meta.get("format"),
                "frames": meta.get("frames"),
                "fps": meta.get("fps"),
                "loop": meta.get("loop"),
                "w": meta.get("w"),
                "h": meta.get("h"),
                "len": payload_len,
                "sha256": __import__("hashlib").sha256(payload).hexdigest(),
                "at": now,
            }
            self.frames.append(record)
        self._send_dsctl(conn, {"op": "stream-ack", "target": target, "seq": seq})
        try:
            self._forward(target, payload)
        except Exception:
            pass  # forwarding is best-effort; the ack already happened
        return True

    # -- forwarding to the device --------------------------------------------

    def _forward(self, target, payload):
        if self.mode == "encode":
            # through the real daemon's encoder (state image path) — the scratch dir is
            # registered in /policy/fs/roots at stack start
            fd, path = tempfile.mkstemp(prefix="ds-streaming-", suffix=".img", dir=str(self.scratch))
            try:
                with os.fdopen(fd, "wb") as fh:
                    fh.write(payload)
                p = self.ds(["set", control_path_for_image(target), "--file", path])
                if p.returncode != 0:
                    raise RuntimeError("encode-mode set failed: %s" % p.stderr)
            finally:
                os.unlink(path)
            return
        pid, cmd, index = target_wire(target)
        link = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        link.settimeout(OP_TIMEOUT_S)
        try:
            link.connect(str(self.vdev_sock))
            for i in range(0, max(1, len(payload)), MAX_CHUNK_PAYLOAD):
                chunk = payload[i:i + MAX_CHUNK_PAYLOAD]
                ci = i // MAX_CHUNK_PAYLOAD
                last = 1 if i + MAX_CHUNK_PAYLOAD >= len(payload) else 0
                report = bytearray(REPORT_LEN)
                report[0] = 0x02
                report[1] = cmd
                report[2:4] = struct.pack("<H", ci)
                report[4:6] = struct.pack("<H", len(chunk))
                report[6] = index
                report[7] = last
                report[8:8 + len(chunk)] = chunk
                body = bytes([KIND_WRITE]) + struct.pack("<H", pid) + bytes(report)
                link.sendall(struct.pack("<I", len(body)) + body)
                reply = self._read_vdev_frame(link)
                if reply is None or reply[0] != KIND_ACK:
                    raise RuntimeError("vdev did not ack an image chunk: %r" % (reply,))
        finally:
            link.close()

    @staticmethod
    def _read_vdev_frame(link):
        hdr = b""
        while len(hdr) < 4:
            b = link.recv(4 - len(hdr))
            if not b:
                return None
            hdr += b
        (n,) = struct.unpack("<I", hdr)
        body = b""
        while len(body) < n:
            b = link.recv(n - len(body))
            if not b:
                return None
            body += b
        return body[0], body[1:]


# ---------------------------------------------------------------------------
# the stack: ds-vdev + ds-daemon + stream bridge on a fresh temp dir
# ---------------------------------------------------------------------------


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
    subprocess.run([cargo, "build", "--bin", "ds", "--bin", "ds-daemon", "--bin", "ds-vdev",
                    "--bin", "ds-vdevctl"], cwd=str(ROOT), timeout=BUILD_TIMEOUT_S, check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    missing = [b for b in BINARIES if not (d / b).is_file()]
    if missing:
        raise RuntimeError("cargo build finished but still missing: %s" % ", ".join(missing))
    return d


class Stack:
    """One booted ds-vdev + ds-daemon pair plus the stream bridge on a fresh temp dir."""

    def __init__(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="ds-streaming-"))
        self.vdev_sock = self.tmp / "vdev.sock"
        self.data = self.tmp / "data"
        self.sock = self.data / "daemon.sock"
        self.stream_sock = self.data / "stream.sock"
        self.token = ""
        self.vdev = None
        self.daemon_pid = None
        self.bins = None
        self.bridge = None
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
            raise RuntimeError("ds daemon start failed rc=%d\n%s\n%s\n%s"
                               % (p.returncode, p.stdout, p.stderr, self._read(self.daemon_log)))
        self._wait_for_file(self.sock, 10, "daemon control socket")
        self.token = (self.data / "token").read_text().strip()
        # allow the stack temp dir as an fs root (the encode-mode bridge writes its
        # scratch frames there), like tests/e2e/run.py does for the repo root
        p = self.ds(["set", "/policy/fs/roots", "--json", json.dumps([str(self.tmp), "/tmp"])])
        if p.returncode != 0:
            raise RuntimeError("cannot set /policy/fs/roots: %s" % p.stderr)
        pid_file = self.data / "daemon.pid"
        if pid_file.is_file():
            try:
                self.daemon_pid = int(pid_file.read_text().strip())
            except ValueError:
                self.daemon_pid = None
        self.bridge = StreamBridge(self.stream_sock, self.ds, self.vdev_sock, self.tmp)
        self.bridge.start()

    def ds(self, args, timeout=OP_TIMEOUT_S):
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

    def rev(self):
        p = self.ds(["get", "/meta", "--json"])
        assert p.returncode == 0, p.stderr
        return json.loads(p.stdout)["value"]["rev"]

    def vdev_frame_sha(self, target):
        """The sha256 of the latest reassembled frame on the vdev for `target` (panel|key:N|dial)."""
        p = self.vdevctl("frames", target)
        if p.returncode != 0:
            return None
        # "<target> <sha256> <bytes>B seq=<n>"
        return p.stdout.split()[1] if p.stdout.strip() else None

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
        raise RuntimeError("timed out after %ss waiting for %s" % (timeout_s, what))

    def stop(self):
        problems = []
        try:
            self.ds(["daemon", "stop"], timeout=OP_TIMEOUT_S)
        except Exception:
            pass
        if self.bridge is not None:
            self.bridge.stop()
        for name, proc, pid in (("daemon", None, self.daemon_pid),
                                ("vdev", self.vdev, self.vdev.pid if self.vdev else None)):
            targets = []
            if proc is not None and proc.poll() is None:
                targets.append(proc)
            if pid is not None:
                try:
                    os.kill(pid, 0)
                    targets.append(pid)
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
                    (t.poll() is None if isinstance(t, subprocess.Popen) else True)
                    for t in targets):
                time.sleep(0.05)
            for t in targets:
                try:
                    if isinstance(t, subprocess.Popen):
                        if t.poll() is None:
                            t.kill()
                    else:
                        os.kill(t, signal.SIGKILL)
                except OSError:
                    pass
        if self.sock.exists():
            problems.append("control socket still present")
        try:
            shutil.rmtree(self.tmp, ignore_errors=True)
        except Exception as e:
            problems.append("temp dir removal failed: %r" % e)
        try:
            self.vdev_log.close()
        except Exception:
            pass
        return problems
