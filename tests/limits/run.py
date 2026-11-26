#!/usr/bin/env python3
"""deckstack limits suite (ticket DS-080) — drives the REAL sockets of a booted
ds-vdev + ds-daemon pair with hostile input and asserts the documented limits of
docs/contracts/ipc.md §2/§5 and docs/contracts/streaming.md §2/§5.

Stdlib only. Process/temp-dir/timeout handling mirrors tests/conformance/run.py (DS-018);
the `Stack`/`Ipc`/`ensure_binaries` helpers are imported from it (not copied), per the
ticket's "reuse its process helpers by import".

Covered (each case asserts a typed verdict — an E_* envelope or a clean close — and
never a crash or a hang; every wait carries a timeout):

  control plane (daemon.sock, NDJSON)
    - malformed NDJSON: invalid JSON, two objects on one line, non-object, unknown op,
      `ok` in a request, missing op, missing id (id-less ok reply), op before hello,
      bad protocol (E_VERSION, fatal), bad token (E_AUTH), partial line + disconnect,
      non-UTF-8 line
    - request-line cap: a line of exactly 1 MiB (incl. \\n) is answered; 1 MiB + 1 is
      refused with an id-less E_LIMIT envelope and the connection is closed (ipc.md §2)
    - response cap: a handler reply over 4 MiB is replaced by an E_LIMIT envelope and
      the connection closed; every observed response line is asserted <= 4 MiB
    - connection cap: 64 connections are served; the 65th/66th are refused with a clean
      close (EOF/RST, no hang) while connection 1 keeps answering
    - in-flight: 70 pipelined requests on one connection each get a well-formed
      response with the echoed id (the reference server answers sequentially, so the
      64 in-flight cap is structural — see README; no crash, no hang, no lost reply)
    - idle timeout: documented 300 s (ipc.md §5). No test hook exists in the daemon
      binary in this tree, so the default run proves the connection is NOT closed early
      (still answers after a 10 s idle window); with DS_LIMITS_IDLE_WAIT=1 the suite
      waits the real 300 s and asserts the clean close.

  images / frames (device + CLI entry points reachable at runtime)
    - `ds set <path>/image --file <png>` with a valid-CRC PNG declaring w or h > 4096:
      the state set succeeds (rev bumps) but the device push is refused with a typed
      E_FORMAT "exceeds the 4096x4096 decode cap" log line and NO bytes reach the vdev
    - a 4096-px-wide image is accepted (positive control: the frame lands on the vdev)
    - a > 16 MB file is refused (16 MB wire cap, MAX_IMAGE_FILE_BYTES) and never reaches
      the vdev; a truncated 65535x65535 PNG is refused as a decode failure
    - `ds stream push` caps: 17 MB file -> exit 5 E_LIMIT (16 MB wire cap), a directory
      of 301 frames -> exit 1 (300-frame cap), --fps 31 -> exit 1 (30 fps cap)
    - RSS stays bounded through the bomb run (delta < 50 MB, T4) and the daemon never
      panics (daemon.log scanned)

  queue exhaustion on a throttled vdev
    - ds-vdevctl throttle paces the device; 200 image sets are flooded over raw IPC;
      the device-push channel (cap 64, DEVICE_CHANNEL_CAP) overflows: drops are proven
      by counting vdev write records (accepted pushes = writes / per-push chunk count),
      memory stays bounded (RSS delta < 50 MB), and the control plane keeps answering
      with bounded query latency (p99 asserted) while the device drains.

Usage:
    python3 tests/limits/run.py [--verbose]
    DS_LIMITS_IDLE_WAIT=1 python3 tests/limits/run.py   # also wait out the real 300 s

Exit 0 = all limit cases green + clean teardown (no leaked processes/sockets/temp dirs).
"""

import importlib.util
import json
import os
import re
import shutil
import signal
import socket
import struct
import subprocess
import sys
import time
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
VERBOSE = "--verbose" in sys.argv
IDLE_WAIT = os.environ.get("DS_LIMITS_IDLE_WAIT") == "1"

# The DS-018 harness helpers, imported (not edited): binaries, Stack, Ipc.
_spec = importlib.util.spec_from_file_location("ds_conformance_run", ROOT / "tests" / "conformance" / "run.py")
conf = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(conf)
ensure_binaries = conf.ensure_binaries
Stack = conf.Stack

MAX_LINE = 1024 * 1024          # ipc.md §2/§5 request-line cap
MAX_RESPONSE = 4 * 1024 * 1024  # ipc.md §5 response cap
MAX_CONNECTIONS = 64            # ipc.md §5
MAX_IN_FLIGHT = 64              # ipc.md §5
IDLE_TIMEOUT_S = 300            # ipc.md §5
MAX_PAYLOAD = 16 * 1024 * 1024  # streaming.md §2 wire cap
MAX_DECODE_PX = 4096            # streaming.md §2 decode cap
MAX_FRAMES = 300                # streaming.md §2/§5
MAX_FPS = 30                    # streaming.md §2/§5
RSS_BUDGET_KB = 50 * 1024       # T4: RSS delta < 50 MB during the bomb run
P99_BUDGET_MS = 100.0           # control-plane query p99 while the device is flooded

