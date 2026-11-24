#!/usr/bin/env python3
"""DS-028 golden checks — repeatable camera verification against committed goldens.

Drives the real deck (ds-daemon on hid:) into each golden state declared in
manifest.json, captures a frame with the DS-027 harness, and compares it against
the committed golden over every calibrated ROI. Exit 0 = all goldens match,
7 = at least one mismatch (the harness's E_DEVICE-class exit), 1 = operational
error, 2 = usage.

    python3 tools/verify/golden/run_golden.py                 # check all goldens
    python3 tools/verify/golden/run_golden.py --only key0,led
    python3 tools/verify/golden/run_golden.py --record        # (re)capture goldens + calibration
    python3 tools/verify/golden/run_golden.py --keep /tmp/dsv-frames   # keep the actual frames

Env: DS_BIN_DIR (dir holding ds + ds-daemon; default: first of
  <root>/../target/debug, <root>/../../target/debug that has both),
  DSV_CAMERA (camera node; default /dev/video2, the AUKEY PC-W1).

Each golden runs against a FRESH daemon data dir, so the durable state starts
empty (empty-state invariant): a golden state that only sets key:0 leaves keys
1-14 unset, and the golden frame records that exactly. Goldens are test
fixtures ONLY — nothing here is ever installed as a deck default.

Hardware-gated (phase 2+): needs the deck on hid: and the camera. CI never runs
this — CI runs only `deckstack_verify.py selftest` (see docs/90-verification.md).
"""

import argparse
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

GOLDEN_DIR = Path(__file__).resolve().parent
ROOT = GOLDEN_DIR.parent.parent.parent          # tools/verify/golden -> repo root
VERIFY = GOLDEN_DIR.parent / "deckstack_verify.py"
MANIFEST = GOLDEN_DIR / "manifest.json"

EXIT_OK, EXIT_MISMATCH, EXIT_ERROR, EXIT_USAGE = 0, 7, 1, 2


def eprint(*a):
    print(*a, file=sys.stderr)


def run(argv, timeout=90, env=None):
    return subprocess.run(argv, capture_output=True, text=True, timeout=timeout,
                          stdin=subprocess.DEVNULL, env=env)


def find_bins(explicit):
    cands = []
    if explicit:
        cands.append(Path(explicit))
    if os.environ.get("DS_BIN_DIR"):
        cands.append(Path(os.environ["DS_BIN_DIR"]))
    cands.append(ROOT.parent / "target" / "debug")       # .worktrees/<id>/../target
    cands.append(ROOT.parent.parent / "target" / "debug")  # repo target/debug
    for c in cands:
        if (c / "ds").exists() and (c / "ds-daemon").exists():
            return c
    raise RuntimeError("cannot find ds + ds-daemon binaries (tried: %s); set DS_BIN_DIR"
                       % ", ".join(str(c) for c in cands))


class Deck:
    """One fresh ds-daemon on hid: with its own empty data dir."""

    def __init__(self, bins, tmp):
        self.bins = bins
        self.data = Path(tmp)
        self.sock = self.data / "daemon.sock"
        self.pid = None
        self.env = dict(os.environ)
        self.env["DS_DAEMON_BIN"] = str(bins / "ds-daemon")

    def ds(self, *args, timeout=60):
        return run([str(self.bins / "ds"), "--socket", str(self.sock),
                    "--data-dir", str(self.data)] + list(args), timeout=timeout, env=self.env)

    def start(self, device="hid:", timeout_s=30.0):
        self.data.mkdir(parents=True, exist_ok=True)
        p = run([str(self.bins / "ds"), "daemon", "start", "--device", device,
                 "--data-dir", str(self.data)], timeout=90, env=self.env)
        if p.returncode != 0:
            raise RuntimeError("ds daemon start failed rc=%d: %s%s"
                               % (p.returncode, p.stdout.strip(), p.stderr.strip()))
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            try:
                st = self.ds("daemon", "status", "--json", timeout=10)
                if st.returncode == 0:
                    d = json.loads(st.stdout)
                    if str(d.get("device_status", "")).startswith("connected"):
                        return d
                    if "without hardware" in str(d.get("device_status", "")):
                        raise RuntimeError(
                            "daemon on hid: is running WITHOUT hardware — another daemon "
                            "holds the device (stop it first); status: %s" % d.get("device_status"))
            except (RuntimeError, json.JSONDecodeError):
                pass
            time.sleep(0.25)
        raise RuntimeError("daemon did not reach 'connected' within %.0fs" % timeout_s)

    def stop(self, timeout_s=15.0):
        pid_file = self.data / "daemon.pid"
        pid = None
        if pid_file.is_file():
            try:
                pid = int(pid_file.read_text().strip())
            except ValueError:
                pid = None
        if pid is None:
            return
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            return
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                return
            time.sleep(0.2)
        eprint("warning: daemon pid %d did not exit after SIGTERM" % pid)


