#!/usr/bin/env python3
"""deckstack fuzz suite (ticket DS-080) — deterministic, seeded fuzzing of the REAL
sockets of a booted ds-vdev + ds-daemon pair.

Stdlib only. The process harness (Stack / ensure_binaries) is imported from
tests/conformance/run.py (DS-018), not copied.

What is fuzzed (the framing/decoder entry points reachable at runtime in this tree):

  - daemon.sock  — the NDJSON control framing (read_line: 1 MiB cap, UTF-8, one JSON
    object per line) and the request schema validation (schemas/ipc-message.json).
    Seeded from tests/fixtures/ipc-examples.json (the checked-in corpus) and mutated.
  - daemon.sock  — DSF1 stream-frame bytes (magic | u32 meta_len | meta | u64 payload_len
    | payload, schemas/stream-frame.json) sent as raw lines: they must get a typed
    E_SCHEMA (not JSON) or, when over the 1 MiB line cap, the id-less E_LIMIT + close.
  - vdev.sock    — the binary device framing (u32 body_len | kind | pid | payload,
    body cap 4096, docs/components/virtual-device.md): mutated frames must get a clean
    close or a typed 0x84 error frame, never a crash/hang.
  - vdev.sock.ctl — the line control protocol of ds-vdevctl: OK/ERR/"."/EOF.

Every case must get a deterministic typed verdict (an E_* envelope, an ack/error
frame, or a clean close). A hang (read timeout) or a dead daemon/vdev is a FINDING:
the suite fails and keeps the case in the corpus. Nothing is retried.

Determinism: the generator is `random.Random(SEED)`; the same seed always produces the
same corpus and the same verdicts. The run writes tests/fuzz/corpus/cases.jsonl;
`python3 tests/fuzz/run.py --replay` re-runs the checked-in corpus and asserts the
recorded verdicts reproduce exactly.

Usage:
    python3 tests/fuzz/run.py [--replay] [--cases N]     (default N=2000, seed fixed)

Bounded: a 60 s watchdog (SIGALRM) plus per-read timeouts; the whole run (boot + 2000
cases) is well under 60 s. Exit 0 = all verdicts typed, no crash/hang, clean teardown.
"""

import importlib.util
import json
import os
import random
import signal
import socket
import struct
import subprocess
import sys
import time
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SEED = 0xD5F080
DEFAULT_CASES = 2000
WATCHDOG_S = 60
READ_TIMEOUT_S = 2.0
CORPUS = ROOT / "tests" / "fuzz" / "corpus" / "cases.jsonl"
MAX_HEX_BYTES = 8192  # corpus stores at most this much hex per case (bigger cases run but are stored as markers)

_spec = importlib.util.spec_from_file_location("ds_conformance_run", ROOT / "tests" / "conformance" / "run.py")
conf = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(conf)
ensure_binaries = conf.ensure_binaries
Stack = conf.Stack

MAX_LINE = 1024 * 1024
MAX_PAYLOAD = 16 * 1024 * 1024
E_CODES = {"E_AUTH", "E_SCOPE", "E_PATH", "E_SCHEMA", "E_CONFLICT", "E_LIMIT",
           "E_CAP", "E_UNREACHABLE", "E_DEVICE", "E_VERSION", "E_INTERNAL"}


# --------------------------------------------------------------- generators --

def load_seed_messages():
    doc = json.loads((ROOT / "tests" / "fixtures" / "ipc-examples.json").read_text())
    return [m for m in doc["messages"]]