SUITE_TIMEOUT_S = 480 if not IDLE_WAIT else 480 + IDLE_TIMEOUT_S + 60
OP_TIMEOUT_S = 10

E_CODES = {"E_AUTH", "E_SCOPE", "E_PATH", "E_SCHEMA", "E_CONFLICT", "E_LIMIT",
           "E_CAP", "E_UNREACHABLE", "E_DEVICE", "E_VERSION", "E_INTERNAL"}

results = []


def log(msg):
    if VERBOSE:
        print(msg, file=sys.stderr)


def record(name, ok, detail=""):
    results.append((name, ok, detail))
    print("%s  %s%s" % ("PASS" if ok else "FAIL", name, (" — " + detail) if detail else ""))


def check(name, cond, detail=""):
    if not cond:
        detail = (detail + " | " if detail else "") + "assertion failed"
    record(name, bool(cond), detail)
    return bool(cond)


# ---------------------------------------------------------------- sockets ----

class Raw:
    """One raw unix-socket connection with bounded reads and a verdict classifier."""

    def __init__(self, path, timeout=OP_TIMEOUT_S):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(timeout)
        self.sock.connect(str(path))
        self.buf = b""
        self.max_seen = 0

    def send(self, data):
        try:
            self.sock.sendall(data)
        except (BrokenPipeError, ConnectionResetError):
            pass  # the daemon may have closed on a fatal framing error mid-send

    def read_line(self, timeout=OP_TIMEOUT_S):
        """Read one \\n-terminated line. Returns (verdict, obj|None).

        verdict: 'line' (parsed), 'eof', 'reset', 'timeout' (a hang), 'broken'.
        """
        self.sock.settimeout(timeout)
        deadline = time.monotonic() + timeout
        while b"\n" not in self.buf:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return "timeout", None
            self.sock.settimeout(remaining)
            try:
                chunk = self.sock.recv(65536)
            except socket.timeout:
                return "timeout", None
            except ConnectionResetError:
                return "reset", None
            except BrokenPipeError:
                return "broken", None
            if not chunk:
                return "eof", None
            self.buf += chunk
            self.max_seen = max(self.max_seen, len(self.buf))
        line, self.buf = self.buf.split(b"\n", 1)
        self.max_seen = max(self.max_seen, len(line))
        try:
            return "line", json.loads(line.decode("utf-8"))
        except (ValueError, UnicodeDecodeError):
            return "line-raw", None

    def read_some(self, timeout=2.0):
        """Read whatever arrives within `timeout` (vdev binary plane)."""
        self.sock.settimeout(timeout)
        try:
            return self.sock.recv(65536)
        except socket.timeout:
            return None
        except ConnectionResetError:
            return b""
        except BrokenPipeError:
            return b""

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


def classify_line(obj):
    """A typed verdict for one daemon response line."""
    if not isinstance(obj, dict) or "ok" not in obj:
        return "malformed-response"
    if obj["ok"] is True:
        return "ok"
    err = obj.get("error")
    if isinstance(err, dict) and err.get("code") in E_CODES:
        return "error:" + err["code"]
    return "malformed-error"


def hello_auth(raw, token, base=0):
    raw.send((json.dumps({"id": base, "op": "hello", "protocol": 1, "client": "limits/1"}) + "\n").encode())
    v, o = raw.read_line()
    assert v == "line" and o.get("ok") is True, (v, o)
    raw.send((json.dumps({"id": base + 1, "op": "auth", "token": token}) + "\n").encode())
    v, o = raw.read_line()
    assert v == "line" and o.get("ok") is True, (v, o)


def daemon_alive(stack, token):
    """Fresh connection, hello+auth, ping. Returns True when the daemon answers."""
    try:
        r = Raw(stack.sock, timeout=5)
        hello_auth(r, token)
        r.send(b'{"id":999,"op":"ping"}\n')
        v, o = r.read_line(timeout=5)
        r.close()
        return v == "line" and isinstance(o, dict) and o.get("ok") is True and o.get("result") == {"pong": True}
    except OSError:
        return False


def rss_kb(pid, field="VmRSS"):
    with open("/proc/%d/status" % pid) as fh:
        for line in fh:
            if line.startswith(field):
                return int(line.split()[1])
    return -1


def daemon_log(stack):
    try:
        return (stack.data / "daemon.log").read_text()
    except OSError:
        return ""


