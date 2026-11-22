#!/usr/bin/env python3
"""deckstack end-to-end walking skeleton + query-under-load bench skeleton (ticket DS-020).

Single entry point, python3 stdlib only. It builds the four phase-1 binaries when they
are missing (bounded), boots ds-vdev + ds-daemon on a fresh temp dir with all sockets
under it, and drives the system ONLY through the real binaries (black box):

    1. ds-vdev --socket <tmp>/vdev.sock  +  ds daemon start --device virtual:<tmp>/vdev.sock
       --data-dir <tmp>/data   (fresh start => empty state, rev 0)
    2. ds set /controls/keys/0/image --file examples/key.png ; ds get ... --json
       round-trips the file visual and rev increased
    3. ds-vdevctl --socket <tmp>/vdev.sock frames key:0 prints the reassembled sha256,
       which must equal the golden ENCODED-frame sha tests/golden/example-key0.sha256
       (DS-030: the daemon re-encodes examples/key.png to baseline JPEG 4:4:4 q90 for
       key:0 before chunking, so the wire frame is the encoded JPEG, not the file)
    4. ds watch --ndjson --max-events 1 sees the event of a set ; ds watch --json exits 1
    5. ds migrate <anything> exits 1 (scope invariant: no migrate verb)
    6. bench skeleton: a background writer loops colour sets while N>=200 sequential
       `ds get /controls/keys/0` calls are timed (after a warmup of 50); p50/p95/p99 are
       printed and the run fails if p99 > 10 ms; the whole load part is bounded to <= 10 s

Usage:
    python3 tests/e2e/run.py [--no-bench] [--verbose]

Exit code 0 = every step green, nonzero = at least one step failed (each failure prints a
precise message). Every wait carries a timeout; the daemon and the vdev are always killed
(SIGTERM then SIGKILL) and the temp dir is removed, so the run can never hang or leak
processes/sockets.

Notes:
- Process/temp-dir/timeout handling mirrors tests/conformance/run.py (DS-018); the ticket
  scope of DS-020 only allows new files under tests/e2e/, so the helpers are duplicated
  here instead of factored into tests/conformance/.
- The bench asserts p99 <= 10 ms on real `ds get` invocations, so this harness builds and
  uses RELEASE binaries (cargo build --release --bin ds --bin ds-daemon --bin ds-vdev
  --bin ds-vdevctl — the -p names of the two vdev bins are ds-vdev-bin/ds-vdevctl-bin, so
  --bin is used). With debug binaries the per-invocation cost of compiling the embedded
  IPC schema validator (~8 ms) alone exceeds the budget. Set DS_BIN_DIR to override the
  binary directory (e.g. DS_BIN_DIR=target/debug python3 tests/e2e/run.py --no-bench).
- `ds-vdev --socket <path>` serves directly; the phase-1 binary has no `up` subcommand
  (an `up` argument would exit 2 as an unknown argument), so the e2e runs the plain
  serve command.
- examples/key.png is a test fixture (the golden example); it is never installed as a
  deck default — the empty-state invariant is asserted on the fresh boot.
"""

import hashlib
import json
import math
import os
import re
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
KEY_PNG = ROOT / "examples" / "key.png"
# DS-030 golden: the frame on the device wire is the daemon's re-encoded baseline JPEG
# (standard Huffman, 4:4:4, q90 for key:N), so step 3 compares the vdev-reassembled frame
# sha against the golden ENCODED-frame sha, not against sha256(examples/key.png).
GOLDEN_DIR = ROOT / "tests" / "golden"
EXAMPLE_KEY0_SHA = GOLDEN_DIR / "example-key0.sha256"

NO_BENCH = "--no-bench" in sys.argv
VERBOSE = "--verbose" in sys.argv

BUILD_TIMEOUT_S = 900
OP_TIMEOUT_S = 20
BOOT_TIMEOUT_S = 30
WATCH_TIMEOUT_S = 20
KILL_TIMEOUT_S = 5
SUITE_TIMEOUT_S = 300

# bench skeleton parameters (ticket DS-020 step 6)
BENCH_WARMUP = 50
BENCH_N = 200
BENCH_P99_LIMIT_MS = 10.0
BENCH_MAX_S = 10.0  # the whole load part stays bounded to <= 10 s