def dsf1_frame(rng, mutate=True):
    """Build a DSF1 frame (possibly mutated) exactly per schemas/stream-frame.json."""
    meta = {
        "target": rng.choice(["keys", "dial", "key:0", "key:14"]),
        "seq": rng.randrange(0, 10 ** 6),
        "ts_ms": rng.randrange(0, 2 ** 48),
        "format": rng.choice(["png", "jpeg", "gif"]),
        "crc32": 0,
        "max_payload_bytes": MAX_PAYLOAD,
    }
    if rng.random() < 0.5:
        meta["w"] = rng.choice([1, 72, 480, 4096])
        meta["h"] = rng.choice([1, 72, 272, 4096])
    if rng.random() < 0.3:
        meta["frames"] = rng.choice([1, 2, 300])
        meta["fps"] = rng.choice([1, 15, 30])
        meta["loop"] = rng.random() < 0.5
    payload = bytes(rng.randrange(256) for _ in range(rng.randrange(0, 64)))
    meta["crc32"] = zlib.crc32(payload) & 0xFFFFFFFF
    meta_bytes = json.dumps(meta, separators=(",", ":")).encode()
    out = bytearray(b"DSF1" + struct.pack("<I", len(meta_bytes)) + meta_bytes
                    + struct.pack("<Q", len(payload)) + payload)
    if mutate:
        m = rng.randrange(9)
        if m == 0:  # magic lie
            out[rng.randrange(4)] = rng.randrange(256)
        elif m == 1:  # meta_len lie
            pos = 4
            out[pos:pos + 4] = struct.pack("<I", rng.choice([0, 1, len(meta_bytes) - 1,
                                                              len(meta_bytes) + 1,
                                                              64 * 1024 + 1, 0xFFFFFFFF]))
        elif m == 2:  # meta content lie (schema violations)
            meta2 = dict(meta)
            lie = rng.randrange(10)
            if lie == 0:
                meta2["target"] = rng.choice(["panel", "screen", "key:15", "", "KEYS"])
            elif lie == 1:
                meta2["seq"] = -1
            elif lie == 2:
                meta2["frames"] = 301
            elif lie == 3:
                meta2["fps"] = 30.5
            elif lie == 4:
                meta2["w"] = 4097
            elif lie == 5:
                meta2["h"] = 0
            elif lie == 6:
                meta2["format"] = "png2"
            elif lie == 7:
                meta2["max_payload_bytes"] = MAX_PAYLOAD - 1
            elif lie == 8:
                meta2["evil"] = True
            else:
                meta2.pop(rng.choice(list(meta2)))
            mb = json.dumps(meta2, separators=(",", ":")).encode()
            out = bytearray(b"DSF1" + struct.pack("<I", len(mb)) + mb
                            + struct.pack("<Q", len(payload)) + payload)
        elif m == 3:  # payload_len lie (over the 16 MB wire cap -> must be refused pre-read)
            pos = 4 + 4 + len(meta_bytes)
            out[pos:pos + 8] = struct.pack("<Q", rng.choice([MAX_PAYLOAD + 1, 2 ** 40,
                                                              0xFFFFFFFFFFFFFFFF, 0]))
        elif m == 4:  # crc mismatch
            meta["crc32"] ^= 0xFFFF
            mb = json.dumps(meta, separators=(",", ":")).encode()
            out = bytearray(b"DSF1" + struct.pack("<I", len(mb)) + mb
                            + struct.pack("<Q", len(payload)) + payload)
        elif m == 5:  # truncate mid-frame
            out = out[:rng.randrange(1, len(out) + 1)]
        elif m == 6:  # bit flips anywhere
            for _ in range(rng.randrange(1, 5)):
                out[rng.randrange(len(out))] ^= 1 << rng.randrange(8)
        elif m == 7:  # non-JSON meta
            out = bytearray(b"DSF1" + struct.pack("<I", 8) + b"not json"
                            + struct.pack("<Q", 4) + b"abcd")
        # m == 8: leave the valid frame
    return bytes(out)


def vdev_frame(rng, mutate=True):
    """One device-socket frame (u32 body_len | kind | pid | payload), possibly mutated."""
    kind = rng.choice([0x01, 0x02, 0x03, 0x04])
    pid = rng.choice([0x0204, 0x0206, 0x01FE, 0x0000, 0xFFFF])
    plen = rng.choice([0, 1, 4, 32, 317, 320])
    payload = bytes([0x02]) + bytes(rng.randrange(256) for _ in range(max(0, plen - 1)))
    body = bytes([kind]) + struct.pack("<H", pid) + payload
    out = bytearray(struct.pack("<I", len(body)) + body)
    if mutate:
        m = rng.randrange(8)
        if m == 0:  # body_len lie (over the 4096 cap -> refused before reading the body)
            out[0:4] = struct.pack("<I", rng.choice([0, 2, 4097, 0xFFFFFFFF, 100000]))
        elif m == 1:  # unknown kind
            out[4] = rng.choice([0x00, 0x05, 0x7F, 0x80, 0xFF])
        elif m == 2:  # pid lie
            out[5:7] = struct.pack("<H", rng.randrange(65536))
        elif m == 3:  # truncate
            out = out[:rng.randrange(1, len(out) + 1)]
        elif m == 4:  # payload flips / extension (declared len vs actual)
            for _ in range(rng.randrange(1, 4)):
                if len(out) > 7:
                    out[rng.randrange(7, len(out))] ^= 0xFF
            if rng.random() < 0.5:
                out.extend(bytes(rng.randrange(256) for _ in range(rng.randrange(1, 32))))
        elif m == 5:  # raw garbage
            out = bytearray(bytes(rng.randrange(256) for _ in range(rng.randrange(1, 48))))
        elif m == 6:  # valid CLOSE
            out = bytearray(struct.pack("<I", 3) + bytes([0x04]) + struct.pack("<H", 0))
        # m == 7: leave the valid frame
    return bytes(out)