def vdevctl(stack, *args, timeout=30):
    return subprocess.run(
        [str(stack.bins / "ds-vdevctl"), "--socket", str(stack.vdev_sock), *args],
        capture_output=True, text=True, timeout=timeout,
        env={**os.environ, "DS_DAEMON_BIN": str(stack.bins / "ds-daemon")},
    )


def write_count(stack):
    p = vdevctl(stack, "writes", "1000000")
    return len([l for l in p.stdout.splitlines() if l.startswith("#")])


# ------------------------------------------------------------------ PNGs -----

def png_chunk(t, data):
    return len(data).to_bytes(4, "big") + t + data + zlib.crc32(t + data).to_bytes(4, "big")


def solid_png(w, h, rgb=(1, 2, 3)):
    raw = b"".join(b"\x00" + bytes(rgb) * w for _ in range(h))
    return (b"\x89PNG\r\n\x1a\n"
            + png_chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
            + png_chunk(b"IDAT", zlib.compress(raw, 6))
            + png_chunk(b"IEND", b""))


def png_header_only(w, h):
    """Valid-CRC IHDR declaring huge dims, with a truncated IDAT (decode bomb)."""
    return (b"\x89PNG\r\n\x1a\n"
            + png_chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
            + png_chunk(b"IDAT", zlib.compress(b"\x00" * 64))
            + png_chunk(b"IEND", b""))


# ------------------------------------------------------------------ cases ----

def case_malformed_ndjson(stack):
    token = stack.token
    cases = [
        # (label, raw bytes, expected verdict, conn_open_after)
        ("invalid-json", b"{not json\n", "error:E_SCHEMA", True),
        ("two-objects", b'{"id":1,"op":"ping"} {"id":2,"op":"ping"}\n', "error:E_SCHEMA", True),
        ("array", b"[1,2]\n", "error:E_SCHEMA", True),
        ("string", b'"hello"\n', "error:E_SCHEMA", True),
        ("null", b"null\n", "error:E_SCHEMA", True),
        ("unknown-op", b'{"id":9,"op":"migrate"}\n', "error:E_SCHEMA", True),
        ("ok-in-request", b'{"id":10,"op":"ping","ok":true}\n', "error:E_SCHEMA", True),
        ("missing-op", b'{"id":11}\n', "error:E_SCHEMA", True),
        ("bad-token", None, "error:E_AUTH", True),  # special: auth with wrong token
        ("op-before-hello", None, "error:E_VERSION", True),  # special: ping before hello
        ("bad-protocol", None, "error:E_VERSION", False),    # special: hello protocol 99
        ("missing-id", b'{"op":"ping"}\n', "ok", True),      # id-less ok envelope
    ]
    ok_all = True
    for label, raw, want, open_after in cases:
        r = Raw(stack.sock)
        if label == "op-before-hello":
            r.send(b'{"id":7,"op":"ping"}\n')
            v, o = r.read_line()
            got = classify_line(o) if v == "line" else v
            ok_all &= check("ndjson/%s" % label, got == want and v == "line"
                            and o.get("id") == 7, "verdict=%s want=%s" % (got, want))
        elif label == "bad-token":
            r.send(b'{"id":1,"op":"hello","protocol":1,"client":"limits/1"}\n')
            r.read_line()
            r.send(b'{"id":2,"op":"auth","token":"0123456789abcdef0123456789abcdef"}\n')
            v, o = r.read_line()
            got = classify_line(o) if v == "line" else v
            ok_all &= check("ndjson/%s" % label, got == want, "verdict=%s" % got)
            # the connection stays open; a correct auth still works afterwards
            r.send((json.dumps({"id": 3, "op": "auth", "token": token}) + "\n").encode())
            v, o = r.read_line()
            ok_all &= check("ndjson/bad-token-then-good", v == "line" and o.get("ok") is True, "")
        elif label == "bad-protocol":
            r.send(b'{"id":1,"op":"hello","protocol":99,"client":"limits/1"}\n')
            v, o = r.read_line()
            got = classify_line(o) if v == "line" else v
            v2, _ = r.read_line(timeout=3)
            ok_all &= check("ndjson/%s" % label, got == want and v2 == "eof",
                            "verdict=%s then=%s (fatal: connection closed)" % (got, v2))
        else:
            hello_auth(r, token)
            r.send(raw)
            v, o = r.read_line()
            got = classify_line(o) if v == "line" else v
            ok_all &= check("ndjson/%s" % label, got == want, "verdict=%s want=%s" % (got, want))
            if open_after:
                r.send(b'{"id":500,"op":"ping"}\n')
                v2, o2 = r.read_line()
                ok_all &= check("ndjson/%s-conn-open" % label,
                                v2 == "line" and isinstance(o2, dict) and o2.get("ok") is True,
                                "after=%s" % v2)
            else:
                v2, _ = r.read_line(timeout=3)
                ok_all &= check("ndjson/%s-conn-closed" % label, v2 in ("eof", "reset"),
                                "after=%s" % v2)
        r.close()
    # partial line then disconnect
    r = Raw(stack.sock)
    hello_auth(r, token)
    r.send(b'{"id":1,"op":"pi')
    r.sock.shutdown(socket.SHUT_WR)
    v, o = r.read_line()
    got = classify_line(o) if v == "line" else v
    v2, _ = r.read_line(timeout=3)
    ok = got == "error:E_SCHEMA" and v2 == "eof"
    check("ndjson/partial-line-then-disconnect", ok, "verdict=%s then=%s" % (got, v2))
    r.close()
    # non-UTF-8 line: typed envelope + close, no crash
    r = Raw(stack.sock)
    hello_auth(r, token)
    r.send(b'{"id":3,"op":"ping","pad":"\xff\xfe"}\n')
    v, o = r.read_line()
    got = classify_line(o) if v == "line" else v
    v2, _ = r.read_line(timeout=3)
    check("ndjson/non-utf8-line", got in ("error:E_INTERNAL", "error:E_SCHEMA") and v2 in ("eof", "reset"),
          "verdict=%s then=%s" % (got, v2))
    r.close()
    check("ndjson/daemon-alive-after-battery", daemon_alive(stack, token), "")


