#!/usr/bin/env python3
"""Helpers for the DS-026 hardware smoke suite (`tests/hw/smoke.py`).

Python 3 stdlib only. Everything here is black-box: the deck is addressed through
sysfs (the same descriptor source the libusb backend of `ds-device::hid::enumerate`
reads), through the real `ds` / `ds-daemon` binaries, and through `/proc` — no driver
internals are imported. All subprocess calls use argv lists (no shell interpolation)
and every wait carries a timeout.
"""

import os
import shutil
import signal
import subprocess
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

BUILD_TIMEOUT_S = 900
OP_TIMEOUT_S = 20
BOOT_TIMEOUT_S = 30
KILL_TIMEOUT_S = 5

BINARIES = ("ds", "ds-daemon")

#: USB vendor id of the Cooler Master MasterHub family (ds-device VENDOR_ID).
VENDOR_ID = 0x2516
#: pid -> module kind name (crates/ds-device/src/module.rs, mh-protocol ModuleKind).
PID_KINDS = {
    0x01F6: "base",
    0x01FE: "keys",
    0x0200: "faders",
    0x0202: "knobs",
    0x0204: "rollers",
    0x0206: "dial",
}

SYS_USB_DEVICES = Path("/sys/bus/usb/devices")
SYS_HIDRAW = Path("/sys/class/hidraw")
USB_UNBIND = Path("/sys/bus/usb/drivers/usb/unbind")
USB_BIND = Path("/sys/bus/usb/drivers/usb/bind")


def bin_dir():
    """Directory of the built binaries ($DS_BIN_DIR, else <root>/target/debug)."""
    env = os.environ.get("DS_BIN_DIR")
    if env:
        return Path(env)
    return ROOT / "target" / "debug"


def ensure_binaries():
    """Build the two binaries the smoke drives if they are missing (bounded)."""
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
        [cargo, "build", "--bin", "ds", "--bin", "ds-daemon"],
        cwd=str(ROOT), timeout=BUILD_TIMEOUT_S, check=True,
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    missing = [b for b in BINARIES if not (d / b).is_file()]
    if missing:
        raise RuntimeError("cargo build finished but still missing: %s" % ", ".join(missing))
    return d


def _read(path):
    try:
        return Path(path).read_text().strip()
    except OSError:
        return ""


def enumerate_sysfs():
    """Enumerate attached MasterHub modules from sysfs (libusb reads the same files).

    Returns a list of dicts sorted by sysfs name, each with: sysfs (e.g. "3-2.3.4"),
    pid, kind, busnum, devnum, usb_node (/dev/bus/usb/BBB/DDD), hidraw (/dev/hidrawN,
    when the module exposes one), serial, product. Empty list = no deck attached.
    """
    modules = []
    if not SYS_USB_DEVICES.is_dir():
        return modules
    for entry in sorted(SYS_USB_DEVICES.iterdir()):
        if not (entry / "idVendor").is_file():
            continue
        try:
            vid = int(_read(entry / "idVendor"), 16)
            pid = int(_read(entry / "idProduct"), 16)
        except ValueError:
            continue
        if vid != VENDOR_ID or pid not in PID_KINDS:
            continue
        busnum = _read(entry / "busnum")
        devnum = _read(entry / "devnum")
        modules.append(
            {
                "sysfs": entry.name,
                "pid": pid,
                "kind": PID_KINDS[pid],
                "busnum": busnum,
                "devnum": devnum,
                "usb_node": (
                    "/dev/bus/usb/%03d/%03d" % (int(busnum), int(devnum))
                    if busnum and devnum
                    else None
                ),
                "hidraw": None,
                "serial": _read(entry / "serial"),
                "product": _read(entry / "product"),
            }
        )
    # map hidraw nodes onto modules through the sysfs topology
    if SYS_HIDRAW.is_dir():
        for hr in sorted(SYS_HIDRAW.iterdir()):
            dev = hr / "device"
            if not dev.exists():
                continue
            cur = dev.resolve()
            usb_dev = None
            for _ in range(6):  # interface -> config -> device
                if (cur / "idVendor").is_file():
                    usb_dev = cur
                    break
                cur = cur.parent
            if usb_dev is None:
                continue
            for m in modules:
                if m["sysfs"] == usb_dev.name:
                    m["hidraw"] = "/dev/%s" % hr.name
    modules.sort(key=lambda m: m["sysfs"])
    return modules


def probe_rw(path):
    """Try to open a device node read-write; returns (ok, errno or None)."""
    if not path:
        return False, None
    try:
        fd = os.open(path, os.O_RDWR)
    except OSError as e:
        return False, e.errno
    os.close(fd)
    return True, None


def access_report(modules):
    """Per-module openability of the usb/hidraw nodes, for precise SKIP evidence."""
    out = []
    for m in modules:
        usb_ok, usb_errno = probe_rw(m["usb_node"])
        hr_ok, hr_errno = probe_rw(m["hidraw"])
        out.append(
            {
                "kind": m["kind"],
                "usb_node": m["usb_node"],
                "usb_ok": usb_ok,
                "usb_errno": usb_errno,
                "hidraw": m["hidraw"],
                "hidraw_ok": hr_ok,
                "hidraw_errno": hr_errno,
            }
        )
    return out