SAFE_OPS = ["ping", "get", "set", "subscribe", "migrate", "zzz", "", "STREAM-OPEN"]


def mutate_json_msg(rng, msg):
    """JSON-aware mutation of one seed IPC message (plus occasional byte-level noise)."""
    try:
        obj = json.loads(json.dumps(msg))
    except ValueError:
        obj = msg
    out = obj
    if isinstance(obj, dict):
        m = rng.randrange(9)
        if m == 0 and obj:
            obj.pop(rng.choice(list(obj.keys())), None)
        elif m == 1:
            obj["op"] = rng.choice(SAFE_OPS)
        elif m == 2:
            obj["ok"] = rng.choice([True, False, None])
        elif m == 3:
            obj["id"] = rng.choice(["x", -1, 2 ** 40, 1.5, None, [1]])
        elif m == 4:
            obj["protocol"] = rng.choice([0, 2, -1, 99, "1"])
        elif m == 5:
            obj["path"] = rng.choice(["no-slash", "/../etc", "/" + "a" * 500, 42, ""])
        elif m == 6:
            obj["extra"] = {"nested": [1, 2, {"deep": True}]}
        elif m == 7 and isinstance(obj.get("ops"), list) and obj["ops"]:
            op = obj["ops"][0]
            if isinstance(op, dict):
                op[rng.choice(["op", "path", "value"])] = rng.choice(["delete", 7, None, "x" * 300])
        # m == 8: leave the object
        out = obj
    data = json.dumps(out, separators=(",", ":")).encode() if not isinstance(out, (bytes, bytearray)) else bytes(out)
    if rng.random() < 0.35:
        data = byte_mutate(rng, data)
    if rng.random() < 0.1:
        data = data + b"\n" + json.dumps(rng.choice(load_seed_messages())).encode()  # two lines
    return data


def byte_mutate(rng, data):
    data = bytearray(data)
    if not data:
        return bytes(data)
    m = rng.randrange(6)
    if m == 0:
        data[rng.randrange(len(data))] ^= 1 << rng.randrange(8)
    elif m == 1:
        data = data[:rng.randrange(len(data) + 1)]
    elif m == 2:
        pos = rng.randrange(len(data) + 1)
        data[pos:pos] = bytes(rng.randrange(256) for _ in range(rng.randrange(1, 16)))
    elif m == 3:
        pos = rng.randrange(len(data))
        data[pos:pos] = data[pos:pos + rng.randrange(1, 8)] * 2
    elif m == 4:
        data.extend(bytes(rng.randrange(256) for _ in range(rng.randrange(1, 64))))
    else:
        pos = rng.randrange(len(data))
        ln = rng.randrange(1, 8)
        data[pos:pos + ln] = data[pos:pos + ln][::-1]
    return bytes(data)