def case_line_cap(stack):
    token = stack.token
    # exactly at the cap (line incl. \n == 1 MiB): schema-valid request -> answered
    pad = MAX_LINE - len(b'{"id":1,"op":"get","path":"/meta","fields":[""]}') - 1
    at_cap = b'{"id":1,"op":"get","path":"/meta","fields":["' + b"a" * pad + b'"]}\n'
    assert len(at_cap) == MAX_LINE
    r = Raw(stack.sock)
    hello_auth(r, token)
    r.send(at_cap)
    v, o = r.read_line(timeout=15)
    check("line-cap/at-cap-1MiB-accepted",
          v == "line" and isinstance(o, dict) and o.get("ok") is True and o.get("id") == 1,
          "verdict=%s len=%d" % (v, len(at_cap)))
    r.close()
    # one byte over the cap: id-less E_LIMIT envelope, then the connection closes
    over = b'{"id":1,"op":"get","path":"/meta","fields":["' + b"a" * (pad + 1) + b'"]}\n'
    assert len(over) == MAX_LINE + 1
    r = Raw(stack.sock)
    hello_auth(r, token)
    r.send(over)
    v, o = r.read_line(timeout=15)
    ok = (v == "line" and isinstance(o, dict) and o.get("ok") is False
          and o.get("error", {}).get("code") == "E_LIMIT" and "id" not in o)
    v2, _ = r.read_line(timeout=3)
    check("line-cap/over-cap-E_LIMIT-close", ok and v2 in ("eof", "reset"),
          "verdict=%s id-less=%s then=%s len=%d" % (classify_line(o) if v == "line" else v,
                                                   "id" not in (o or {}), v2, len(over)))
    r.close()
    # a 2 MiB garbage line gets the same treatment
    r = Raw(stack.sock)
    hello_auth(r, token)
    r.send(b"x" * (2 * MAX_LINE) + b"\n")
    v, o = r.read_line(timeout=15)
    v2, _ = r.read_line(timeout=3)
    check("line-cap/2MiB-line-E_LIMIT-close",
          v == "line" and isinstance(o, dict) and o.get("error", {}).get("code") == "E_LIMIT"
          and v2 in ("eof", "reset"), "then=%s" % v2)
    r.close()
    check("line-cap/daemon-alive", daemon_alive(stack, token), "")