def unbind_bind_writable():
    """Whether this user may drive the udev unbind/bind replug path (root only)."""
    return USB_UNBIND.is_file() and os.access(str(USB_UNBIND), os.W_OK) and os.access(
        str(USB_BIND), os.W_OK
    )


def fd_targets(pid, prefixes=("/dev/hidraw", "/dev/bus/usb")):
    """Symlink targets of <pid>'s open fds that point at device nodes."""
    out = []
    fd_dir = Path("/proc/%d/fd" % pid)
    try:
        fds = list(fd_dir.iterdir())
    except OSError:
        return out
    for fd in fds:
        try:
            target = os.readlink(str(fd))
        except OSError:
            continue
        if target.startswith(prefixes):
            out.append(target)
    return sorted(set(out))


def procs_holding_deck_nodes(modules):
    """All pids currently holding an fd on one of the deck's usb/hidraw nodes."""
    nodes = set()
    for m in modules:
        if m["usb_node"]:
            nodes.add(m["usb_node"])
        if m["hidraw"]:
            nodes.add(m["hidraw"])
    holders = {}
    proc = Path("/proc")
    for p in proc.iterdir():
        if not p.name.isdigit():
            continue
        for t in fd_targets(int(p.name)):
            if t in nodes:
                try:
                    cmd = _read(p / "cmdline").replace("\0", " ")
                except OSError:
                    cmd = ""
                holders[int(p.name)] = (t, cmd)
    return holders


def alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


def wait_gone(pid, timeout_s, what):
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        if not alive(pid):
            return True
        time.sleep(0.05)
    raise TimeoutError("timed out after %ss waiting for %s (pid %s)" % (timeout_s, what, pid))


def wait_file(path, timeout_s, what):
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        if Path(path).exists():
            return
        time.sleep(0.05)
    raise TimeoutError("timed out after %ss waiting for %s (%s)" % (timeout_s, what, path))


class Daemon:
    """One `ds daemon start --device hid:` run on a fresh temp data dir."""

    def __init__(self, bins, data_dir):
        self.bins = bins
        self.data = Path(data_dir)
        self.sock = self.data / "daemon.sock"
        self.pid_file = self.data / "daemon.pid"
        self.log_path = self.data / "daemon.log"
        self.pid = None
        self._env = dict(os.environ)
        self._env["DS_DAEMON_BIN"] = str(bins / "ds-daemon")

    def start(self, device="hid:", timeout=BOOT_TIMEOUT_S):
        self.data.mkdir(parents=True, exist_ok=True)
        p = subprocess.run(
            [
                str(self.bins / "ds"),
                "daemon", "start",
                "--device", device,
                "--data-dir", str(self.data),
            ],
            capture_output=True, text=True, timeout=timeout, env=self._env,
        )
        if p.returncode != 0:
            raise RuntimeError(
                "ds daemon start failed rc=%d\nstdout: %s\nstderr: %s\nlog: %s"
                % (p.returncode, p.stdout, p.stderr, self.log_text())
            )
        wait_file(self.sock, timeout, "daemon control socket")
        if self.pid_file.is_file():
            try:
                self.pid = int(self.pid_file.read_text().strip())
            except ValueError:
                self.pid = None
        return p

    def ds(self, *args, timeout=OP_TIMEOUT_S):
        """Run the real `ds` binary against this daemon. Returns CompletedProcess."""
        return subprocess.run(
            [
                str(self.bins / "ds"),
                "--socket", str(self.sock),
                "--data-dir", str(self.data),
            ]
            + list(args),
            capture_output=True, text=True, timeout=timeout, env=self._env,
        )

    def status_json(self, timeout=OP_TIMEOUT_S):
        p = self.ds("daemon", "status", "--json", timeout=timeout)
        if p.returncode != 0:
            raise RuntimeError(
                "ds daemon status rc=%d stderr=%s" % (p.returncode, p.stderr)
            )
        import json

        return json.loads(p.stdout)

    def log_text(self):
        return _read(self.log_path)

    def log_lines(self):
        return [l for l in self.log_text().splitlines() if l.strip()]

    def sigterm(self, timeout=KILL_TIMEOUT_S + 5):
        """SIGTERM the daemon; returns True when the process exited within timeout."""
        if self.pid is None or not alive(self.pid):
            return True
        os.kill(self.pid, signal.SIGTERM)
        try:
            wait_gone(self.pid, timeout, "daemon exit after SIGTERM")
            return True
        except TimeoutError:
            return False

    def kill(self):
        """Last-resort cleanup: SIGTERM then SIGKILL the daemon pid."""
        if self.pid is None:
            return
        for sig in (signal.SIGTERM, signal.SIGKILL):
            if not alive(self.pid):
                return
            try:
                os.kill(self.pid, sig)
            except OSError:
                return
            try:
                wait_gone(self.pid, KILL_TIMEOUT_S, "daemon death")
                return
            except TimeoutError:
                continue
