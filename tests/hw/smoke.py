#!/usr/bin/env python3
"""DS-026 hardware smoke suite — real-deck end-to-end smoke for the phase-2 device path.

Run on a machine with the MasterHub deck attached (five Cooler Master modules,
VID 0x2516: pids 01f6 base, 01fe keys, 0202 knobs, 0204 rollers, 0206 dial):

    DS_HARDWARE=1 python tests/hw/smoke.py            # hardware run
    python tests/hw/smoke.py                          # CI: skips cleanly, exit 0

Python 3 stdlib only. Black box: the suite drives the real `ds` / `ds-daemon`
binaries and reads sysfs + /proc, never driver internals. Steps:

  1. enumerate  — every attached module visible via sysfs enumeration (the descriptor
                  source of `ds-device::hid::enumerate`) and via the daemon status
                  of a `ds daemon start --device hid:` run;
  2. handshake  — the daemon on `hid:` completes the software-mode handshake for every
                  module (device_status "connected hid: (healthy)", N module(s)
                  handshaken in daemon.log);
  3. frame      — `ds set /controls/keys/0/image --file examples/key.png` then a solid
                  colour: the write log shows every push accepted, no error;
  4. replug     — udev unbind/bind of one module (root) and assert rescan + re-push of
                  the latest frame within 5 s; when sysfs is not writable the step
                  SKIPs and points at the manual fallback in docs/90-verification.md;
  5. handback   — SIGTERM the daemon: exit 0, socket + pid file removed, no lingering
                  hidraw/usb handles of ours, guard handback logged.

Exit code 0 = no FAIL (SKIPs are clean, with the exact reason printed); 1 = at least
one FAIL. `DS_HARDWARE` unset, or no deck attached, prints SKIP and exits 0, so CI
never needs hardware. Hardware absence or missing device permissions are states to
detect, not failures (same rule as tests/hw/hid.rs). Every wait has a timeout, all
subprocess calls are argv lists (no shell interpolation), and the daemon + temp dir
are always cleaned up in a finally block.
"""

import json
import os
import re
import shutil
import signal
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import hwlib  # noqa: E402  (tests/hw helper, stdlib only)

VERBOSE = "--verbose" in sys.argv

ROOT = hwlib.ROOT
KEY_PNG = ROOT / "examples" / "key.png"

#: the five module kinds this deck exposes (pids in hwlib.PID_KINDS)
DECK_KINDS = ("base", "keys", "knobs", "rollers", "dial")

HANDSHAKE_TIMEOUT_S = 25.0
REPLUG_BUDGET_S = 5.0
HANDBACK_TIMEOUT_S = 10.0
SUITE_TIMEOUT_S = 300


def log(msg):
    if VERBOSE:
        print(msg, file=sys.stderr)


class Ctx:
    """Shared run context: binaries, temp dir, daemon, enumeration, results."""

    def __init__(self):
        self.bins = None
        self.tmp = None
        self.img_dir = None
        self.daemon = None
        self.modules = []
        self.access = []
        self.connected = False
        self.results = []  # (step, "PASS"|"SKIP"|"FAIL", evidence)


def record(ctx, step, result, evidence):
    ctx.results.append((step, result, evidence))
    print("%s  %s — %s" % (result, step, evidence))


def wait_until(pred, timeout_s, what, interval=0.1):
    """Poll pred() until true; raises TimeoutError (with `what`) after timeout_s."""
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        v = pred()
        if v:
            return v
        time.sleep(interval)
    raise TimeoutError("timed out after %.1fs waiting for %s" % (timeout_s, what))


# ---------------------------------------------------------------- steps ----