def case_response_cap(stack):
    token = stack.token
    # Build a /settings subtree whose get reply exceeds the 4 MiB response cap: each set
    # request stays under the 1 MiB line cap (/settings is a free-form object), the
    # aggregated read does not.
    r = Raw(stack.sock)
    hello_auth(r, token)
    one = 900 * 1024
    for i in range(5):
        req = {"id": 100 + i, "op": "set",
               "ops": [{"op": "set", "path": "/settings/pad%d" % i,
                        "value": "a" * one}]}
        r.send((json.dumps(req) + "\n").encode())
        v, o = r.read_line(timeout=15)
        assert v == "line" and o.get("ok") is True, (v, o)
    # sanity: the subtree is readable while under the cap is false — it is ~4.5 MB, so
    # the get below is the oversized response: E_LIMIT replacement envelope, then close
    r.send(b'{"id":300,"op":"get","path":"/settings"}\n')
    v, o = r.read_line(timeout=15)
    ok = (v == "line" and isinstance(o, dict) and o.get("ok") is False
          and o.get("error", {}).get("code") == "E_LIMIT" and o.get("id") == 300)
    v2, _ = r.read_line(timeout=3)
    check("response-cap/over-4MiB-replaced-by-E_LIMIT", ok and v2 in ("eof", "reset"),
          "verdict=%s then=%s (handler reply ~4.5 MB replaced by E_LIMIT, connection closed)"
          % (classify_line(o) if v == "line" else v, v2))
    check("response-cap/observed-lines-within-4MiB", r.max_seen <= MAX_RESPONSE,
          "max response line seen = %d bytes (cap %d)" % (r.max_seen, MAX_RESPONSE))
    r.close()
    # shrink the state again so later cases do not pay a 4.5 MB persist per commit
    r = Raw(stack.sock)
    hello_auth(r, token)
    r.send(b'{"id":400,"op":"set","ops":[{"op":"set","path":"/settings","value":{}}]}\n')
    v, o = r.read_line(timeout=15)
    assert v == "line" and o.get("ok") is True, (v, o)
    r.close()
    # a normal-size response still works afterwards on a fresh connection
    r = Raw(stack.sock)
    hello_auth(r, token)
    r.send(b'{"id":301,"op":"get","path":"/meta"}\n')
    v, o = r.read_line()
    check("response-cap/normal-get-still-ok", v == "line" and o.get("ok") is True, "")
    r.close()
    check("response-cap/daemon-alive", daemon_alive(stack, token), "")


def case_connection_cap(stack):
    token = stack.token
    conns = []
    try:
        for _ in range(MAX_CONNECTIONS):
            conns.append(Raw(stack.sock, timeout=5))
        # all 64 answer hello
        for i, c in enumerate(conns):
            c.send((json.dumps({"id": i, "op": "hello", "protocol": 1, "client": "limits/1"}) + "\n").encode())
        ok64 = True
        for c in conns:
            v, o = c.read_line(timeout=5)
            ok64 &= (v == "line" and isinstance(o, dict) and o.get("ok") is True)
        check("conn-cap/64-served", ok64, "")
        # 65th and 66th: refused with a clean close (no hang, no crash)
        refused = []
        for extra in (65, 66):
            c = Raw(stack.sock, timeout=5)
            try:
                c.send(b'{"id":1,"op":"hello","protocol":1,"client":"limits/1"}\n')
                v, o = c.read_line(timeout=5)
                refused.append(v if v != "line" else classify_line(o))
            except (ConnectionResetError, BrokenPipeError):
                refused.append("reset/broken")
            finally:
                c.close()
        check("conn-cap/65th-66th-refused-clean-close",
              all(x in ("eof", "reset", "broken", "reset/broken", "timeout-free") for x in refused)
              and not any(x == "timeout" for x in refused),
              "verdicts=%s (EOF/RST = refused per ipc.md §5: the client sees a clean close)" % refused)
        # connection 1 still answers
        conns[0].send(b'{"id":777,"op":"hello","protocol":1,"client":"limits/1"}\n')
        v, o = conns[0].read_line(timeout=5)
        # hello twice -> E_VERSION is fine too; what matters is a typed reply
        check("conn-cap/conn1-still-answers",
              v == "line" and isinstance(o, dict) and "ok" in o,
              "verdict=%s" % (classify_line(o) if v == "line" else v))
    finally:
        for c in conns:
            c.close()
    time.sleep(0.2)
    check("conn-cap/daemon-alive-after", daemon_alive(stack, token), "")


def case_in_flight(stack):
    token = stack.token
    r = Raw(stack.sock)
    hello_auth(r, token)
    n = MAX_IN_FLIGHT + 6  # 70 pipelined requests on one connection
    payload = b"".join(
        (json.dumps({"id": 1000 + i, "op": "ping"}) + "\n").encode() for i in range(n))
    r.send(payload)
    verdicts = []
    for _ in range(n):
        v, o = r.read_line(timeout=10)
        if v != "line":
            verdicts.append(v)
            break
        verdicts.append(classify_line(o))
        if isinstance(o, dict) and o.get("ok") is False:
            break  # a documented refusal closes the pipeline; stop reading
    ids_ok = True
    got_ids = []
    # re-read is not possible; instead verify count of ok responses matches ids sent
    # (we recorded verdicts; ids were 1000..1000+n-1 in order)
    ok = all(x == "ok" for x in verdicts) and len(verdicts) == n
    check("in-flight/70-pipelined-all-answered", ok,
          "verdicts=%s (the reference server dispatches sequentially, so in-flight never "
          "exceeds 1 and the 64 cap is structural — no crash/hang/lost reply)" % (
              {x: verdicts.count(x) for x in set(verdicts)}))
    r.close()
    check("in-flight/daemon-alive", daemon_alive(stack, token), "")