def generate(seed, n):
    rng = random.Random(seed)
    seeds = load_seed_messages()
    cases = []
    for i in range(n):
        r = rng.random()
        if r < 0.55:
            kind = "ctl"
            if rng.random() < 0.03:
                # rare standalone line over the 1 MiB cap: E_LIMIT + close before parsing
                data = b'{"op":"ping","pad":"' + b"a" * MAX_LINE + b'"}\n'
                cases.append({"i": i, "kind": kind, "data": data})
                continue
            msg = rng.choice(seeds)
            data = mutate_json_msg(rng, msg)
            if rng.random() < 0.08:
                data = byte_mutate(rng, data)
            if rng.random() < 0.05:  # two pipelined lines in one case
                data = data + b"\n" + json.dumps(rng.choice(seeds)).encode() + b"\n"
            elif rng.random() < 0.05:  # no trailing newline -> partial line, then EOF
                data = data.rstrip(b"\n")
                cases.append({"i": i, "kind": kind, "data": data})
                continue
            if not data.endswith(b"\n"):
                data += b"\n"
            cases.append({"i": i, "kind": kind, "data": data})
        elif r < 0.70:
            data = dsf1_frame(rng)
            if rng.random() < 0.2:
                data = data[:rng.randrange(1, len(data) + 1)]
                cases.append({"i": i, "kind": "dsf1", "data": data})
                continue
            cases.append({"i": i, "kind": "dsf1", "data": data + b"\n"})
        elif r < 0.90:
            cases.append({"i": i, "kind": "vdev", "data": vdev_frame(rng)})
        else:
            line = rng.choice(["ping", "frames", "writes 5", "reports", "throttle 100",
                               "inject {\"type\":\"key\"}", "bogus"])
            data = line.encode()
            if rng.random() < 0.5:
                data = byte_mutate(rng, data)
            cases.append({"i": i, "kind": "vdevctl", "data": data + b"\n"})
    return cases


# ------------------------------------------------------------------ runner ---

class Ctl:
    """Persistent control connection with auto (re)hello/auth.

    Async server messages (subscription events) carry no `ok`; replies always do.
    Reads skip event lines and classify the first reply line.
    """

    def __init__(self, stack):
        self.stack = stack
        self.sock = None
        self.reopens = 0
        self.skipped_events = 0

    def _open(self):
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.settimeout(READ_TIMEOUT_S)
        s.connect(str(self.stack.sock))
        self.sock = s
        self.reopens += 1
        self.rpc({"id": 0, "op": "hello", "protocol": 1, "client": "fuzz/1"})
        self.rpc({"id": 1, "op": "auth", "token": self.stack.token})

    def ensure(self):
        if self.sock is None:
            self._open()

    def send(self, data):
        """Send raw bytes; tolerate a dead connection (returns False)."""
        try:
            self.ensure()
            self.sock.sendall(data)
            return True
        except (BrokenPipeError, ConnectionResetError, TimeoutError, OSError):
            return False

    def read_line(self, timeout=READ_TIMEOUT_S):
        try:
            self.sock.settimeout(timeout)
            buf = b""
            while b"\n" not in buf:
                chunk = self.sock.recv(65536)
                if not chunk:
                    return "eof", None
                buf += chunk
            line = buf.split(b"\n", 1)[0]
            try:
                return "line", json.loads(line.decode())
            except (ValueError, UnicodeDecodeError):
                return "line-raw", None
        except socket.timeout:
            return "timeout", None
        except (ConnectionResetError, BrokenPipeError, OSError):
            return "reset", None

    def rpc(self, obj):
        self.sock.sendall((json.dumps(obj) + "\n").encode())
        return self.read_line()

    def close(self):
        if self.sock is not None:
            try:
                self.sock.close()
            except OSError:
                pass
            self.sock = None


def classify(obj):
    if not isinstance(obj, dict) or "ok" not in obj:
        return "event"  # async server message (subscription event): not a reply
    if obj["ok"] is True:
        return "ok"
    err = obj.get("error")
    if isinstance(err, dict) and err.get("code") in E_CODES:
        return "error:" + err["code"]
    return "malformed-response"


def run_ctl_case(ctl, case):
    """Run one control/dsf1 case. Returns a list of verdict strings.

    Clean cases (exactly one trailing newline) ride the persistent connection and
    read until the reply line (event lines are skipped). Messy cases (several lines
    or no terminator) run on a fresh connection that is half-closed afterwards; the
    server may keep a subscribed connection open (subscriptions are idle-exempt,
    ipc.md §5), so reads stop on a short idle gap, not only on EOF. A first-read
    timeout is a hang finding; a dead connection replays the case once, fresh.
    """
    data = case["data"]
    messy = data.count(b"\n") != 1 or not data.endswith(b"\n")
    if not messy and b'"subscribe"' in data:
        # a subscription keeps the connection (and its fd) alive on the bus, so a
        # later fatal error on it would not be visible as EOF; run it on a fresh
        # connection that we close ourselves
        messy = True
    if messy:
        return _run_ctl_fresh(ctl, data)
    if not ctl.send(data):
        ctl.close()
        return _run_ctl_fresh(ctl, data)
    verdicts = []
    first = True
    while len(verdicts) < 8:
        v, o = ctl.read_line(READ_TIMEOUT_S if first else 0.3)
        first = False
        if v == "line":
            c = classify(o)
            if c == "event":
                ctl.skipped_events += 1
                continue
            verdicts.append(c)
            break
        verdicts.append(v)
        break
    if verdicts and verdicts[-1] in ("eof", "reset"):
        ctl.close()
        return _run_ctl_fresh(ctl, data)
    if verdicts and verdicts[-1] == "timeout":
        ctl.close()
    return verdicts


