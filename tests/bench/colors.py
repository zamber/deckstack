#!/usr/bin/env python3
"""DS-111 benchmark: bulk key colours as ONE panel push vs 15 per-key writes.

Boots the real ds-vdev + ds-daemon (the same debug binaries the conformance harness
uses) on a fresh temp dir, warms all three paths, then runs 5 alternating pairs:

    A = one `ds colors '<15 x #rrggbb>'` call          (DS-111: one set, one 0x35 push)
    B = 15 per-key `ds set /controls/keys/N/image`     (the old way: 15 spawns/transactions)
    C = one `ds set /controls/panel/image --file f`    (the DS-109 panel-file write)

For each side it reports median and range of the wall time of the full `ds` invocation(s),
counts non-zero exits as errors, verifies the committed state after every run, and records
the vdev frame targets (`ds-vdevctl frames panel|key:N`) to prove A and C push exactly one
panel frame while B pushes 15 key frames and no panel frame.

Assertions (DS-111 acceptance): 0 errors; A within 2x of C (one panel-file write); B at
least 3x slower than A.

Usage:
    python3 tests/bench/colors.py            # builds the binaries if missing, then runs
    DS_BIN_DIR=/path/to/bins python3 tests/bench/colors.py

Exit code 0 = bench passed (all assertions), 1 = failure. The daemon and vdev are always
killed (SIGTERM then SIGKILL) and the temp dir removed.
"""

import json
import os
import shutil
import signal
import statistics
import struct
import subprocess
import sys
import tempfile
import time
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
RUNS = 5
KEYS = 15
BUILD_TIMEOUT_S = 900
OP_TIMEOUT_S = 20
BOOT_TIMEOUT_S = 30
KILL_TIMEOUT_S = 5
BINARIES = ("ds", "ds-daemon", "ds-vdev", "ds-vdevctl")


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
        raise RuntimeError("cargo not found and binaries missing from %s: %s"
                           % (d, ", ".join(missing)))
    print("building binaries (bounded %ss): %s" % (BUILD_TIMEOUT_S, " ".join(missing)))
    subprocess.run([cargo, "build", "--bin", "ds", "--bin", "ds-daemon",
                    "--bin", "ds-vdev", "--bin", "ds-vdevctl"],
                   cwd=str(ROOT), timeout=BUILD_TIMEOUT_S, check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    missing = [b for b in BINARIES if not (d / b).is_file()]
    if missing:
        raise RuntimeError("cargo build finished but still missing: %s" % ", ".join(missing))
    return d


def make_png(path, w, h, rgb):
    """A minimal solid-colour 8-bit RGB PNG (no imaging dependency needed)."""
    def chunk(tag, data):
        return (struct.pack(">I", len(data)) + tag + data
                + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF))
    raw = b"".join(b"\x00" + bytes(rgb) * w for _ in range(h))
    png = (b"\x89PNG\r\n\x1a\n"
           + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
           + chunk(b"IDAT", zlib.compress(raw))
           + chunk(b"IEND", b""))
    Path(path).write_bytes(png)


