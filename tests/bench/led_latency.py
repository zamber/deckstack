#!/usr/bin/env python3
"""LED latency bench: newest colour must reach the device, no stale backlog.

Boots a private ds-vdev + ds-daemon (the same debug binaries the conformance harness
uses) on a fresh temp dir, then drives a 20 Hz colour feed for 5 s the way the Salon
micro-app does: every frame writes /controls/base/led and /controls/knobs/0..2/led
(the per-index knob paths collapse to one module-wide report). One /controls/rollers/0/led
write happens before the feed: the M032LE has no LED command, so it must be a no-op
warn, never a "no led report" error.

Arrival times come from the vdev write log (`ds-vdevctl writes <n> full`, which adds the
report payload; LED colour reports are `02 2D …` WRITE frames) mapped onto wall clock with
`ds-vdevctl clock` (hub at_ms + epoch_ms). Each frame's colour encodes its index, so every
arrival is matched to the exact issue that produced it.

Printed numbers: reports per frame, delivered frames, lag p50/p95/max of every delivered
colour vs its issue time, the newest colour's lag, effective colour frames/s, and the
"no led report" count in daemon.log.

With --check (the verification command) it asserts and exits nonzero on failure:
  * p95 lag of delivered colours <= 100 ms (newest colour arrives within 100 ms p95)
  * max lag <= 250 ms (no arrival older than 250 ms behind its issue)
  * zero "no led report" errors in daemon.log
  * >= 20 effective colour frames/s

Usage:
    python3 tests/bench/led_latency.py            # build binaries if missing, run, print
    python3 tests/bench/led_latency.py --check    # same + assert, exit 0/1
    DS_BIN_DIR=/path/to/bins python3 tests/bench/led_latency.py --check

The daemon and vdev are always killed (SIGTERM then SIGKILL) and the temp dir removed.
"""

import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
BINARIES = ("ds", "ds-daemon", "ds-vdev", "ds-vdevctl")
BUILD_TIMEOUT_S = 900
OP_TIMEOUT_S = 20
BOOT_TIMEOUT_S = 30

FPS = 20
SECONDS = 5.0
FRAMES = int(FPS * SECONDS)
BASE_PID = 0x01F6
KNOBS_PID = 0x0202
LED_COLOUR_CMD = 0x2D
WRITE_KIND = 0x01

# assertion thresholds (--check)
P95_LAG_MS = 100.0
MAX_LAG_MS = 250.0
MIN_EFFECTIVE_FPS = 20.0
# A 20 Hz feed measured over a finite wall clock can never read above ~20.0 fps, so the
# effective-fps check allows only the pacing jitter of the final frame's issue (~tens of ms
# over a 5 s feed). Anything below that means frames were genuinely coalesced away.
FPS_JITTER_SLACK = 0.5


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


def colour(i):
    """Frame i's unique colour: r = i, g/b fixed (so the payload identifies the frame)."""
    return "#%02x%02x%02x" % (i & 0xFF, 0x40, 0x80)