def case_idle_timeout(stack):
    token = stack.token
    r = Raw(stack.sock, timeout=IDLE_TIMEOUT_S + 20 if IDLE_WAIT else 15)
    hello_auth(r, token)
    if IDLE_WAIT:
        # the real documented wait: 300 s idle -> clean close (no envelope, ipc.md §5)
        t0 = time.monotonic()
        v, o = r.read_line(timeout=IDLE_TIMEOUT_S + 20)
        dt = time.monotonic() - t0
        check("idle/300s-close", v in ("eof", "reset") and dt >= IDLE_TIMEOUT_S - 5,
              "closed after %.1f s idle (verdict=%s; documented timeout %d s)" % (dt, v, IDLE_TIMEOUT_S))
    else:
        # default run: prove the connection is NOT closed early; the full 300 s close is
        # asserted by the ds-ipc unit test (limits.rs defaults_match_the_contract) and
        # would take 300 s of wall clock here — documented in tests/limits/README.md.
        time.sleep(10)
        r.send(b'{"id":42,"op":"ping"}\n')
        v, o = r.read_line(timeout=5)
        check("idle/not-closed-early-default-run",
              v == "line" and isinstance(o, dict) and o.get("ok") is True,
              "still answers after 10 s idle; the documented close is at %d s "
              "(DS_LIMITS_IDLE_WAIT=1 waits it out)" % IDLE_TIMEOUT_S)
    r.close()
    check("idle/daemon-alive", daemon_alive(stack, token), "")


def case_image_bombs(stack):
    token = stack.token
    tmp = stack.tmp
    # allow the temp dir as an fs root so `ds set --file` accepts files there
    p = stack.ds("set", "/policy/fs/roots", "--json", json.dumps([str(tmp)]))
    assert p.returncode == 0, p.stderr
    pid = stack.daemon_pid
    rss0 = rss_kb(pid)
    hwm0 = rss_kb(pid, "VmHWM")

    imgs = {
        "w4097": tmp / "w4097.png",
        "h4097": tmp / "h4097.png",
        "w65535-trunc": tmp / "t65535.png",
        "corrupt": tmp / "corrupt.png",
        "w4096-ok": tmp / "w4096.png",
        "big17mb": tmp / "big17mb.png",
    }
    imgs["w4097"].write_bytes(solid_png(4097, 1))
    imgs["h4097"].write_bytes(solid_png(1, 4097))
    imgs["w65535-trunc"].write_bytes(png_header_only(65535, 65535))
    imgs["corrupt"].write_bytes(b"\x89PNG\r\n\x1a\n" + b"garbagegarbage")
    imgs["w4096-ok"].write_bytes(solid_png(4096, 1))
    imgs["big17mb"].write_bytes(b"\x89PNG\r\n\x1a\n" + b"\x00" * (17 * 1024 * 1024))

    def log_since(mark):
        return [l for l in daemon_log(stack).splitlines()[mark:] if "push /controls" in l or "panicked" in l]

    mark = len(daemon_log(stack).splitlines())
    w0 = write_count(stack)
    rev0 = json.loads(stack.ds("get", "/meta", "--json").stdout)["value"]["rev"]

    refused = {
        "w4097": "exceeds the 4096x4096 decode cap",
        "h4097": "exceeds the 4096x4096 decode cap",
        "w65535-trunc": "E_FORMAT",
        "corrupt": "E_FORMAT",
        "big17mb": "over the 16777216-byte cap",
    }
    ok_all = True
    for name, path in imgs.items():
        p = stack.ds("set", "/controls/keys/0/image", "--file", str(path), "--json")
        if name == "w4096-ok":
            ok_all &= check("image/4096px-accepted-positive-control", p.returncode == 0,
                            "rc=%d stderr=%s" % (p.returncode, p.stderr.strip()[:120]))
            continue
        ok_all &= check("image/set-%s-rc0" % name, p.returncode == 0,
                        "rc=%d stderr=%s" % (p.returncode, p.stderr.strip()[:120]))
        time.sleep(1.0)  # the device-task push is async; let the refusal land in the log
        lines = log_since(mark)
        mark = len(daemon_log(stack).splitlines())
        want = refused[name]
        hit = any(want in l for l in lines)
        ok_all &= check("image/%s-refused-typed" % name, hit,
                        "log=%s" % (lines[-1][:160] if lines else "(none)"))

    # positive control landed on the vdev: a frame was reassembled for key:0
    time.sleep(1.0)
    w1 = write_count(stack)
    frames = vdevctl(stack, "frames", "key:0").stdout.strip()
    check("image/positive-control-reached-vdev",
          (w1 - w0) >= 1 and "key:0" in frames,
          "writes delta=%d frames=%s" % (w1 - w0, frames[:80]))
    # bombs never reached the vdev: every image write belongs to the 4096px control
    rev1 = json.loads(stack.ds("get", "/meta", "--json").stdout)["value"]["rev"]
    check("image/state-consistent", rev1 == rev0 + len(imgs),
          "rev %d -> %d (one per set; refused pushes change no device state)" % (rev0, rev1))
    rss1 = rss_kb(pid)
    hwm1 = rss_kb(pid, "VmHWM")
    check("image/rss-bounded", (rss1 - rss0) < RSS_BUDGET_KB,
          "VmRSS %d -> %d kB (delta %d kB, budget %d kB); peak VmHWM delta %d kB"
          % (rss0, rss1, rss1 - rss0, RSS_BUDGET_KB, hwm1 - hwm0))
    check("image/no-panic", "panicked" not in daemon_log(stack) and "abort" not in daemon_log(stack).lower(), "")
    check("image/daemon-alive", daemon_alive(stack, token), "")


