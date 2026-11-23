#!/usr/bin/env python3
"""deckstack STREAMING conformance suite (ticket DS-034, tests/conformance/streaming/).

Black-box machine-checks of the animation/GIF pipeline through the REAL `ds` binary on
a booted ds-vdev + ds-daemon stack, with a test-side DSF1 stream plane on
`<data>/stream.sock` (the phase-1 daemon does not bind it yet — see harness.py) and the
`ds-vdevctl frames` reassembly oracle on the device side:

    010 animated GIF via `ds stream push dial --file anim.gif --loop`: accepted, loop
        meta preserved on the wire, frame reassembled on the vdev, rev unchanged (S1)
    020 `--dir --fps` animation: frame count, per-frame pacing (first frame immediate,
        gaps >= 1/fps), DSF1 meta (frames/fps/seq/ts_ms), final vdev frame == last frame,
        S1 rev delta 0
    030 the final reassembled frame on the vdev matches the ENCODER OUTPUT of the last
        frame (golden tests/golden/example-key0.sha256, forwarded through the daemon's
        own encoder in encode mode)
    040 a 301-frame directory is refused (exit 1, "capped at 300"), nothing pushed; the
        300-frame boundary is accepted
    050 --fps 40 / --fps 0 / missing --fps are refused (exit 1)
    060 a > 16 MB --file (PNG or GIF magic) is refused with E_LIMIT (exit 5) before any
        frame is pushed
    070 --loop: the loop flag is carried on every frame's meta; repeating the push
        replays the animation once more with the identical final frame
    080 per-target queues: key:4 / key:5 / dial each get their own frame; key:15 refused
    090 the raw stream plane itself: rate limit (min 0.02 s per target) nacks E_LIMIT,
        an over-16 MB declared payload nacks E_LIMIT and closes the connection

The Rust side of the animation plane (multi-frame decode, limits, pacing, queue push) is
machine-checked by `cargo test -p ds-daemon frames::animate` (crates/ds-daemon/src/frames/
animate.rs).

Usage:
    python3 tests/conformance/streaming/run.py [--verbose] [-k SUBSTR ...]

Exit code 0 = all green. Every wait carries a timeout; the daemon, vdev and bridge are
always stopped and the temp dir removed, so the run can never hang or leak processes.
"""

import json
import os
import re
import socket
import struct
import sys
import time
import unittest
import zlib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from harness import (  # noqa: E402
    DSCTL_MAGIC,
    DSF1_MAGIC,
    MAX_ANIMATION_FRAMES,
    MAX_FPS,
    MAX_PAYLOAD_BYTES,
    MIN_FRAME_INTERVAL_S,
    ROOT,
    Stack,
    ensure_binaries,
    make_gif,
    make_png,
    solid_rgb,
)

SUITE_TIMEOUT_S = 600
VERBOSE = "--verbose" in sys.argv

GOLDEN_KEY0 = ROOT / "tests" / "golden" / "example-key0.sha256"
EXAMPLE_KEY_PNG = ROOT / "examples" / "key.png"


def log(msg):
    if VERBOSE:
        print(msg, file=sys.stderr)


def wait_until(cond, timeout_s, what):
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        if cond():
            return
        time.sleep(0.05)
    raise AssertionError("timed out after %ss waiting for %s" % (timeout_s, what))


def dsf1_frame(meta, payload):
    meta_bytes = json.dumps(meta).encode()
    return (DSF1_MAGIC + struct.pack("<I", len(meta_bytes)) + meta_bytes
            + struct.pack("<Q", len(payload)) + payload)


def dsctl_line(obj):
    return DSCTL_MAGIC + json.dumps(obj).encode() + b"\n"