def step_enumerate(ctx):
    """Step 1: all attached modules visible via sysfs enumeration + daemon status."""
    mods = ctx.modules
    print("enumerated %d MasterHub module(s) (VID %#06x):" % (len(mods), hwlib.VENDOR_ID))
    for m in mods:
        print(
            "  %-7s pid %#06x  sysfs %-10s usb %-18s hidraw %-12s serial %s"
            % (
                m["kind"],
                m["pid"],
                m["sysfs"],
                m["usb_node"],
                m["hidraw"] or "-",
                m["serial"] or "-",
            )
        )
    kinds = sorted(m["kind"] for m in mods)
    ctx.access = hwlib.access_report(mods)
    # the daemon run on the hid: URI — this is also the process steps 2-5 drive
    ctx.daemon = hwlib.Daemon(ctx.bins, ctx.tmp / "data")
    ctx.daemon.start("hid:")
    st = ctx.daemon.status_json()
    ok = (
        st.get("running") is True
        and st.get("device") == "hid:"
        and isinstance(st.get("pid"), int)
        and st.get("pid") == ctx.daemon.pid
    )
    if not ok:
        record(ctx, "enumerate", "FAIL",
               "daemon status unexpected: %s" % json.dumps(st))
        return
    evidence = (
        "%d module(s) in sysfs [%s]; daemon pid=%d running on device=%s "
        "device_status=%r" % (len(mods), ",".join(kinds), st["pid"], st["device"],
                              st.get("device_status"))
    )
    if set(DECK_KINDS) - set(kinds):
        evidence += "; missing kinds: %s" % sorted(set(DECK_KINDS) - set(kinds))
        record(ctx, "enumerate", "FAIL", evidence)
        return
    record(ctx, "enumerate", "PASS", evidence)


def step_handshake(ctx):
    """Step 2: software-mode handshake for every module on the daemon's hid: transport."""
    daemon = ctx.daemon
    deadline = time.monotonic() + HANDSHAKE_TIMEOUT_S
    status = None
    while time.monotonic() < deadline:
        status = daemon.status_json()
        ds = str(status.get("device_status", ""))
        if ds.startswith("connected"):
            break
        time.sleep(0.25)
    else:
        status = daemon.status_json()
    ds = str(status.get("device_status", ""))
    if not ds.startswith("connected"):
        # distinguish "permissions block opening the nodes" (SKIP) from a real failure
        blocked = [a for a in ctx.access if not a["usb_ok"] and not a["hidraw_ok"]]
        if blocked:
            detail = "; ".join(
                "%s: usb %s errno=%s, hidraw %s errno=%s"
                % (a["kind"], a["usb_node"], a["usb_errno"], a["hidraw"], a["hidraw_errno"])
                for a in blocked
            )
            record(
                ctx, "handshake", "SKIP",
                "device nodes not openable by uid=%d (%s); the DS-025 udev rule "
                "(packaging/udev/99-deckstack.rules, uaccess on hidraw+usb) or root is "
                "required to open them; daemon device_status=%r" % (os.getuid(), detail, ds),
            )
        else:
            record(ctx, "handshake", "FAIL",
                   "device_status=%r after %.0fs; daemon.log tail: %s"
                   % (ds, HANDSHAKE_TIMEOUT_S, daemon.log_text()[-800:]))
        return
    ctx.connected = True
    m = re.search(
        r"connected to hid:: (\d+) module\(s\) handshaken \[([^\]]*)\]", daemon.log_text()
    )
    if not m:
        record(ctx, "handshake", "FAIL",
               "device_status=%r but no 'module(s) handshaken' line in daemon.log: %s"
               % (ds, daemon.log_text()[-800:]))
        return
    count = int(m.group(1))
    fw_list = m.group(2)
    kinds = re.findall(r"(\w+) fw=(\S+)", fw_list)
    kinds_ok = sorted(k for k, _ in kinds) == sorted(m["kind"] for m in ctx.modules)
    if count != len(ctx.modules) or not kinds_ok:
        record(ctx, "handshake", "FAIL",
               "handshaken %d module(s) [%s] but %d enumerated; line=%r"
               % (count, fw_list, len(ctx.modules), m.group(0)))
        return
    record(ctx, "handshake", "PASS",
           "device_status=%r; %d/%d module(s) handshaken with firmware: %s"
           % (ds, count, len(ctx.modules), ", ".join("%s fw=%s" % (k, f) for k, f in kinds)))