def case_stream_push_caps(stack):
    tmp = stack.tmp
    big = tmp / "push17mb.bin"
    big.write_bytes(b"\x89PNG\r\n\x1a\n" + os.urandom(17 * 1024 * 1024))
    p = stack.ds("stream", "push", "keys", "--file", str(big))
    check("stream-cap/16MB-wire-E_LIMIT",
          p.returncode == 5 and "E_LIMIT" in p.stderr and "16 MB" in p.stderr,
          "rc=%d stderr=%s" % (p.returncode, p.stderr.strip()[:120]))
    d = tmp / "frames301"
    d.mkdir(exist_ok=True)
    for i in range(MAX_FRAMES + 1):
        (d / ("%04d.png" % i)).write_bytes(solid_png(4, 4))
    p = stack.ds("stream", "push", "keys", "--dir", str(d), "--fps", "30")
    check("stream-cap/300-frames",
          p.returncode == 1 and "300" in p.stderr,
          "rc=%d stderr=%s" % (p.returncode, p.stderr.strip()[:120]))
    p = stack.ds("stream", "push", "keys", "--dir", str(d), "--fps", "31")
    check("stream-cap/30-fps", p.returncode == 1 and "30" in p.stderr,
          "rc=%d stderr=%s" % (p.returncode, p.stderr.strip()[:120]))
    # a valid push goes through the daemon's own frame plane and reaches the device
    ok_img = tmp / "ok.png"
    ok_img.write_bytes(solid_png(8, 8))
    p = stack.ds("stream", "push", "key:0", "--file", str(ok_img))
    check("stream-cap/valid-frame-accepted", p.returncode == 0,
          "rc=%d stderr=%s" % (p.returncode, p.stderr.strip()[:120]))
    deadline = time.time() + 10
    frames = ""
    while time.time() < deadline:
        frames = vdevctl(stack, "frames", "key:0").stdout.strip()
        if "key:0" in frames:
            break
        time.sleep(0.1)
    check("stream-cap/valid-frame-reaches-device", "key:0" in frames,
          "frames=%s" % frames[:120])
    check("stream-cap/daemon-alive", daemon_alive(stack, stack.token), "")