def expand_ops(ops, tmp):
    """Expand manifest state ops into concrete ds argv tails (no shell)."""
    out = []
    for op in ops:
        if "foreach" in op:
            fe = op["foreach"]
            var, lo, hi = fe["var"], fe["from"], fe["to"]
            for i in range(lo, hi + 1):
                for tail in expand_ops([{"set": fe["set"]}], tmp):
                    out.append([part.replace("{%s}" % var, str(i)) for part in tail])
        elif "set" in op:
            out.append(["set"] + list(op["set"]))
        elif "set_file" in op:
            path, src = op["set_file"]
            src_path = ROOT / src
            stage = Path(tmp) / "stage"
            stage.mkdir(parents=True, exist_ok=True)
            staged = stage / Path(src).name
            shutil.copyfile(src_path, staged)
            out.append(["set", "/policy/fs/roots", "--json", json.dumps([str(stage)])])
            out.append(["set", path, "--file", str(staged)])
        else:
            raise RuntimeError("unknown manifest op: %r" % (op,))
    return out


def apply_state(deck, ops, tmp):
    for tail in expand_ops(ops, tmp):
        p = deck.ds(*tail)
        if p.returncode != 0:
            raise RuntimeError("ds %s failed rc=%d: %s%s"
                               % (" ".join(tail), p.returncode, p.stdout.strip(), p.stderr.strip()))


def harness(args, timeout=180):
    return run([sys.executable, str(VERIFY)] + args, timeout=timeout)


def load_harness():
    """Import deckstack_verify.py (same directory) for image means; the harness
    itself stays a standalone single-file CLI."""
    import importlib.util
    spec = importlib.util.spec_from_file_location("deckstack_verify", str(VERIFY))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def record_calibration(man, bins, camera, args):
    cal = man["calibration"]
    tmp = tempfile.mkdtemp(prefix="dsv-golden-cal-")
    try:
        deck = Deck(bins, tmp)
        deck.start()
        try:
            apply_state(deck, cal["state"], tmp)
            time.sleep(cal.get("settle_ms", 2000) / 1000.0)
            frame = GOLDEN_DIR / cal["frame"]
            p = harness(["capture", "--out", str(frame), "--device", camera,
                         "--width", str(man["camera"]["width"]), "--height", str(man["camera"]["height"]),
                         "--settle", str(man["camera"].get("settle_ms", 800))])
            print(p.stdout.strip() or p.stderr.strip())
            if p.returncode != 0:
                raise RuntimeError("calibration capture failed rc=%d" % p.returncode)
            cargs = ["calibrate", "--frame", str(frame), "--out", str(GOLDEN_DIR / cal["file"]),
                     "--overlay", str(GOLDEN_DIR / cal["overlay"])]
            if cal.get("panel_hint"):
                cargs += ["--panel", cal["panel_hint"]]
            if cal.get("led_region"):
                cargs += ["--led-region", cal["led_region"]]
            if cal.get("orientation") is not None:
                cargs += ["--orientation", str(cal["orientation"])]
            p = harness(cargs)
            print(p.stdout.strip() or p.stderr.strip())
            if p.returncode != 0:
                raise RuntimeError("calibrate failed rc=%d" % p.returncode)
        finally:
            deck.stop()
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    print("recorded calibration: %s (+ %s, %s)" % (cal["file"], cal["frame"], cal["overlay"]))