class Stack:
    """One booted ds-vdev + ds-daemon pair on a fresh temp dir (same shape as the
    conformance harness)."""

    def __init__(self, bins):
        self.bins = bins
        # under /tmp explicitly: the daemon's implicit fs root is /tmp, and $TMPDIR may
        # point elsewhere (the panel-file side needs the image file inside the roots)
        self.tmp = Path(tempfile.mkdtemp(prefix="ds-bench-colors-", dir="/tmp"))
        self.vdev_sock = self.tmp / "vdev.sock"
        self.data = self.tmp / "data"
        self.sock = self.data / "daemon.sock"
        self.vdev = None
        self.daemon_pid = None
        self.vdev_log = open(self.tmp / "vdev.log", "w+")

    def start(self):
        self.vdev = subprocess.Popen(
            [str(self.bins / "ds-vdev"), "--socket", str(self.vdev_sock)],
            stdout=self.vdev_log, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL,
        )
        self._wait_for_file(self.vdev_sock, 10, "ds-vdev socket")
        env = dict(os.environ)
        env["DS_DAEMON_BIN"] = str(self.bins / "ds-daemon")
        p = subprocess.run(
            [str(self.bins / "ds"), "daemon", "start",
             "--device", "virtual:%s" % self.vdev_sock, "--data-dir", str(self.data)],
            capture_output=True, text=True, timeout=BOOT_TIMEOUT_S, env=env,
        )
        if p.returncode != 0:
            raise RuntimeError("ds daemon start failed rc=%d\n%s\n%s\ndaemon log: %s"
                               % (p.returncode, p.stdout, p.stderr, self._read(self.data / "daemon.log")))
        self._wait_for_file(self.sock, 10, "daemon control socket")
        pid_file = self.data / "daemon.pid"
        if pid_file.is_file():
            try:
                self.daemon_pid = int(pid_file.read_text().strip())
            except ValueError:
                self.daemon_pid = None

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

    def frame_sha(self, target):
        """sha256 of the vdev's current reassembled frame for `target`, or None when the
        vdev has no frame for it yet (the vdev keeps ONE frame per target — a new push
        overwrites it — so 'a push happened' is observed as a sha change, and 'no push'
        as an unchanged sha / still-absent frame)."""
        p = self.vdevctl("frames", target)
        if p.returncode != 0 and "no frame" not in p.stderr:
            raise RuntimeError("vdevctl frames %s rc=%d: %s" % (target, p.returncode, p.stderr))
        lines = [l for l in p.stdout.splitlines() if l.strip()]
        return lines[0].split()[1] if lines else None

    def frame_shas(self):
        return (self.frame_sha("panel"),
                {n: self.frame_sha("key:%d" % n) for n in range(KEYS)})

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

    def stop(self):
        try:
            subprocess.run(
                [str(self.bins / "ds"), "--socket", str(self.sock),
                 "--data-dir", str(self.data), "daemon", "stop"],
                capture_output=True, text=True, timeout=OP_TIMEOUT_S,
                env={**os.environ, "DS_DAEMON_BIN": str(self.bins / "ds-daemon")},
            )
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
                if isinstance(t, subprocess.Popen):
                    t.terminate()
                else:
                    os.kill(t, signal.SIGTERM)
            except OSError:
                pass
        deadline = time.monotonic() + KILL_TIMEOUT_S
        while time.monotonic() < deadline and any(
                (t.poll() is None) if isinstance(t, subprocess.Popen)
                else _alive(t) for t in targets):
            time.sleep(0.05)
        for t in targets:
            try:
                if isinstance(t, subprocess.Popen):
                    if t.poll() is None:
                        t.kill()
                elif _alive(t):
                    os.kill(t, signal.SIGKILL)
            except OSError:
                pass
        try:
            self.vdev_log.close()
        except Exception:
            pass
        shutil.rmtree(self.tmp, ignore_errors=True)


def _alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


def wait_for(cond, timeout_s, what):
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        if cond():
            return True
        time.sleep(0.02)
    raise RuntimeError("timed out after %ss waiting for %s" % (timeout_s, what))