class Streaming(unittest.TestCase):
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
                cls.stack.vdev_log.flush()
                print(cls.stack._read(cls.stack.vdev_log.name), file=sys.stderr)

    @classmethod
    def tearDownClass(cls):
        if cls.stack is not None:
            problems = cls.stack.stop()
            if problems:
                print("CLEANUP PROBLEMS: %s" % "; ".join(problems), file=sys.stderr)

    def setUp(self):
        if self.boot_error:
            self.fail("streaming stack did not boot: %s" % self.boot_error)
        self.stack.bridge.reset_records()
        self.stack.bridge.mode = "raw"

    def jout(self, p):
        self.assertEqual(p.returncode, 0, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        return json.loads(p.stdout)

    def tmpdir(self):
        import tempfile
        return tempfile.TemporaryDirectory(prefix="ds-streaming-case-")

    # ---- 010 GIF --file --loop on the dial ---------------------------------

    def test_streaming_010_gif_file_push_with_loop_on_dial(self):
        st = self.stack
        gif = make_gif([(0xFF, 0, 0), (0, 0xFF, 0), (0, 0, 0xFF)], [5, 5, 10], 8, 8, loop=True)
        self.assertTrue(gif.startswith(b"GIF89a"))
        with self.tmpdir() as td:
            gif_path = Path(td) / "anim.gif"
            gif_path.write_bytes(gif)
            rev_before = st.rev()
            p = st.ds(["stream", "push", "dial", "--file", str(gif_path), "--loop", "--json"])
            out = self.jout(p)
            self.assertEqual(out["ok"], True)
            self.assertEqual(out["target"], "dial")
            self.assertEqual(out["sent"], 1)
            self.assertEqual(out["frames"], 1)
            self.assertEqual(out["loop"], True)
        frames = st.bridge.accepted()
        self.assertEqual(len(frames), 1, "the GIF is one DSF1 frame on the wire")
        m = frames[0]
        self.assertEqual(m["target"], "dial")
        self.assertEqual(m["format"], "gif")
        self.assertEqual(m["loop"], True, "loop flag preserved in the DSF1 meta")
        self.assertEqual(m["len"], len(gif))
        # reassembled on the vdev (raw forward) and no rev churn (S1)
        wait_until(lambda: st.vdev_frame_sha("dial") is not None, 10, "dial frame on vdev")
        self.assertEqual(st.vdev_frame_sha("dial"), __import__("hashlib").sha256(gif).hexdigest())
        self.assertEqual(st.rev(), rev_before, "a frame push never bumps rev (S1)")

    # ---- 020 --dir --fps: count, pacing, meta, final frame -------------------

    def test_streaming_020_dir_animation_pacing_count_and_final_frame(self):
        st = self.stack
        target = "key:0"
        n = 4
        fps = 5  # 200 ms interval: easy to assert first-immediate + pacing
        payloads = []
        with self.tmpdir() as td:
            d = Path(td) / "frames"
            d.mkdir()
            for i in range(n):
                rgb = solid_rgb((i * 40, 0x80, 0x20), 8, 8)
                payload = make_png(rgb, 8, 8)
                payloads.append(payload)
                (d / ("%04d.png" % i)).write_bytes(payload)
            rev_before = st.rev()
            start = time.monotonic()
            p = st.ds(["stream", "push", target, "--dir", str(d), "--fps", str(fps), "--json"])
            wall = time.monotonic() - start
            out = self.jout(p)
        self.assertEqual(out["sent"], n)
        self.assertEqual(out["frames"], n)
        self.assertEqual(out["fps"], fps)
        self.assertEqual(out["loop"], False)
        frames = [f for f in st.bridge.accepted() if f["target"] == target]
        self.assertEqual(len(frames), n, "one DSF1 frame per directory file")
        for i, m in enumerate(frames):
            self.assertEqual(m["seq"], i)
            self.assertEqual(m["frames"], n, "frames meta on every frame")
            self.assertEqual(m["fps"], fps, "fps meta on every frame")
            self.assertEqual(m["loop"], None)
            self.assertEqual(m["format"], "png")
            self.assertEqual(m["len"], len(payloads[i]))
        arrivals = [m["at"] for m in frames]
        # pacing: gaps between frames are >= ~1/fps (0.2 s here)
        for a, b in zip(arrivals, arrivals[1:]):
            self.assertGreaterEqual(b - a, 0.18, "frame gap %0.3f s < 1/fps" % (b - a))
        # first frame sent immediately: it arrives in well under one interval after start
        self.assertLess(arrivals[0] - start, 0.15,
                        "first frame must be sent immediately, not after one interval")
        self.assertGreaterEqual(wall, (n - 1) * 0.18, "total wall time honours the fps")
        # ts_ms is paced by the producer as well
        ts = [m["ts_ms"] for m in frames]
        for a, b in zip(ts, ts[1:]):
            self.assertGreaterEqual(b - a, 180, "ts_ms paced at >= 1/fps")
        # the final reassembled frame on the vdev is the LAST frame pushed
        import hashlib
        wait_until(lambda: st.vdev_frame_sha(target) == hashlib.sha256(payloads[-1]).hexdigest(),
                   10, "final frame on vdev")
        self.assertEqual(st.vdev_frame_sha(target), hashlib.sha256(payloads[-1]).hexdigest())
        self.assertEqual(st.rev(), rev_before, "animation pushes never bump rev (S1)")
        # the device-side write log shows the same pacing (image reports are 320 B writes)
        w = st.vdevctl("writes", "2000")
        self.assertEqual(w.returncode, 0, w.stderr)
        ats = [int(m) for m in re.findall(r"len=320 at_ms=(\d+)", w.stdout)]
        self.assertGreaterEqual(len(ats), n)
        self.assertGreaterEqual(max(ats) - min(ats), 500, "vdev saw the frames paced")

    # ---- 030 final frame == encoder output of the last frame ------------------

    def test_streaming_030_final_vdev_frame_matches_encoder_output_of_last_frame(self):
        st = self.stack
        target = "key:0"
        self.assertTrue(EXAMPLE_KEY_PNG.is_file(), "golden example missing")
        self.assertTrue(GOLDEN_KEY0.is_file(), "golden sha missing")
        golden_sha = GOLDEN_KEY0.read_text().strip()
        last_payload = EXAMPLE_KEY_PNG.read_bytes()
        with self.tmpdir() as td:
            d = Path(td) / "frames"
            d.mkdir()
            for i in range(3):
                (d / ("%04d.png" % i)).write_bytes(make_png(solid_rgb((i, i, i), 8, 8), 8, 8))
            (d / "0003.png").write_bytes(last_payload)  # the last frame is the golden input
            st.bridge.mode = "encode"  # forward each frame through the daemon's encoder
            p = st.ds(["stream", "push", target, "--dir", str(d), "--fps", "10", "--json"])
            out = self.jout(p)
            self.assertEqual(out["sent"], 4)
        frames = [f for f in st.bridge.accepted() if f["target"] == target]
        self.assertEqual(len(frames), 4)
        self.assertEqual(frames[-1]["len"], len(last_payload))
        # the vdev's final reassembled frame is the daemon-encoded JPEG of the last frame
        wait_until(lambda: st.vdev_frame_sha(target) == golden_sha, 15,
                   "vdev final frame == golden encoder output")
        self.assertEqual(st.vdev_frame_sha(target), golden_sha,
                         "final reassembled frame must equal tests/golden/example-key0.sha256 "
                         "(the encoder output of the last frame)")

    # ---- 040 frame-count limit ------------------------------------------------

    def test_streaming_040_frame_count_limit_301_refused_300_accepted(self):
        st = self.stack
        target = "key:1"
        with self.tmpdir() as td:
            big = Path(td) / "big"
            big.mkdir()
            for i in range(MAX_ANIMATION_FRAMES + 1):
                (big / ("%04d.png" % i)).write_bytes(make_png(solid_rgb((1, 2, 3), 2, 2), 2, 2))
            p = st.ds(["stream", "push", target, "--dir", str(big), "--fps", "30"])
            self.assertEqual(p.returncode, 1, "301 frames must be a usage error: rc=%d" % p.returncode)
            self.assertIn("capped at %d" % MAX_ANIMATION_FRAMES, p.stderr, p.stderr)
            self.assertEqual([f for f in st.bridge.accepted() if f["target"] == target], [],
                             "a refused animation never reaches the stream plane")
            # the 300-frame boundary is accepted
            ok = Path(td) / "ok"
            ok.mkdir()
            for i in range(MAX_ANIMATION_FRAMES):
                (ok / ("%04d.png" % i)).write_bytes(make_png(solid_rgb((i % 256, 0, 0), 2, 2), 2, 2))
            p = st.ds(["stream", "push", target, "--dir", str(ok), "--fps", "30", "--json"],
                      timeout=120)
            out = self.jout(p)
            self.assertEqual(out["frames"], MAX_ANIMATION_FRAMES)
            self.assertEqual(out["sent"], MAX_ANIMATION_FRAMES)
        frames = [f for f in st.bridge.accepted() if f["target"] == target]
        self.assertEqual(len(frames), MAX_ANIMATION_FRAMES)
        self.assertTrue(all(m["frames"] == MAX_ANIMATION_FRAMES for m in frames))

    # ---- 050 fps limits ---------------------------------------------------------

    def test_streaming_050_fps_limits_refused(self):
        st = self.stack
        with self.tmpdir() as td:
            d = Path(td) / "frames"
            d.mkdir()
            (d / "0000.png").write_bytes(make_png(solid_rgb((9, 9, 9), 4, 4), 4, 4))
            for bad in ("40", "0", "31"):
                p = st.ds(["stream", "push", "key:1", "--dir", str(d), "--fps", bad])
                self.assertEqual(p.returncode, 1, "--fps %s must be refused" % bad)
                self.assertIn("--fps must be", p.stderr, p.stderr)
            p = st.ds(["stream", "push", "key:1", "--dir", str(d)])
            self.assertEqual(p.returncode, 1)
            self.assertIn("needs --fps", p.stderr, p.stderr)
            self.assertEqual(st.bridge.accepted(), [], "nothing pushed on refused input")

    # ---- 060 oversize wire payload ----------------------------------------------

    def test_streaming_060_oversize_file_refused_with_e_limit(self):
        st = self.stack
        with self.tmpdir() as td:
            for magic, name in ((b"\x89PNG\r\n\x1a\n", "big.png"), (b"GIF89a", "big.gif")):
                big = Path(td) / name
                big.write_bytes(magic + b"\x00" * MAX_PAYLOAD_BYTES)  # 16 MB + 1 with magic
                self.assertGreater(big.stat().st_size, MAX_PAYLOAD_BYTES)
                p = st.ds(["stream", "push", "key:2", "--file", str(big)])
                self.assertEqual(p.returncode, 5, "%s: E_LIMIT exit 5, rc=%d stderr=%s"
                                 % (name, p.returncode, p.stderr))
                self.assertIn("16 MB", p.stderr, p.stderr)
        self.assertEqual(st.bridge.accepted(), [], "an oversize frame is refused before the wire")

    # ---- 070 loop repeats once ----------------------------------------------------

    def test_streaming_070_loop_flag_and_repeat_once(self):
        st = self.stack
        target = "key:3"
        n = 3
        payloads = [make_png(solid_rgb((i * 60, 0, 0), 8, 8), 8, 8) for i in range(n)]
        with self.tmpdir() as td:
            d = Path(td) / "frames"
            d.mkdir()
            for i, pl in enumerate(payloads):
                (d / ("%04d.png" % i)).write_bytes(pl)
            p = st.ds(["stream", "push", target, "--dir", str(d), "--fps", "10", "--loop", "--json"])
            out = self.jout(p)
            self.assertEqual(out["loop"], True)
            self.assertEqual(out["sent"], n)
            frames = [f for f in st.bridge.accepted() if f["target"] == target]
            self.assertEqual(len(frames), n)
            self.assertTrue(all(m["loop"] is True for m in frames),
                            "loop:true on every frame's DSF1 meta")
            self.assertTrue(all(m["frames"] == n for m in frames))
            import hashlib
            wait_until(lambda: st.vdev_frame_sha(target) == hashlib.sha256(payloads[-1]).hexdigest(),
                       10, "first pass final frame")
            sha_after_first = st.vdev_frame_sha(target)
            # repeat the push once: the looped animation replays identically
            p = st.ds(["stream", "push", target, "--dir", str(d), "--fps", "10", "--loop", "--json"])
            out = self.jout(p)
            self.assertEqual(out["sent"], n)
        frames = [f for f in st.bridge.accepted() if f["target"] == target]
        self.assertEqual(len(frames), 2 * n, "the repeat is one more full pass")
        seqs = [m["seq"] for m in frames]
        self.assertEqual(seqs, list(range(n)) + list(range(n)), "each pass restarts seq at 0")
        wait_until(lambda: st.vdev_frame_sha(target) == sha_after_first, 10, "repeat final frame")
        self.assertEqual(st.vdev_frame_sha(target), sha_after_first,
                         "the repeated pass lands the identical final frame")

    # ---- 080 per-target queues -----------------------------------------------------

    def test_streaming_080_per_target_queues_and_target_vocabulary(self):
        st = self.stack
        import hashlib
        with self.tmpdir() as td:
            for target, color in (("key:4", (1, 2, 3)), ("key:5", (4, 5, 6)), ("dial", (7, 8, 9))):
                f = Path(td) / ("f_%s.png" % target.replace(":", "_"))
                payload = make_png(solid_rgb(color, 8, 8), 8, 8)
                f.write_bytes(payload)
                p = st.ds(["stream", "push", target, "--file", str(f), "--json"])
                out = self.jout(p)
                self.assertEqual(out["sent"], 1)
                wait_until(lambda t=target: st.vdev_frame_sha(t) == hashlib.sha256(payload).hexdigest(),
                           10, "%s frame on vdev" % target)
                self.assertEqual(st.vdev_frame_sha(target), hashlib.sha256(payload).hexdigest(),
                                 "%s has its own queue and its own frame" % target)
            per_target = {}
            for m in st.bridge.accepted():
                per_target.setdefault(m["target"], 0)
                per_target[m["target"]] += 1
            self.assertEqual(per_target, {"key:4": 1, "key:5": 1, "dial": 1},
                             "each target is queued independently")
            # panel and dial share the same caps: both accepted under the same rules
            p = st.ds(["stream", "push", "keys", "--file", str(Path(td) / "f_dial.png"), "--json"])
            self.jout(p)
            self.assertIsNotNone(st.vdev_frame_sha("panel"))
            # key:15 is not a target (S6)
            p = st.ds(["stream", "push", "key:15", "--file", str(Path(td) / "f_dial.png")])
            self.assertEqual(p.returncode, 1)
            self.assertIn("invalid stream target", p.stderr)

    # ---- 090 raw stream-plane gates -------------------------------------------------

    def test_streaming_090_stream_plane_rate_limit_and_wire_cap_nacks(self):
        st = self.stack
        target = "key:6"
        payload = make_png(solid_rgb((0xAA, 0xBB, 0xCC), 4, 4), 4, 4)
        meta = {"target": target, "seq": 0, "ts_ms": 1, "format": "png",
                "crc32": zlib.crc32(payload) & 0xFFFFFFFF, "max_payload_bytes": MAX_PAYLOAD_BYTES}

        def read_dsctl(s):
            magic = s.recv(6)
            assert magic == DSCTL_MAGIC, magic
            buf = b""
            while not buf.endswith(b"\n"):
                b = s.recv(1)
                if not b:
                    raise AssertionError("stream socket closed while waiting for a DSctl line")
                buf += b
            return json.loads(buf.decode())

        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.settimeout(10)
        s.connect(str(st.stream_sock))
        try:
            s.sendall(dsctl_line({"op": "stream-open", "target": target, "credits": 10}))
            grant = read_dsctl(s)
            self.assertEqual(grant["op"], "stream-credit")
            self.assertEqual(grant["credits"], 10)
            # frame 0: accepted
            m0 = dict(meta, seq=0)
            s.sendall(dsf1_frame(m0, payload))
            ack = read_dsctl(s)
            self.assertEqual(ack["op"], "stream-ack", ack)
            self.assertEqual(ack["seq"], 0)
            # frame 1 within 20 ms of frame 0: rate-limited with E_LIMIT (<= 30 fps)
            m1 = dict(meta, seq=1)
            s.sendall(dsf1_frame(m1, payload))
            nack = read_dsctl(s)
            self.assertEqual(nack["op"], "stream-nack", nack)
            self.assertEqual(nack["seq"], 1)
            self.assertEqual(nack["error"]["code"], "E_LIMIT", nack)
            self.assertIn("0.02", nack["error"]["message"])
            # a frame declaring more than the 16 MB wire cap: E_LIMIT and the connection closes
            big_meta = dict(meta, seq=2, crc32=0)
            wire = DSF1_MAGIC + struct.pack("<I", len(json.dumps(big_meta).encode())) \
                + json.dumps(big_meta).encode() + struct.pack("<Q", MAX_PAYLOAD_BYTES + 1)
            s.sendall(wire)
            nack = read_dsctl(s)
            self.assertEqual(nack["op"], "stream-nack", nack)
            self.assertEqual(nack["error"]["code"], "E_LIMIT", nack)
            self.assertEqual(s.recv(64), b"", "an over-cap declared payload closes the stream")
        finally:
            s.close()
        # only the first frame was accepted (the rate-limited one was nacked, not queued)
        frames = [f for f in st.bridge.accepted() if f["target"] == target]
        self.assertEqual([f["seq"] for f in frames], [0])
        nacks = [n for n in st.bridge.nacks if n["target"] == target]
        self.assertEqual([n["code"] for n in nacks], ["E_LIMIT", "E_LIMIT"])


# ---------------------------------------------------------------------------
# suite plumbing (mirrors tests/conformance/safety/run.py)
# ---------------------------------------------------------------------------


def streaming_suite():
    """The suite object (registered by name pattern `streaming` for `-k streaming`)."""
    loader = unittest.defaultTestLoader
    suite = unittest.TestSuite()
    suite.addTests(loader.loadTestsFromTestCase(Streaming))
    return suite


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


def _filter_suite(suite, patterns):
    out = unittest.TestSuite()
    for t in suite:
        if isinstance(t, unittest.TestSuite):
            sub = _filter_suite(t, patterns)
            if sub.countTestCases():
                out.addTests(sub)
        elif any(p.lower() in t.id().lower() for p in patterns):
            out.addTest(t)
    return out


def _parse_k(argv):
    patterns = []
    i = 1
    while i < len(argv):
        if argv[i] == "-k" and i + 1 < len(argv):
            patterns.append(argv[i + 1])
            i += 2
        else:
            i += 1
    return patterns


def _alarm(signum, frame):
    raise TimeoutError("streaming suite exceeded the %ss watchdog" % SUITE_TIMEOUT_S)


def main(argv):
    import signal
    patterns = _parse_k(argv)
    signal.signal(signal.SIGALRM, _alarm)
    signal.alarm(SUITE_TIMEOUT_S)
    try:
        suite = streaming_suite()
        if patterns:
            suite = _filter_suite(suite, patterns)
        runner = unittest.TextTestRunner(stream=sys.stdout, verbosity=0, resultclass=LineResult)
        result = runner.run(suite)
    finally:
        signal.alarm(0)
    n = result.testsRun
    bad = len(result.failures) + len(result.errors)
    print("-" * 60)
    print("streaming: %d tests, %d passed, %d failed/errored, %d skipped"
          % (n, n - bad - len(result.skipped), bad, len(result.skipped)))
    if bad:
        print("RESULT: FAIL")
        return 1
    print("RESULT: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