BINARIES = ("ds", "ds-daemon", "ds-vdev", "ds-vdevctl")


def log(msg):
    if VERBOSE:
        print(msg, file=sys.stderr)


def bin_dir():
    env = os.environ.get("DS_BIN_DIR")
    if env:
        return Path(env)
    return ROOT / "target" / "release"


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
    print("building binaries (bounded %ss, release): %s" % (BUILD_TIMEOUT_S, " ".join(missing)))
    subprocess.run(
        [cargo, "build", "--release", "--bin", "ds", "--bin", "ds-daemon",
         "--bin", "ds-vdev", "--bin", "ds-vdevctl"],
        cwd=str(ROOT), timeout=BUILD_TIMEOUT_S, check=True,
        stdout=subprocess.DEVNULL if not VERBOSE else None,
        stderr=subprocess.DEVNULL if not VERBOSE else None,
    )
    missing = [b for b in BINARIES if not (d / b).is_file()]
    if missing:
        raise RuntimeError("cargo build finished but still missing: %s" % ", ".join(missing))
    return d


class Stack:
    """One booted ds-vdev + ds-daemon pair on a fresh temp dir (all sockets under it)."""

    def __init__(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="ds-e2e-"))
        self.vdev_sock = self.tmp / "vdev.sock"
        self.data = self.tmp / "data"
        self.sock = self.data / "daemon.sock"
        self.token = ""
        self.vdev = None
        self.daemon_pid = None
        self.bins = None
        self.vdev_log = open(self.tmp / "vdev.log", "w+")
        self.daemon_log = self.data / "daemon.log"
        self._stopped = False

    def start(self, bins):
        self.bins = bins
        # step 1a: the virtual device (ds-vdev serves directly on --socket; no `up` word)
        self.vdev = subprocess.Popen(
            [str(bins / "ds-vdev"), "--socket", str(self.vdev_sock)],
            stdout=self.vdev_log, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL,
            cwd=str(ROOT),
        )
        self._wait_for_file(self.vdev_sock, 10, "ds-vdev device socket")
        # step 1b: the daemon on the same temp dir
        env = dict(os.environ)
        env["DS_DAEMON_BIN"] = str(bins / "ds-daemon")
        p = subprocess.run(
            [str(bins / "ds"), "--socket", str(self.sock), "--data-dir", str(self.data),
             "daemon", "start", "--device", "virtual:%s" % self.vdev_sock,
             "--data-dir", str(self.data)],
            capture_output=True, text=True, timeout=BOOT_TIMEOUT_S, env=env, cwd=str(ROOT),
        )
        if p.returncode != 0:
            raise RuntimeError(
                "ds daemon start failed rc=%d\nstdout: %s\nstderr: %s\ndaemon.log: %s"
                % (p.returncode, p.stdout, p.stderr, self._read(self.daemon_log))
            )
        self._wait_for_file(self.sock, 10, "daemon control socket")
        self.token = (self.data / "token").read_text().strip()
        if len(self.token) < 16:
            raise RuntimeError("token file at %s is too short" % (self.data / "token"))
        pid_file = self.data / "daemon.pid"
        if pid_file.is_file():
            try:
                self.daemon_pid = int(pid_file.read_text().strip())
            except ValueError:
                self.daemon_pid = None

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
        raise RuntimeError("timed out after %ss waiting for %s (%s)" % (timeout_s, what, path))

    def ds(self, *args, timeout=OP_TIMEOUT_S):
        """Run the real `ds` binary against this stack (cwd=ROOT so `--file
        examples/key.png` resolves). Returns CompletedProcess."""
        env = dict(os.environ)
        env["DS_DAEMON_BIN"] = str(self.bins / "ds-daemon")
        return subprocess.run(
            [str(self.bins / "ds"), "--socket", str(self.sock), "--data-dir", str(self.data)]
            + list(args),
            capture_output=True, text=True, timeout=timeout, env=env, cwd=str(ROOT),
            close_fds=False,
        )

    def vdevctl(self, *args, timeout=OP_TIMEOUT_S):
        return subprocess.run(
            [str(self.bins / "ds-vdevctl"), "--socket", str(self.vdev_sock)] + list(args),
            capture_output=True, text=True, timeout=timeout, cwd=str(ROOT),
        )

    def stop(self):
        """Mandatory cleanup (idempotent): stop the daemon, SIGTERM then SIGKILL both
        processes, remove the temp dir. Returns a list of problems (empty = clean)."""
        if self._stopped:
            return []
        self._stopped = True
        problems = []
        try:
            subprocess.run(
                [str(self.bins / "ds"), "--socket", str(self.sock),
                 "--data-dir", str(self.data), "daemon", "stop"],
                capture_output=True, text=True, timeout=OP_TIMEOUT_S,
                env={**os.environ, "DS_DAEMON_BIN": str(self.bins / "ds-daemon")},
                cwd=str(ROOT),
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


# ---------------------------------------------------------------------------
# steps


def step_boot(stack):
    """Step 1: vdev up + daemon started on tmp paths; fresh start is empty at rev 0."""
    assert stack.vdev_sock.exists(), "vdev socket missing: %s" % stack.vdev_sock
    assert stack.sock.exists(), "daemon socket missing: %s" % stack.sock
    p = stack.ds("daemon", "status", "--json")
    assert p.returncode == 0, "daemon status rc=%d stderr=%s" % (p.returncode, p.stderr)
    st = json.loads(p.stdout)
    assert st["running"] is True, "daemon not running: %s" % st
    assert str(st["device"]).startswith("virtual:"), st
    # phase-1 empty-state invariant on the fresh data dir
    p = stack.ds("get", "/controls", "--json")
    assert p.returncode == 0, "get /controls rc=%d stderr=%s" % (p.returncode, p.stderr)
    c = json.loads(p.stdout)
    assert c["value"] == {} and c["rev"] == 0, "fresh start must be empty at rev 0: %s" % c
    return {"device": st["device"], "pid": st["pid"]}


def step_set_get_image(stack):
    """Step 2: set the golden key image from a file; get --json round-trips it, rev up."""
    assert KEY_PNG.is_file(), "golden example missing: %s" % KEY_PNG
    raw = KEY_PNG.read_bytes()
    assert raw[:8] == b"\x89PNG\r\n\x1a\n", "examples/key.png is not a PNG"
    w, h = struct.unpack(">II", raw[16:24])
    assert (w, h) == (72, 72), "examples/key.png must be 72x72, got %dx%d" % (w, h)
    golden = hashlib.sha256(raw).hexdigest()

    # `--file` is confined to /policy/fs/roots (+ /tmp): allow the repo root for the demo
    p = stack.ds("set", "/policy/fs/roots", "--json", json.dumps([str(ROOT)]))
    assert p.returncode == 0, "set /policy/fs/roots rc=%d stderr=%s" % (p.returncode, p.stderr)

    p = stack.ds("get", "/meta", "--json")
    assert p.returncode == 0, p.stderr
    rev_before = json.loads(p.stdout)["value"]["rev"]

    p = stack.ds("set", "/controls/keys/0/image", "--file", "examples/key.png")
    assert p.returncode == 0, (
        "ds set /controls/keys/0/image --file examples/key.png rc=%d stderr=%s"
        % (p.returncode, p.stderr)
    )
    m = re.match(r"^ok rev=(\d+)$", p.stdout.strip())
    assert m, "human set output must be 'ok rev=N', got %r" % p.stdout
    set_rev = int(m.group(1))

    p = stack.ds("get", "/controls/keys/0/image", "--json")
    assert p.returncode == 0, "get rc=%d stderr=%s" % (p.returncode, p.stderr)
    g = json.loads(p.stdout)
    want = {"kind": "file", "path": str(KEY_PNG.resolve())}
    assert g["value"] == want, "round-trip mismatch: got %s want %s" % (g["value"], want)
    assert g["rev"] == set_rev, "get rev %s != set rev %s" % (g.get("rev"), set_rev)
    assert set_rev > rev_before, "rev must increase on set: before=%d set=%d" % (rev_before, set_rev)
    return {"golden_sha256": golden, "bytes": len(raw), "rev": set_rev}


def step_golden_frame_sha(stack, info):
    """Step 3: ds-vdevctl frames key:0 prints the reassembled frame sha256 == the golden
    ENCODED-frame sha (tests/golden/example-key0.sha256).

    DS-030 changed the frame plane: the daemon no longer pushes the file bytes of
    examples/key.png unchanged — it re-encodes the image to the device wire format
    (baseline JPEG, standard Huffman tables, 4:4:4, q90 for key:N; see
    docs/contracts/streaming.md §2 and crates/ds-daemon/src/frames/). ds-vdev reassembles
    the 0x36 chunks into that JPEG, so the sha256 to compare against is the sha of the
    encoder's output for examples/key.png at key:0, checked in as
    tests/golden/example-key0.sha256 (regenerate deliberately with
    `DS_GOLDEN_REGEN=1 cargo test -p ds-daemon frames::golden` — see tests/golden/README.md).
    The frame byte count is likewise the encoded frame's, not the source file's.
    """
    assert EXAMPLE_KEY0_SHA.is_file(), "golden encoded-frame sha missing: %s" % EXAMPLE_KEY0_SHA
    golden = EXAMPLE_KEY0_SHA.read_text().strip()
    assert re.fullmatch(r"[0-9a-f]{64}", golden), "bad golden sha file: %r" % golden
    p = stack.vdevctl("frames", "key:0")
    assert p.returncode == 0, (
        "ds-vdevctl frames key:0 rc=%d stderr=%s — no reassembled frame for key:0. "
        "Expected the daemon's device task to re-encode examples/key.png to the device "
        "JPEG and push it as key-image chunks (crates/ds-daemon/src/frames/ + device.rs); "
        "ds-vdev only stores frames reassembled from 0x35/0x36/0x45 image-chunk reports). "
        "expected sha256=%s" % (p.returncode, p.stderr.strip(), golden)
    )
    line = p.stdout.strip().splitlines()[-1] if p.stdout.strip() else ""
    m = re.match(r"^key:0 ([0-9a-f]{64}) (\d+)B seq=(\d+)$", line)
    assert m, "frames key:0 line must be 'key:0 <sha256> <n>B seq=<k>', got %r" % line
    got_sha, nbytes, seq = m.group(1), int(m.group(2)), int(m.group(3))
    assert got_sha == golden, (
        "golden encoded-frame sha mismatch: ds-vdevctl frames key:0 = %s but the DS-030 "
        "golden sha of encode(examples/key.png, key:0) = %s (%d B, seq=%d; see %s)"
        % (got_sha, golden, nbytes, seq, EXAMPLE_KEY0_SHA)
    )
    assert nbytes > 0, "the reassembled frame must not be empty"
    return {"frame_sha256": got_sha, "bytes": nbytes, "seq": seq}


def step_watch(stack):
    """Step 4: watch --ndjson --max-events 1 sees the set's event; watch --json exits 1."""
    watch = subprocess.Popen(
        [str(stack.bins / "ds"), "--socket", str(stack.sock), "--data-dir", str(stack.data),
         "watch", "/controls", "--ndjson", "--max-events", "1"],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        env={**os.environ, "DS_DAEMON_BIN": str(stack.bins / "ds-daemon")},
        cwd=str(ROOT),
    )
    try:
        time.sleep(1.0)  # bounded settle wait for the subscription to reach the daemon
        assert watch.poll() is None, "watch exited before any set (rc=%s)" % watch.returncode
        p = stack.ds("set", "/controls/keys/3/label", '"watched"')
        assert p.returncode == 0, "set rc=%d stderr=%s" % (p.returncode, p.stderr)
        try:
            out, err = watch.communicate(timeout=WATCH_TIMEOUT_S)
        except subprocess.TimeoutExpired:
            watch.kill()
            out, err = watch.communicate()
            raise AssertionError("watch --max-events 1 did not exit after the set (timeout)")
        assert watch.returncode == 0, "watch rc=%d stderr=%s" % (watch.returncode, err)
        lines = [l for l in out.splitlines() if l.strip()]
        assert len(lines) == 1, "expected exactly 1 NDJSON event line, got %r" % out
        ev = json.loads(lines[0])
        assert ev.get("op") == "event", ev
        assert ev.get("path") == "/controls/keys/3/label", ev
        assert ev.get("value") == "watched", ev
        assert isinstance(ev.get("rev"), int) and ev["rev"] >= 1, ev
    finally:
        if watch.poll() is None:
            watch.kill()
            try:
                watch.communicate(timeout=KILL_TIMEOUT_S)
            except subprocess.TimeoutExpired:
                pass

    p = stack.ds("watch", "/controls", "--json")
    assert p.returncode == 1, "watch --json must exit 1, rc=%d stdout=%r" % (p.returncode, p.stdout)
    assert "--json is not valid for streaming verbs" in p.stderr, p.stderr
    return {"event_path": "/controls/keys/3/label", "watch_json_rc": p.returncode}


def step_no_migrate(stack):
    """Step 5: `ds migrate` does not exist — every form exits 1 (scope invariant)."""
    for args in (["migrate"], ["migrate", "anything"], ["migrate", "/tmp/old"]):
        p = stack.ds(*args)
        assert p.returncode == 1, "%s must exit 1, rc=%d" % (" ".join(args), p.returncode)
        assert "no 'migrate' verb" in p.stderr, "%s stderr=%s" % (" ".join(args), p.stderr)
    return {"migrate_rc": 1}


def _percentile(sorted_ts, p):
    k = max(1, math.ceil(p / 100.0 * len(sorted_ts)))
    return sorted_ts[k - 1]


def step_bench(stack):
    """Step 6: query-under-load bench skeleton.

    A background writer loops colour sets on another key while N=200 sequential
    `ds get /controls/keys/0` calls are timed (after a warmup of 50 calls). p50/p95/p99
    are printed; the step fails if p99 > 10 ms or if the load part exceeds 10 s.
    """
    # /controls/keys/0 must be readable (step 2 created it)
    p = stack.ds("get", "/controls/keys/0")
    assert p.returncode == 0, "get /controls/keys/0 rc=%d stderr=%s" % (p.returncode, p.stderr)

    stop = threading.Event()
    bench_start = time.monotonic()
    writer_errors = []

    def writer():
        i = 0
        while not stop.is_set() and time.monotonic() - bench_start < BENCH_MAX_S:
            try:
                p = stack.ds("set", "/controls/keys/2/image", "--json",
                             '{"kind":"color","value":"#%06x"}' % (i * 997 % 0xFFFFFF),
                             timeout=OP_TIMEOUT_S)
            except subprocess.TimeoutExpired:
                writer_errors.append("writer set timed out")
                break
            if p.returncode != 0:
                writer_errors.append("writer set rc=%d stderr=%s" % (p.returncode, p.stderr))
                break
            i += 1

    # warmup (untimed)
    for _ in range(BENCH_WARMUP):
        p = stack.ds("get", "/controls/keys/0")
        assert p.returncode == 0, "warmup get rc=%d stderr=%s" % (p.returncode, p.stderr)

    w = threading.Thread(target=writer, daemon=True)
    w.start()
    ts = []
    try:
        for _ in range(BENCH_N):
            t0 = time.perf_counter()
            p = stack.ds("get", "/controls/keys/0")
            t1 = time.perf_counter()
            assert p.returncode == 0, "bench get rc=%d stderr=%s" % (p.returncode, p.stderr)
            ts.append((t1 - t0) * 1000.0)
            if time.monotonic() - bench_start > BENCH_MAX_S:
                break
    finally:
        stop.set()
        w.join(timeout=KILL_TIMEOUT_S)
    wall_s = time.monotonic() - bench_start
    assert not writer_errors, "background writer failed: %s" % writer_errors
    assert len(ts) >= BENCH_N, "bench got only %d/%d gets" % (len(ts), BENCH_N)
    assert wall_s <= BENCH_MAX_S, "bench load part took %.2f s > %.0f s bound" % (wall_s, BENCH_MAX_S)
    ts.sort()
    p50 = _percentile(ts, 50)
    p95 = _percentile(ts, 95)
    p99 = _percentile(ts, 99)
    print("bench: n=%d warmup=%d wall=%.0f ms  p50=%.3f ms  p95=%.3f ms  p99=%.3f ms  "
          "max=%.3f ms  (limit: p99 <= %.1f ms)"
          % (len(ts), BENCH_WARMUP, wall_s * 1000.0, p50, p95, p99, ts[-1], BENCH_P99_LIMIT_MS))
    assert p99 <= BENCH_P99_LIMIT_MS, (
        "query-under-load p99 = %.3f ms exceeds the %.1f ms limit (p50=%.3f p95=%.3f n=%d)"
        % (p99, BENCH_P99_LIMIT_MS, p50, p95, len(ts))
    )
    return {"n": len(ts), "p50_ms": p50, "p95_ms": p95, "p99_ms": p99,
            "max_ms": ts[-1], "wall_s": wall_s}


STEPS = [
    ("1 boot vdev+daemon (empty state, rev 0)", step_boot, False),
    ("2 set --file key.png; get --json round-trips, rev up", step_set_get_image, False),
    ("3 ds-vdevctl frames key:0 == golden encoded-frame sha (tests/golden/example-key0.sha256)", step_golden_frame_sha, True),
    ("4 watch --ndjson --max-events 1; watch --json exits 1", step_watch, False),
    ("5 ds migrate exits 1", step_no_migrate, False),
]


def _alarm(signum, frame):
    raise TimeoutError("e2e suite exceeded the %ss watchdog" % SUITE_TIMEOUT_S)


def main():
    stack = None
    failures = []
    bench = None
    try:
        bins = ensure_binaries()  # bounded build happens before the watchdog is armed
        signal.signal(signal.SIGALRM, _alarm)
        signal.alarm(SUITE_TIMEOUT_S)
        stack = Stack()
        stack.start(bins)
        info = {}
        for name, fn, needs_info in STEPS:
            try:
                if needs_info:
                    info.update(fn(stack, info))
                else:
                    info.update(fn(stack))
                print("PASS  step %s" % name)
            except AssertionError as e:
                failures.append((name, str(e)))
                print("FAIL  step %s: %s" % (name, e))
            except Exception as e:  # noqa: BLE001 - report and continue with cleanup
                failures.append((name, "%s: %s" % (type(e).__name__, e)))
                print("ERROR step %s: %s: %s" % (name, type(e).__name__, e))
        if not NO_BENCH:
            try:
                bench = step_bench(stack)
                print("PASS  step 6 bench under load (p99 %.3f ms <= %.1f ms)"
                      % (bench["p99_ms"], BENCH_P99_LIMIT_MS))
            except AssertionError as e:
                failures.append(("6 bench under load", str(e)))
                print("FAIL  step 6 bench under load: %s" % e)
            except Exception as e:  # noqa: BLE001
                failures.append(("6 bench under load", "%s: %s" % (type(e).__name__, e)))
                print("ERROR step 6 bench under load: %s: %s" % (type(e).__name__, e))
        else:
            print("SKIP  step 6 bench (--no-bench)")
    except Exception as e:  # noqa: BLE001 - boot/build failure
        failures.append(("boot", "%s: %s" % (type(e).__name__, e)))
        print("ERROR boot: %s: %s" % (type(e).__name__, e))
    finally:
        signal.alarm(0)
        if stack is not None:
            if VERBOSE:
                stack.dump_logs()  # before the temp dir is removed
            problems = stack.stop()
            if problems:
                failures.append(("cleanup", "; ".join(problems)))
                print("FAIL  cleanup: %s" % "; ".join(problems), file=sys.stderr)
            else:
                print("cleanup ok: daemon+vdev dead (SIGTERM then SIGKILL), temp dir removed, "
                      "no leaked sockets")
    print("-" * 72)
    if bench:
        print("bench numbers: n=%d warmup=%d p50=%.3f ms p95=%.3f ms p99=%.3f ms max=%.3f ms "
              "wall=%.2f s (p99 limit %.1f ms)"
              % (bench["n"], BENCH_WARMUP, bench["p50_ms"], bench["p95_ms"], bench["p99_ms"],
                 bench["max_ms"], bench["wall_s"], BENCH_P99_LIMIT_MS))
    if failures:
        print("RESULT: FAIL (%d failing step(s))" % len(failures))
        for name, msg in failures:
            print("  - %s: %s" % (name, msg))
        return 1
    print("RESULT: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