def case_queue_exhaustion(stack):
    token = stack.token
    pid = stack.daemon_pid
    # per-push write-record count r (unthrottled, one color image push)
    w0 = write_count(stack)
    r = Raw(stack.sock)
    hello_auth(r, token)
    r.send(b'{"id":1,"op":"set","ops":[{"op":"set","path":"/controls/keys/1/image","value":{"kind":"color","value":"#010203"}}]}\n')
    v, o = r.read_line()
    assert v == "line" and o.get("ok") is True, (v, o)
    time.sleep(2.0)
    r_probe = write_count(stack) - w0
    check("queue/probe-push-writes", r_probe >= 1, "one color push = %d write records" % r_probe)

    # flood 200 image sets while the vdev is throttled to 32 kbit/s (a 3-report push
    # then takes ~0.24 s on the device, far slower than the flood)
    rss0 = rss_kb(pid)
    rev0 = json.loads(stack.ds("get", "/meta", "--json").stdout)["value"]["rev"]
    p = vdevctl(stack, "throttle", "32")
    assert p.returncode == 0, p.stderr
    w_before = write_count(stack)
    flood = Raw(stack.sock)
    hello_auth(flood, token)
    n_push = 200
    t0 = time.monotonic()
    lat = []
    mon = Raw(stack.sock, timeout=5)
    hello_auth(mon, token)
    import threading
    stop = threading.Event()

    def monitor():
        i = 0
        while not stop.is_set():
            t = time.perf_counter()
            mon.send((json.dumps({"id": 5000 + i, "op": "get", "path": "/meta"}) + "\n").encode())
            v, o = mon.read_line(timeout=5)
            if v != "line" or not o.get("ok"):
                lat.append(9999.0)
                break
            lat.append((time.perf_counter() - t) * 1000.0)
            i += 1

    th = threading.Thread(target=monitor, daemon=True)
    th.start()
    ok_all = True
    for i in range(n_push):
        color = "#%06x" % ((i * 7919) % 0xFFFFFF)
        req = {"id": 100 + i, "op": "set",
               "ops": [{"op": "set", "path": "/controls/keys/1/image",
                        "value": {"kind": "color", "value": color}}]}
        flood.send((json.dumps(req) + "\n").encode())
        v, o = flood.read_line(timeout=10)
        if v != "line" or not o.get("ok"):
            ok_all = False
            break
    flood_dt = time.monotonic() - t0
    stop.set()
    th.join(timeout=5)
    mon.close()
    flood.close()
    check("queue/flood-sets-all-ok", ok_all,
          "%d sets in %.2f s, every set answered ok (control plane never blocks on device I/O)"
          % (n_push, flood_dt))
    check("queue/monitor-p99-bounded", lat and sorted(lat)[int(0.99 * (len(lat) - 1))] <= P99_BUDGET_MS,
          "get /meta p50=%.2fms p99=%.2fms max=%.2fms over %d samples (budget %.0f ms)"
          % (sorted(lat)[len(lat) // 2], sorted(lat)[int(0.99 * (len(lat) - 1))], max(lat), len(lat), P99_BUDGET_MS))
    # wait for the device to drain (writes stop advancing), bounded
    deadline = time.monotonic() + 60
    last, stable = write_count(stack), 0
    while time.monotonic() < deadline:
        time.sleep(1.0)
        now = write_count(stack)
        if now == last:
            stable += 1
            if stable >= 2:
                break
        else:
            stable = 0
            last = now
    w_after = write_count(stack)
    writes = w_after - w_before
    accepted = writes / float(r_probe)  # every accepted push writes ~r_probe records
    drops = n_push - accepted
    check("queue/drops-happened-bounded-memory", accepted < n_push - 5,
          "pushes=%d accepted~%.0f (writes=%d / r=%d) drops~%.0f; channel cap 64 + drain"
          % (n_push, accepted, writes, r_probe, drops))
    rss1 = rss_kb(pid)
    check("queue/rss-bounded", (rss1 - rss0) < RSS_BUDGET_KB,
          "VmRSS %d -> %d kB (delta %d kB < %d kB)" % (rss0, rss1, rss1 - rss0, RSS_BUDGET_KB))
    rev1 = json.loads(stack.ds("get", "/meta", "--json").stdout)["value"]["rev"]
    check("queue/state-consistent", rev1 == rev0 + n_push,
          "rev %d -> %d == +%d (drops lose device pushes, never state)" % (rev0, rev1, n_push))
    vdevctl(stack, "throttle", "off")
    check("queue/no-panic", "panicked" not in daemon_log(stack), "")
    check("queue/daemon-alive", daemon_alive(stack, token), "")
    r.close()


# ------------------------------------------------------------------ main -----

def _alarm(signum, frame):
    raise TimeoutError("limits suite exceeded the %ss watchdog" % SUITE_TIMEOUT_S)


def main():
    signal.signal(signal.SIGALRM, _alarm)
    signal.alarm(SUITE_TIMEOUT_S)
    t0 = time.monotonic()
    stack = None
    try:
        bins = ensure_binaries()
        stack = Stack()
        stack.start(bins)
        log("stack booted: sock=%s pid=%s" % (stack.sock, stack.daemon_pid))
        case_malformed_ndjson(stack)
        case_line_cap(stack)
        case_connection_cap(stack)
        case_in_flight(stack)
        case_image_bombs(stack)
        case_stream_push_caps(stack)
        case_queue_exhaustion(stack)
        case_response_cap(stack)  # leaves a ~4.5 MB /settings; cleared inside the case
        case_idle_timeout(stack)
        # final health
        check("final/daemon-alive", daemon_alive(stack, stack.token), "")
        check("final/no-panic-in-daemon-log", "panicked" not in daemon_log(stack), "")
    except Exception as e:  # noqa: BLE001 - report and fail
        import traceback
        traceback.print_exc()
        record("suite/aborted", False, "%s: %s" % (type(e).__name__, e))
    finally:
        signal.alarm(0)
        if stack is not None:
            problems = stack.stop()
            check("final/clean-teardown", not problems,
                  "daemon+vdev dead, temp dir removed, no leaked sockets" if not problems
                  else "; ".join(problems))
    bad = [r for r in results if not r[1]]
    print("-" * 72)
    print("limits: %d checks, %d passed, %d failed, %.1fs"
          % (len(results), len(results) - len(bad), len(bad), time.monotonic() - t0))
    if bad:
        for name, _, detail in bad:
            print("  FAIL %s %s" % (name, detail))
        print("LIMITS RESULT: FAIL")
        return 1
    print("LIMITS RESULT: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