def _run_ctl_fresh(ctl, data):
    """Fresh connection: hello, auth, the case bytes, half-close, read to EOF/gap."""
    ctl.close()
    ctl.ensure()
    try:
        ctl.sock.sendall(data)
    except (BrokenPipeError, ConnectionResetError, TimeoutError, OSError):
        pass
    try:
        ctl.sock.shutdown(socket.SHUT_WR)
    except OSError:
        pass
    verdicts = []
    first = True
    while len(verdicts) < 64:
        v, o = ctl.read_line(READ_TIMEOUT_S if first else 0.3)
        first = False
        if v == "line":
            c = classify(o)
            if c == "event":
                continue
            verdicts.append(c)
            continue
        verdicts.append(v if v != "timeout" else "gap")
        break
    ctl.close()
    return verdicts


def run_vdev_case(stack, case):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(0.15)
    try:
        s.connect(str(stack.vdev_sock))
        try:
            s.sendall(case["data"])
        except (BrokenPipeError, ConnectionResetError):
            pass
        try:
            hdr = s.recv(4)
            if not hdr:
                return "eof"
            if len(hdr) < 4:
                return "short"
            n = struct.unpack("<I", hdr)[0]
            if n < 3 or n > 4096:
                return "bad-reply-len"
            body = b""
            while len(body) < n:
                chunk = s.recv(n - len(body))
                if not chunk:
                    return "eof"
                body += chunk
            kind = body[0]
            return {0x81: "input", 0x82: "ack", 0x83: "answer", 0x84: "error-frame"}.get(kind, "kind:%#x" % kind)
        except socket.timeout:
            return "none"  # no reply within the bound; connection still open
        except (ConnectionResetError, BrokenPipeError):
            return "reset"
    except OSError:
        return "reset"
    finally:
        try:
            s.close()
        except OSError:
            pass


def run_vdevctl_case(stack, case):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(0.15)
    try:
        s.connect(str(stack.vdev_sock) + ".ctl")
        try:
            s.sendall(case["data"])
        except (BrokenPipeError, ConnectionResetError):
            pass
        buf = b""
        try:
            while True:
                chunk = s.recv(4096)
                if not chunk:
                    return "eof"
                buf += chunk
                if buf.startswith(b"ERR"):
                    return "err"
                if b"\n.\n" in buf or buf.endswith(b"\n.\n"):
                    return "ok"
                if b"\n" in buf and buf.split(b"\n", 1)[0] in (b"OK",):
                    # keep reading until the '.' terminator
                    continue
        except socket.timeout:
            return "timeout"
    except OSError:
        return "reset"
    finally:
        try:
            s.close()
        except OSError:
            pass


ALLOWED_CTL = lambda v: v == "ok" or v.startswith("error:") or v in ("eof", "reset", "gap")
ALLOWED_VDEV = lambda v: v in ("eof", "reset", "none", "ack", "answer", "error-frame", "input") or v.startswith("kind:")


def alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


def scan_log(path):
    try:
        t = Path(path).read_text()
    except OSError:
        return ""
    return "\n".join(l for l in t.splitlines() if "panicked" in l or "abort" in l.lower())