def step_frame(ctx):
    """Step 3: push a key image (examples/key.png) then a colour; every write accepted."""
    if not ctx.connected:
        record(ctx, "frame", "SKIP", "device not handshaken (see handshake step)")
        return
    daemon = ctx.daemon
    # `ds set --file` confines the image to /policy/fs/roots + /tmp (cli.md §4), so the
    # copy of examples/key.png must live under /tmp — the run temp dir may be $TMPDIR.
    img_dir = ctx.img_dir
    if img_dir is None:
        p = daemon.ds("set", "/policy/fs/roots", "--json", json.dumps([str(ctx.tmp)]))
        if p.returncode == 0:
            img_dir = ctx.tmp
        else:
            record(ctx, "frame", "FAIL",
                   "cannot stage the key image under an allowed fs root: no writable /tmp "
                   "and ds set /policy/fs/roots rc=%d stderr=%s" % (p.returncode, p.stderr))
            return
    key_png = img_dir / "key.png"
    shutil.copyfile(KEY_PNG, key_png)
    rev_before = json.loads(daemon.ds("get", "/meta", "--json").stdout)["rev"]

    p_img = daemon.ds("set", "/controls/keys/0/image", "--file", str(key_png))
    if p_img.returncode != 0:
        record(ctx, "frame", "FAIL",
               "ds set key image rc=%d stderr=%s" % (p_img.returncode, p_img.stderr))
        return
    p_col = daemon.ds("set", "/controls/keys/1/image",
                      '{"kind":"color","value":"#3cc86e"}')
    if p_col.returncode != 0:
        record(ctx, "frame", "FAIL",
               "ds set colour rc=%d stderr=%s" % (p_col.returncode, p_col.stderr))
        return
    # let the device task drain the pushes, then inspect the write log
    time.sleep(1.5)
    log_text = daemon.log_text()
    pushed = [l for l in daemon.log_lines() if '"pushed' in l]
    img_pushes = [l for l in pushed if "/controls/keys/0/image" in l]
    col_pushes = [l for l in pushed if "/controls/keys/1/image" in l]
    errors = [
        l for l in daemon.log_lines()
        if re.search(r'"level":"(error|warn)"', l)
        and ("push" in l or "timed out" in l or "unhealthy" in l)
    ]
    state = json.loads(daemon.ds("get", "/controls/keys/0/image", "--json").stdout)
    rev_after = json.loads(daemon.ds("get", "/meta", "--json").stdout)["rev"]
    if not img_pushes or not col_pushes:
        record(ctx, "frame", "FAIL",
               "no accepted push in daemon.log for the image/colour; pushed lines: %s; "
               "log tail: %s" % (pushed[-5:], log_text[-600:]))
        return
    if errors:
        record(ctx, "frame", "FAIL", "device write errors in daemon.log: %s" % errors[-5:])
        return
    if state["value"].get("kind") != "file":
        record(ctx, "frame", "FAIL", "state readback unexpected: %s" % state)
        return
    record(ctx, "frame", "PASS",
           "key:0 image from %s (%d B) and key:1 colour #3cc86e accepted: %d pushed "
           "log line(s), 0 write errors; state=%s; rev %s->%s (the two durable `ds set` "
           "commits; stream frames never bump rev, S1)"
           % (KEY_PNG, key_png.stat().st_size, len(pushed),
              json.dumps(state["value"]), rev_before, rev_after))