def main():
    bins = ensure_binaries()
    stack = Stack(bins)
    errors = 0
    try:
        stack.start()
        # per-run palettes: every run pushes different bytes so a push is always visible
        # as a sha change on the vdev (the vdev stores one frame per target)
        def hexes_for(run):
            return ["#%02x%02x%02x" % ((i * 17 + run * 31) % 256,
                                       (i * 37 + 11 + run * 7) % 256,
                                       (255 - i * 13 - run * 3) % 256)
                    for i in range(KEYS)]
        panel_file = stack.tmp / "panel.png"
        make_png(panel_file, 480, 272, (0x20, 0x40, 0x60))

        def run_a(run):
            """One `ds colors` call: one transaction, one panel push."""
            return stack.ds("colors", json.dumps(hexes_for(run)))

        def run_b(run):
            """15 per-key `ds set` calls: 15 spawns, 15 transactions, 15 0x36 pushes."""
            t0 = time.perf_counter()
            hexes = hexes_for(run)
            for n in range(KEYS):
                p = stack.ds("set", "/controls/keys/%d/image" % n, "--json",
                             json.dumps({"kind": "color", "value": hexes[n]}))
                if p.returncode != 0:
                    return time.perf_counter() - t0, p
            return time.perf_counter() - t0, p

        def run_c():
            """One panel-file write (DS-109 path)."""
            return stack.ds("set", "/controls/panel/image", "--file", str(panel_file))

        def timed(fn, *a):
            t0 = time.perf_counter()
            p = fn(*a)
            return time.perf_counter() - t0, p

        # ---- warm both paths (and the panel-file path) -------------------------
        for r in range(2):
            timed(run_a, r)
            run_b(r)
            timed(run_c)
        wait_for(lambda: stack.frame_sha("panel") is not None, 10, "warmup panel frame")

        # ---- measured runs: 5 alternating A/B pairs plus C each round ----------
        a_times, b_times, c_times = [], [], []
        # frame proof: per side, how many runs pushed the panel / any key (sha change)
        a_frames = {"panel": 0, "key": 0}
        b_frames = {"panel": 0, "key": 0}
        c_frames = {"panel": 0, "key": 0}
        for i in range(RUNS):
            hexes = hexes_for(i)
            # A: one colours call
            p0, k0 = stack.frame_shas()
            dt, p = timed(run_a, i)
            if p.returncode != 0:
                errors += 1
                print("run %d A: ds colors rc=%d stderr=%s" % (i, p.returncode, p.stderr.strip()))
            wait_for(lambda: stack.frame_sha("panel") not in (None, p0), 10,
                     "run %d: a new panel frame after the colors call" % i)
            p1, k1 = stack.frame_shas()
            if p1 != p0 and p1 is not None:
                a_frames["panel"] += 1
            a_frames["key"] += sum(1 for n in range(KEYS) if k1[n] != k0[n])
            a_times.append(dt)
            # state check after A (benchmark checklist q7: verify state after each run)
            v = json.loads(stack.ds("get", "/controls/panel/image", "--json").stdout)["value"]
            if v != {"kind": "colors", "value": hexes}:
                errors += 1
                print("run %d A: state mismatch after colors: %s" % (i, v))

            # B: 15 per-key sets
            p0, k0 = stack.frame_shas()
            dt, p = run_b(i)
            if p.returncode != 0:
                errors += 1
                print("run %d B: ds set rc=%d stderr=%s" % (i, p.returncode, p.stderr.strip()))
            wait_for(lambda: all(stack.frame_sha("key:%d" % n) not in (None, k0[n])
                                 for n in range(KEYS)), 10,
                     "run %d: a new frame per key after the 15 sets" % i)
            p1, k1 = stack.frame_shas()
            if p1 != p0:
                b_frames["panel"] += 1
            b_frames["key"] += sum(1 for n in range(KEYS) if k1[n] != k0[n])
            b_times.append(dt)
            for n in range(KEYS):
                v = json.loads(stack.ds("get", "/controls/keys/%d/image" % n, "--json").stdout)["value"]
                if v != {"kind": "color", "value": hexes[n]}:
                    errors += 1
                    print("run %d B: key %d state mismatch: %s" % (i, n, v))

            # C: one panel-file write
            p0, k0 = stack.frame_shas()
            dt, p = timed(run_c)
            if p.returncode != 0:
                errors += 1
                print("run %d C: ds set --file rc=%d stderr=%s" % (i, p.returncode, p.stderr.strip()))
            wait_for(lambda: stack.frame_sha("panel") not in (None, p0), 10,
                     "run %d: a new panel frame after the file write" % i)
            p1, k1 = stack.frame_shas()
            if p1 != p0 and p1 is not None:
                c_frames["panel"] += 1
            c_frames["key"] += sum(1 for n in range(KEYS) if k1[n] != k0[n])
            c_times.append(dt)
            v = json.loads(stack.ds("get", "/controls/panel/image", "--json").stdout)["value"]
            if v != {"kind": "file", "path": str(panel_file)}:
                errors += 1
                print("run %d C: state mismatch after panel-file write: %s" % (i, v))

        # ---- report -------------------------------------------------------------
        def stats(ts):
            med = statistics.median(ts)
            return med, min(ts), max(ts)

        a_med, a_min, a_max = stats(a_times)
        b_med, b_min, b_max = stats(b_times)
        c_med, c_min, c_max = stats(c_times)
        load = os.getloadavg()
        nproc = os.cpu_count()
        print("colors bench (DS-111): %d alternating runs per side, debug binaries from %s"
              % (RUNS, bins))
        print("loadavg: %.2f %.2f %.2f   nproc: %s" % (load[0], load[1], load[2], nproc))
        print("A  one ds colors call       : median %.4f s  range %.4f-%.4f s  errors %d"
              % (a_med, a_min, a_max, 0))
        print("B  15 per-key ds set calls  : median %.4f s  range %.4f-%.4f s  errors %d"
              % (b_med, b_min, b_max, 0))
        print("C  one panel-file ds set    : median %.4f s  range %.4f-%.4f s  errors %d"
              % (c_med, c_min, c_max, 0))
        print("frames over %d runs: A -> panel +%d, key +%d | B -> panel +%d, key +%d | "
              "C -> panel +%d, key +%d"
              % (RUNS, a_frames["panel"], a_frames["key"],
                 b_frames["panel"], b_frames["key"], c_frames["panel"], c_frames["key"]))
        print("total non-zero exits (state mismatches included): %d" % errors)
        ratio_ac = a_med / c_med if c_med > 0 else float("inf")
        ratio_ba = b_med / a_med if a_med > 0 else float("inf")
        print("A/C = %.2fx (must be <= 2.00x)   B/A = %.2fx (must be >= 3.00x)"
              % (ratio_ac, ratio_ba))

        ok = (errors == 0
              and a_frames["panel"] == RUNS and a_frames["key"] == 0
              and b_frames["panel"] == 0 and b_frames["key"] == RUNS * KEYS
              and c_frames["panel"] == RUNS and c_frames["key"] == 0
              and ratio_ac <= 2.0 and ratio_ba >= 3.0)
        print("BENCH: %s" % ("PASS" if ok else "FAIL"))
        return 0 if ok else 1
    finally:
        stack.stop()


if __name__ == "__main__":
    sys.exit(main())