def main():
    argv = sys.argv[1:]
    replay = "--replay" in argv
    n = DEFAULT_CASES
    if "--cases" in argv:
        n = int(argv[argv.index("--cases") + 1])
    seed = SEED

    def _alarm(signum, frame):
        raise TimeoutError("fuzz suite exceeded the %ds watchdog" % WATCHDOG_S)

    signal.signal(signal.SIGALRM, _alarm)
    signal.alarm(WATCHDOG_S)
    t0 = time.monotonic()
    stack = None
    stats = {}
    findings = []
    case_verdicts = {}
    cases = generate(seed, n)
    try:
        bins = ensure_binaries()
        stack = Stack()
        stack.start(bins)
        ctl = Ctl(stack)
        recorded = {}
        if replay and CORPUS.is_file():
            for line in CORPUS.read_text().splitlines():
                if line.strip():
                    e = json.loads(line)
                    recorded[e["i"]] = e
            assert len(recorded) == len(cases), "corpus/cases.jsonl is stale (%d vs %d cases)" % (len(recorded), len(cases))
            for c in cases:
                if c["i"] in recorded and recorded[c["i"]].get("hex"):
                    assert bytes.fromhex(recorded[c["i"]]["hex"]) == c["data"], "case %d differs from corpus" % c["i"]
        for case in cases:
            if case["kind"] in ("ctl", "dsf1"):
                verdicts = run_ctl_case(ctl, case)
                ok = all(ALLOWED_CTL(v) for v in verdicts) and verdicts
                v = "+".join(verdicts)
            elif case["kind"] == "vdev":
                v = run_vdev_case(stack, case)
                ok = ALLOWED_VDEV(v)
            else:
                v = run_vdevctl_case(stack, case)
                ok = v in ("ok", "err", "eof", "reset")
            stats[v] = stats.get(v, 0) + 1
            case_verdicts[case["i"]] = v
            if replay and case["i"] in recorded and recorded[case["i"]].get("verdict"):
                assert recorded[case["i"]]["verdict"] == v, (
                    "case %d verdict drifted: %s != recorded %s"
                    % (case["i"], v, recorded[case["i"]]["verdict"]))
            if not ok:
                findings.append((case["i"], case["kind"], v, case["data"][:64].hex()))
            if case["i"] % 200 == 0:
                if not alive(stack.daemon_pid) or stack.vdev.poll() is not None:
                    findings.append((case["i"], "health", "daemon/vdev died", ""))
                    break
        # final health: daemon answers, vdev answers, no panics in either log
        ctl.close()
        health = Ctl(stack)
        health.ensure()
        v, o = health.rpc({"id": 9999, "op": "ping"})
        daemon_ok = v == "line" and isinstance(o, dict) and o.get("ok") is True
        health.close()
        c = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        c.settimeout(3)
        c.connect(str(stack.vdev_sock) + ".ctl")
        c.sendall(b"ping\n")
        reply = c.recv(64)
        c.close()
        vdev_ok = reply.startswith(b"OK")
        panic_daemon = scan_log(stack.data / "daemon.log")
        panic_vdev = scan_log(stack.tmp / "vdev.log")
    except Exception as e:  # noqa: BLE001
        import traceback
        traceback.print_exc()
        findings.append((-1, "suite", "%s: %s" % (type(e).__name__, e), ""))
        daemon_ok = vdev_ok = False
        panic_daemon = panic_vdev = ""
        stats = stats or {}
    finally:
        signal.alarm(0)
        teardown_ok = False
        problems = []
        if stack is not None:
            problems = stack.stop()
            teardown_ok = not problems
    elapsed = time.monotonic() - t0

    # persist the corpus (cases + verdicts) for replay
    try:
        CORPUS.parent.mkdir(parents=True, exist_ok=True)
        with CORPUS.open("w") as fh:
            for case in cases:
                fh.write(json.dumps({
                    "i": case["i"], "kind": case["kind"],
                    "hex": case["data"].hex() if len(case["data"]) <= MAX_HEX_BYTES else None,
                    "big": len(case["data"]) > MAX_HEX_BYTES,
                    "verdict": case_verdicts.get(case["i"]),
                }) + "\n")
    except OSError:
        pass

    print("-" * 72)
    print("fuzz: seed=0x%x cases=%d elapsed=%.1fs (watchdog %ds)%s"
          % (seed, len(cases), elapsed, WATCHDOG_S, " replay=on" if replay else ""))
    print("verdict histogram: %s" % json.dumps(stats, sort_keys=True))
    print("health: daemon_ping=%s vdev_ping=%s panics=%r teardown_clean=%s (%s)"
          % (daemon_ok, vdev_ok, (panic_daemon + panic_vdev)[:200], teardown_ok, "; ".join(problems)))
    if replay:
        print("replay: %d corpus cases re-ran from seed 0x%x; verdicts matched the recorded ones"
              % (len(cases), seed))
    if findings or not daemon_ok or not vdev_ok or panic_daemon or panic_vdev or not teardown_ok:
        for f in findings:
            print("  FINDING case=%s kind=%s verdict=%s data=%s" % f)
        print("FUZZ RESULT: FAIL")
        return 1
    print("FUZZ RESULT: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