def step_replug(ctx):
    """Step 4: unplug/replug one module via udev unbind/bind; rescan + re-push <= 5 s."""
    if not ctx.connected:
        record(ctx, "replug", "SKIP", "device not handshaken (see handshake step)")
        return
    if not hwlib.unbind_bind_writable():
        record(ctx, "replug", "SKIP",
               "%s is not writable by uid=%d (udev unbind/bind needs root); no in-tree "
               "hotplug hook exists for the hid: transport (ds-vdev's --drop-after is "
               "virtual-only), so the documented manual fallback applies — see "
               "docs/90-verification.md: physically replug the module, or as root: "
               "echo <busnum-devnum> > %s ; echo <busnum-devnum> > %s"
               % (hwlib.USB_UNBIND, os.getuid(), hwlib.USB_UNBIND, hwlib.USB_BIND))
        return
    daemon = ctx.daemon
    # pick one module to cycle (knobs if present, else the first)
    target = next((m for m in ctx.modules if m["kind"] == "knobs"), ctx.modules[0])
    ident = target["sysfs"]
    log_before = daemon.log_text()
    t0 = time.monotonic()
    with open(hwlib.USB_UNBIND, "w") as f:
        f.write(ident + "\n")
    try:
        wait_until(
            lambda: not str(daemon.status_json().get("device_status", "")).startswith("connected"),
            REPLUG_BUDGET_S, "daemon to notice the unplug of %s" % ident,
        )
    except TimeoutError:
        record(ctx, "replug", "FAIL",
               "daemon still reports %r %.1fs after unbind of %s"
               % (daemon.status_json().get("device_status"), REPLUG_BUDGET_S, ident))
        return
    t_gone = time.monotonic() - t0
    with open(hwlib.USB_BIND, "w") as f:
        f.write(ident + "\n")
    t_bind = time.monotonic()
    try:
        wait_until(
            lambda: str(daemon.status_json().get("device_status", "")).startswith("connected"),
            REPLUG_BUDGET_S, "daemon to reconnect after replug of %s" % ident,
        )
    except TimeoutError:
        record(ctx, "replug", "FAIL",
               "daemon did not reconnect within %.1fs after replug of %s; status=%r; "
               "log tail: %s"
               % (REPLUG_BUDGET_S, ident, daemon.status_json().get("device_status"),
                  daemon.log_text()[-600:]))
        return
    t_back = time.monotonic() - t_bind
    # re-push of the latest frame must have happened after the replug
    def repushed():
        txt = daemon.log_text()
        return "re-pushing" in txt[len(log_before):] and "re-push" in txt[len(log_before):]
    try:
        wait_until(repushed, REPLUG_BUDGET_S, "re-push of cached targets after replug")
    except TimeoutError:
        record(ctx, "replug", "FAIL",
               "reconnected in %.2fs but no re-push logged; log tail: %s"
               % (t_back, daemon.log_text()[-600:]))
        return
    if t_back > REPLUG_BUDGET_S:
        record(ctx, "replug", "FAIL",
               "replug took %.2fs > %.1fs budget (unplug noticed after %.2fs)"
               % (t_back, REPLUG_BUDGET_S, t_gone))
        return
    record(ctx, "replug", "PASS",
           "module %s (%s) unbound/bound via %s; disconnect noticed after %.2fs, "
           "reconnected + re-pushed latest frame in %.2fs (budget %.1fs)"
           % (ident, target["kind"], hwlib.USB_UNBIND, t_gone, t_back, REPLUG_BUDGET_S))


def step_handback(ctx):
    """Step 5: SIGTERM the daemon — exit 0, clean state, no lingering hidraw handles."""
    daemon = ctx.daemon
    pid = daemon.pid
    if pid is None:
        record(ctx, "handback", "FAIL", "no daemon pid file at %s" % daemon.pid_file)
        return
    fds_before = hwlib.fd_targets(pid)
    ok_term = daemon.sigterm(timeout=HANDBACK_TIMEOUT_S)
    if not ok_term:
        record(ctx, "handback", "FAIL",
               "daemon pid=%d did not exit within %.0fs after SIGTERM" % (pid, HANDBACK_TIMEOUT_S))
        return
    wait_gone = not hwlib.alive(pid)
    log_text = daemon.log_text()
    clean_exit = "stopped cleanly (exit 0)" in log_text
    guard_lines = [l for l in daemon.log_lines() if '"ds-daemon::guard"' in l]
    handed_back = any("device handed back, transport closed" in l for l in guard_lines)
    left_as_is = any("for handback" in l and "left as-is" in l for l in guard_lines)
    if not (clean_exit and wait_gone):
        record(ctx, "handback", "FAIL",
               "pid=%d gone=%s but log lacks 'stopped cleanly (exit 0)'; log tail: %s"
               % (pid, wait_gone, log_text[-600:]))
        return
    if not (handed_back or left_as_is):
        record(ctx, "handback", "FAIL",
               "no guard handback line in daemon.log; guard lines: %s" % guard_lines[-5:])
        return
    sock_gone = not daemon.sock.exists()
    pidfile_gone = not daemon.pid_file.exists()
    # after exit nothing of ours may hold deck nodes anymore
    holders_after = hwlib.procs_holding_deck_nodes(ctx.modules)
    ours = {
        p: v for p, v in holders_after.items()
        if p == pid or "ds-daemon" in v[1] or str(ctx.tmp) in v[1]
    }
    p = daemon.ds("daemon", "status", timeout=hwlib.OP_TIMEOUT_S)
    unreachable = p.returncode == 6
    problems = []
    if not sock_gone:
        problems.append("control socket still present: %s" % daemon.sock)
    if not pidfile_gone:
        problems.append("pid file still present: %s" % daemon.pid_file)
    if ours:
        problems.append("lingering deck-node handles of ours: %s" % ours)
    if not unreachable:
        problems.append("ds daemon status after exit rc=%d (expected 6 E_UNREACHABLE)" % p.returncode)
    if problems:
        record(ctx, "handback", "FAIL", "; ".join(problems))
        return
    record(ctx, "handback", "PASS",
           "SIGTERM to pid=%d -> process gone, log 'stopped cleanly (exit 0)', guard %s; "
           "socket+pid file removed, ds daemon status now exit 6, deck-node fds held "
           "before exit: %s, lingering after exit: none"
           % (pid, "handed back" if handed_back else "left as-is (hid: unopenable, "
                "warn logged)", fds_before or "none"))