class Stack:
    def __init__(self, bins):
        self.bins = bins
        self.tmp = Path(tempfile.mkdtemp(prefix="ds-bench-led-", dir="/tmp"))
        self.vdev_sock = self.tmp / "vdev.sock"
        self.data = self.tmp / "data"
        self.sock = self.data / "daemon.sock"
        self.vdev = None
        self.daemon_pid = None
        self.env = dict(os.environ)
        self.env["DS_DAEMON_BIN"] = str(self.bins / "ds-daemon")

    def start(self):
        vdev_log = open(self.tmp / "vdev.log", "w+")
        self.vdev = subprocess.Popen(
            [str(self.bins / "ds-vdev"), "--socket", str(self.vdev_sock)],
            stdout=vdev_log, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL,
        )
        self._wait_for_file(self.vdev_sock, 10, "ds-vdev socket")
        p = subprocess.run(
            [str(self.bins / "ds"), "daemon", "start",
             "--device", "virtual:%s" % self.vdev_sock, "--data-dir", str(self.data)],
            capture_output=True, text=True, timeout=BOOT_TIMEOUT_S, env=self.env,
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
        # wait until the device task reports connected
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            p = self.ds("daemon", "status", "--json")
            if p.returncode == 0 and "connected" in p.stdout:
                return
            time.sleep(0.1)
        raise RuntimeError("daemon never reported a connected device: %s / %s"
                           % (p.stdout, self._read(self.data / "daemon.log")))

    def ds(self, *args, timeout=OP_TIMEOUT_S):
        return subprocess.run(
            [str(self.bins / "ds"), "--socket", str(self.sock), "--data-dir", str(self.data)]
            + list(args),
            capture_output=True, text=True, timeout=timeout, env=self.env,
        )

    def vdevctl(self, *args, timeout=OP_TIMEOUT_S):
        return subprocess.run(
            [str(self.bins / "ds-vdevctl"), "--socket", str(self.vdev_sock)] + list(args),
            capture_output=True, text=True, timeout=timeout,
        )

    def clock(self):
        """hub epoch (wall ms at at_ms=0) from `ds-vdevctl clock`."""
        p = self.vdevctl("clock")
        if p.returncode != 0:
            raise RuntimeError("ds-vdevctl clock rc=%d: %s" % (p.returncode, p.stderr))
        m = re.search(r"at_ms=(\d+) epoch_ms=(\d+)", p.stdout)
        if not m:
            raise RuntimeError("bad clock reply: %r" % p.stdout)
        at_ms, epoch_ms = int(m.group(1)), int(m.group(2))
        return epoch_ms - at_ms

    def writes_full(self):
        p = self.vdevctl("writes", "1000000", "full")
        if p.returncode != 0:
            raise RuntimeError("ds-vdevctl writes rc=%d: %s" % (p.returncode, p.stderr))
        out = []
        for l in p.stdout.splitlines():
            m = re.match(
                r"^#(\d+) pid=(0x[0-9a-f]+) kind=(0x[0-9a-f]+) len=(\d+) at_ms=(\d+)( refused)?"
                r"(?: data=([0-9a-f]*))?$", l)
            if m:
                out.append(dict(seq=int(m.group(1)), pid=int(m.group(2), 16),
                                kind=int(m.group(3), 16), len=int(m.group(4)),
                                at_ms=int(m.group(5)), refused=bool(m.group(6)),
                                data=m.group(7) or ""))
        return out

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
            time.sleep(0.02)
        raise RuntimeError("timed out after %ss waiting for %s (%s)" % (timeout_s, what, path))

    def stop(self):
        try:
            self.ds("daemon", "stop")
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
        for t in targets:
            try:
                if isinstance(t, subprocess.Popen):
                    t.wait(timeout=5)
                else:
                    time.sleep(0.2)
                    os.kill(t, 0)
                    os.kill(t, signal.SIGKILL)
            except (OSError, subprocess.TimeoutExpired):
                pass
        shutil.rmtree(self.tmp, ignore_errors=True)


def percentile(sorted_vals, pct):
    if not sorted_vals:
        return float("nan")
    k = int(pct * (len(sorted_vals) - 1))
    return sorted_vals[k]


def run(check):
    bins = ensure_binaries()
    st = Stack(bins)
    try:
        st.start()
        # one rollers LED write before the feed: must be a no-op warn, not an error
        p = st.ds("set", "/controls/rollers/0/led", "--json",
                  json.dumps({"mode": "static", "color": "#102030", "brightness": 50}))
        assert p.returncode == 0, (p.returncode, p.stdout, p.stderr)

        hub_epoch_ms = st.clock()

        # ---- the 20 Hz colour feed -------------------------------------------
        issues_ms = []  # issue wall time (ms) per frame index
        t_feed0 = time.monotonic()
        for i in range(FRAMES):
            c = colour(i)
            issues_ms.append(time.time() * 1000.0)  # epoch ms, same base as ds-vdevctl clock
            for path in ("/controls/base/led",
                         "/controls/knobs/0/led", "/controls/knobs/1/led",
                         "/controls/knobs/2/led"):
                p = st.ds("set", path, "--json",
                          json.dumps({"mode": "static", "color": c, "brightness": 100}))
                assert p.returncode == 0, (p.returncode, p.stdout, p.stderr, path)
            target = t_feed0 + (i + 1) / FPS
            while time.monotonic() < target:
                time.sleep(0.002)
        feed_wall_s = time.monotonic() - t_feed0

        # let the lane drain, then read the write log
        time.sleep(1.5)
        hub_epoch_ms2 = st.clock()
        writes = st.writes_full()

        # LED colour arrivals: WRITE frames whose payload is `02 2D <group> <idx> r g b save`
        arrivals = []  # (frame, pid, lag_ms, at_ms)
        for w in writes:
            if w["refused"] or w["kind"] != WRITE_KIND or not w["data"]:
                continue
            b = bytes.fromhex(w["data"])
            if len(b) < 8 or b[0] != 0x02 or b[1] != LED_COLOUR_CMD:
                continue
            if w["pid"] not in (BASE_PID, KNOBS_PID):
                continue
            r, g, bl = b[4], b[5], b[6]
            if (g, bl) != (0x40, 0x80):
                continue  # not one of our feed colours (e.g. the rollers probe)
            frame = r
            if frame >= FRAMES:
                continue
            lag_ms = (hub_epoch_ms + w["at_ms"]) - issues_ms[frame]
            arrivals.append((frame, w["pid"], lag_ms, w["at_ms"]))

        base_frames = sorted({f for (f, pid, _, _) in arrivals if pid == BASE_PID})
        knob_frames = sorted({f for (f, pid, _, _) in arrivals if pid == KNOBS_PID})
        delivered_frames = sorted(set(base_frames) | set(knob_frames))
        lags = sorted(l for (_, _, l, _) in arrivals)
        base_reports = sum(1 for (f, pid, _, _) in arrivals if pid == BASE_PID)
        knob_reports = sum(1 for (f, pid, _, _) in arrivals if pid == KNOBS_PID)
        newest_lag = max((l for (f, _, l, _) in arrivals if f == FRAMES - 1), default=float("nan"))
        effective_fps = len(delivered_frames) / feed_wall_s

        log_text = st._read(st.data / "daemon.log")
        no_led_report = log_text.count("no led report")
        rollers_warn = log_text.count("no LED command")

        # ---- print the numbers ------------------------------------------------
        print("feed: %d frames at %d Hz target, wall %.2f s (%.1f Hz issue rate)"
              % (FRAMES, FPS, feed_wall_s, FRAMES / feed_wall_s))
        print("LED colour reports arrived: base=%d knobs=%d (steady state: 1 per frame each; "
              "the 3 per-index knob paths collapse to one module-wide report)"
              % (base_reports, knob_reports))
        print("frames delivered: base=%d/100 knobs=%d/100 union=%d/100"
              % (len(base_frames), len(knob_frames), len(delivered_frames)))
        if lags:
            print("arrival lag vs issue (ms): n=%d p50=%.1f p95=%.1f max=%.1f"
                  % (len(lags), percentile(lags, 0.50), percentile(lags, 0.95), max(lags)))
        print("newest colour (frame %d) lag: %.1f ms" % (FRAMES - 1, newest_lag))
        print("effective colour frames/s: %.1f" % effective_fps)
        print("daemon.log: 'no led report'=%d, rollers no-op warn=%d"
              % (no_led_report, rollers_warn))

        # ---- assertions -------------------------------------------------------
        problems = []
        if not lags:
            problems.append("no LED colour arrivals recorded at all")
        else:
            p95 = percentile(lags, 0.95)
            if p95 > P95_LAG_MS:
                problems.append("p95 lag %.1f ms > %.0f ms" % (p95, P95_LAG_MS))
            if max(lags) > MAX_LAG_MS:
                problems.append("max lag %.1f ms > %.0f ms" % (max(lags), MAX_LAG_MS))
        if no_led_report != 0:
            problems.append("'no led report' errors in daemon.log: %d" % no_led_report)
        if effective_fps < MIN_EFFECTIVE_FPS - FPS_JITTER_SLACK:
            problems.append("effective colour frames/s %.1f < %.0f (minus jitter slack %.1f)"
                            % (effective_fps, MIN_EFFECTIVE_FPS, FPS_JITTER_SLACK))

        if check:
            if problems:
                print("CHECK FAILED:")
                for pr in problems:
                    print("  - " + pr)
                return 1
            print("CHECK PASSED: p95<=%.0f ms, max<=%.0f ms, zero 'no led report', >=%.0f fps"
                  % (P95_LAG_MS, MAX_LAG_MS, MIN_EFFECTIVE_FPS - FPS_JITTER_SLACK))
            return 0
        for pr in problems:
            print("note (no --check): " + pr)
        return 0
    finally:
        st.stop()


def main():
    check = "--check" in sys.argv[1:]
    sys.exit(run(check))


if __name__ == "__main__":
    main()