def record_golden(g, man, bins, camera, args, dv, tmp):
    """Capture RECORD_FRAMES frames in the golden state and keep the one whose
    per-target means are closest to the per-target medians (suppresses the moving
    glare that spikes individual key faces on this glossy panel)."""
    n = max(2, args.record_frames)
    frames = []
    for i in range(n):
        f = str(Path(tmp) / ("rec-%d.jpg" % i))
        p = harness(["capture", "--out", f, "--device", camera,
                     "--width", str(man["camera"]["width"]), "--height", str(man["camera"]["height"]),
                     "--settle", str(man["camera"].get("settle_ms", 800))])
        if p.returncode != 0:
            raise RuntimeError("golden capture %d failed rc=%d: %s" % (i, p.returncode, p.stderr.strip()))
        frames.append(f)
    cal = json.loads((GOLDEN_DIR / man["calibration"]["file"]).read_text())
    per = {}
    for f in frames:
        img = dv.load_image(f)
        for name in cal["targets"]:
            per.setdefault(name, []).append(dv.mean_of_target(img, cal, name))
    # per-target channel medians, then the frame with the smallest total deviation
    med = {n: [sorted(v[c] for v in vals)[len(vals) // 2] for c in range(3)]
           for n, vals in per.items()}
    def dev(f):
        img = dv.load_image(f)
        return sum(abs(dv.mean_of_target(img, cal, n)[c] - med[n][c])
                   for n in cal["targets"] for c in range(3))
    best = min(frames, key=dev)
    golden = GOLDEN_DIR / g["file"]
    shutil.copyfile(best, golden)
    spreads = {n: round(sum(max(v[c] for v in vals) - min(v[c] for v in vals)
                            for c in range(3)) / 3.0, 1) for n, vals in per.items()}
    print("RECORD %-6s -> %s (median-selected of %d frames; per-target spread min..max: %s)"
          % (g["name"], g["file"], n,
             " ".join("%s=%.0f" % (n, s) for n, s in sorted(spreads.items()))))
    return True


def check_golden(g, man, bins, camera, args, dv, tmp, keep_dir):
    """Run the harness `check` (capture + compare vs golden) `attempts` times; pass
    when every calibrated target's MINIMUM delta over the attempts is within
    tolerance (moving glare spikes single targets in single frames; a wrong deck
    state fails every attempt and every target)."""
    name = g["name"]
    tol = args.tolerance if args.tolerance is not None else g.get("tolerance", 12)
    golden = GOLDEN_DIR / g["file"]
    if not golden.is_file():
        raise RuntimeError("golden %s missing (run --record first)" % golden)
    attempts = max(1, args.attempts)
    mind = {}   # target -> (min delta_mean, expected, actual)
    for i in range(1, attempts + 1):
        cargs = ["check", "--expected", str(golden),
                 "--calibration", str(GOLDEN_DIR / man["calibration"]["file"]),
                 "--tolerance", str(tol), "--device", camera,
                 "--width", str(man["camera"]["width"]), "--height", str(man["camera"]["height"]),
                 "--settle", str(args.settle), "--json"]
        if keep_dir:
            cargs += ["--keep", str(keep_dir / ("%s-actual-%d.jpg" % (name, i)))]
        p = harness(cargs, timeout=240)
        rows = []
        for ln in p.stdout.splitlines():
            try:
                d = json.loads(ln)
            except ValueError:
                continue
            if "summary" not in d:
                rows.append(d)
        for d in rows:
            t = d["target"]
            if t not in mind or d["delta_mean"] < mind[t][0]:
                mind[t] = (d["delta_mean"], d["expected"], d["actual"])
        failed = [d["target"] for d in rows if not d["pass"]]
        print("attempt %d/%d %-6s rc=%d failed-this-frame: %s"
              % (i, attempts, name, p.returncode, ", ".join(failed) or "none"))
        if p.returncode == EXIT_OK:
            break
    bad = {t: v for t, v in mind.items() if v[0] > tol}
    for t in sorted(mind):
        v = mind[t]
        print("  %-8s min_delta=%5.2f (tol %.1f) expected=%s actual=%s %s"
              % (t, v[0], tol, v[1], v[2], "OK" if v[0] <= tol else "FAIL"))
    ok = not bad
    print("%s %-6s min-over-%d-attempts: %d/%d targets within tolerance %.1f%s"
          % ("PASS" if ok else "FAIL", name, attempts, len(mind) - len(bad), len(mind), tol,
             "" if ok else "  failing: " + ", ".join(sorted(bad))))
    return ok


def run_golden(g, man, bins, camera, args, record, dv):
    tmp = tempfile.mkdtemp(prefix="dsv-golden-%s-" % g["name"])
    keep_dir = Path(args.keep) if args.keep else None
    if keep_dir and not record:
        keep_dir.mkdir(parents=True, exist_ok=True)
    try:
        deck = Deck(bins, tmp)
        deck.start()
        try:
            apply_state(deck, g["state"], tmp)
            time.sleep(g.get("settle_ms", 2000) / 1000.0)
            if record:
                return record_golden(g, man, bins, camera, args, dv, tmp)
            return check_golden(g, man, bins, camera, args, dv, tmp, keep_dir)
        finally:
            deck.stop()
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def main(argv=None):
    ap = argparse.ArgumentParser(
        prog="run_golden.py",
        description="DS-028: set the deck to each golden state via ds, capture, compare "
                    "against tools/verify/golden/ (exit 0 pass / 7 mismatch). Hardware-gated.")
    ap.add_argument("--record", action="store_true",
                    help="(re)capture the goldens + calibration instead of checking")
    ap.add_argument("--only", default=None, help="comma-separated golden names to run")
    ap.add_argument("--bins", default=None, help="dir with ds + ds-daemon (default $DS_BIN_DIR)")
    ap.add_argument("--camera", default=None, help="camera node (default $DSV_CAMERA or /dev/video2)")
    ap.add_argument("--settle", type=int, default=800, help="harness capture settle ms for checks")
    ap.add_argument("--attempts", type=int, default=5,
                    help="harness check captures per golden in check mode; a target passes "
                         "when its MINIMUM delta over the attempts is within tolerance "
                         "(moving glare spikes single targets in single frames; a wrong "
                         "deck state fails every attempt)")
    ap.add_argument("--record-frames", type=int, default=5,
                    help="--record: captures per golden; the median-closest frame is kept")
    ap.add_argument("--tolerance", type=float, default=None, help="override manifest tolerance")
    ap.add_argument("--keep", default=None, help="dir to keep the captured actual frames")
    ap.add_argument("--skip-calibration", action="store_true",
                    help="--record: do not re-record calibration.json")
    ap.add_argument("--strict", action="store_true",
                    help="check mode: treat a golden carrying a documented skip_reason as a failure")
    args = ap.parse_args(argv)

    man = json.loads(MANIFEST.read_text())
    camera = args.camera or os.environ.get("DSV_CAMERA") or man["camera"].get("device", "/dev/video2")
    try:
        bins = find_bins(args.bins)
    except RuntimeError as e:
        eprint("run_golden: %s" % e)
        return EXIT_ERROR
    print("run_golden: bins=%s camera=%s mode=%s" % (bins, camera, "record" if args.record else "check"))

    goldens = man["goldens"]
    if args.only:
        want = set(s.strip() for s in args.only.split(",") if s.strip())
        goldens = [g for g in goldens if g["name"] in want]
        if not goldens:
            eprint("no goldens match --only %s (have: %s)" % (args.only, ", ".join(g["name"] for g in man["goldens"])))
            return EXIT_USAGE

    try:
        if args.record and not args.skip_calibration:
            record_calibration(man, bins, camera, args)
        dv = load_harness()
        dv = load_harness()
        passed = failed = 0
        skipped = []
        for g in goldens:
            if not args.record and g.get("skip_reason") and not args.strict:
                print("SKIP %-6s %s" % (g["name"], g["skip_reason"]))
                skipped.append(g["name"])
                continue
            if run_golden(g, man, bins, camera, args, args.record, dv):
                passed += 1
            else:
                failed += 1
        if args.record:
            print("record: done (%d golden(s))" % len(goldens))
            return EXIT_OK
        print("golden checks: %s (%d pass, %d fail, %d skipped%s)" % (
            "PASS" if not failed else "MISMATCH", passed, failed, len(skipped),
            " (" + ", ".join(skipped) + ")" if skipped else ""))
        return EXIT_OK if not failed else EXIT_MISMATCH
    except RuntimeError as e:
        eprint("run_golden: %s" % e)
        return EXIT_ERROR


if __name__ == "__main__":
    sys.exit(main())