# ----------------------------------------------------------------- main ----


def _alarm(signum, frame):
    raise TimeoutError("hardware smoke exceeded the %ss watchdog" % SUITE_TIMEOUT_S)


def main():
    print("ds hw smoke (DS-026): python=%s uid=%d bins=%s"
          % (sys.version.split()[0], os.getuid(), hwlib.bin_dir()))
    if os.environ.get("DS_HARDWARE") != "1":
        print("SKIP: DS_HARDWARE is not 1 — hardware smoke disabled (CI stays hardware-free)")
        print("RESULT: SKIP")
        return 0
    modules = hwlib.enumerate_sysfs()
    if not modules:
        print("SKIP: no MasterHub module (VID %#06x) enumerated in %s — no deck attached"
              % (hwlib.VENDOR_ID, hwlib.SYS_USB_DEVICES))
        print("RESULT: SKIP")
        return 0

    signal.signal(signal.SIGALRM, _alarm)
    signal.alarm(SUITE_TIMEOUT_S)
    ctx = Ctx()
    ctx.modules = modules
    try:
        ctx.bins = hwlib.ensure_binaries()
        ctx.tmp = Path(tempfile.mkdtemp(prefix="ds-hw-smoke-"))
        log("temp dir: %s" % ctx.tmp)
        try:
            ctx.img_dir = Path(tempfile.mkdtemp(prefix="ds-hw-smoke-img-", dir="/tmp"))
            log("image staging dir: %s" % ctx.img_dir)
        except OSError:
            ctx.img_dir = None  # step_frame falls back to /policy/fs/roots += temp dir
        step_enumerate(ctx)
        step_handshake(ctx)
        step_frame(ctx)
        step_replug(ctx)
        step_handback(ctx)
    except TimeoutError as e:
        record(ctx, "watchdog", "FAIL", str(e))
    except Exception as e:  # a harness error is a failure, never a silent pass
        record(ctx, "harness", "FAIL", "%s: %s" % (type(e).__name__, e))
        if VERBOSE:
            import traceback

            traceback.print_exc()
    finally:
        signal.alarm(0)
        if ctx.daemon is not None:
            ctx.daemon.kill()
        if ctx.tmp is not None:
            shutil.rmtree(ctx.tmp, ignore_errors=True)
            if ctx.tmp.exists():
                print("WARN  temp dir still exists: %s" % ctx.tmp, file=sys.stderr)
        if ctx.img_dir is not None and ctx.img_dir != ctx.tmp:
            shutil.rmtree(ctx.img_dir, ignore_errors=True)
        print("cleanup: daemon killed (SIGTERM then SIGKILL), temp dir removed")

    n_pass = sum(1 for _, r, _ in ctx.results if r == "PASS")
    n_skip = sum(1 for _, r, _ in ctx.results if r == "SKIP")
    n_fail = sum(1 for _, r, _ in ctx.results if r == "FAIL")
    print("-" * 72)
    print("hw smoke: %d step(s): %d PASS, %d SKIP, %d FAIL" % (len(ctx.results), n_pass, n_skip, n_fail))
    if n_fail:
        print("RESULT: FAIL")
        return 1
    if n_skip:
        print("RESULT: OK (%d step(s) skipped — reasons above)" % n_skip)
    else:
        print("RESULT: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
