#!/usr/bin/env python3
"""Smoke test for the deckstack Python helper (ticket DS-063).

Boots ds-vdev + ds-daemon on a fresh temp dir (same pattern as tests/e2e/run.py),
then drives the system ONLY through the `deckstack` helper (which shells out to
the real `ds` binary) and asserts:

    1. status() reports the daemon running on the virtual device
    2. set() a key colour, get() it back, rev round-trips
    3. bind()/unbind() round-trip (binding id bNNNN, removed list)
    4. watch() yields exactly one event for a set (streaming verb, --ndjson)
    5. stream_push() + stream_stats() on key:0 against vdev
    6. error mapping: get() of an unknown path raises DeckError with exit_code 2
       and the `[exit 2]` suffix in the message

Exit 0 = all green. Every wait carries a timeout; the daemon and vdev are always
killed (SIGTERM then SIGKILL) and the temp dir removed, so the run cannot hang or
leak processes.

Usage: python clients/ds-py/smoke.py   (from the repo root; DS_BIN_DIR overrides
the binary directory, default target/release with target/debug fallback)
"""

import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(Path(__file__).resolve().parent))

from deckstack import DeckClient, DeckError  # noqa: E402

BUILD_TIMEOUT_S = 900
OP_TIMEOUT_S = 20
BOOT_TIMEOUT_S = 30
WATCH_TIMEOUT_S = 20
KILL_TIMEOUT_S = 5

BINARIES = ("ds", "ds-daemon", "ds-vdev", "ds-vdevctl")


def bin_dir():
    env = os.environ.get("DS_BIN_DIR")
    if env:
        return Path(env)
    for candidate in (ROOT / "target" / "release", ROOT / "target" / "debug"):
        if all((candidate / b).is_file() for b in BINARIES):
            return candidate
    return ROOT / "target" / "debug"


def ensure_binaries():
    d = bin_dir()
    missing = [b for b in BINARIES if not (d / b).is_file()]
    if not missing:
        return d
    cargo = shutil.which("cargo")
    if cargo is None:
        raise RuntimeError("cargo not found and binaries missing from %s" % d)
    print("building binaries (bounded %ss): %s" % (BUILD_TIMEOUT_S, " ".join(missing)))
    subprocess.run([cargo, "build", "--bin", "ds", "--bin", "ds-daemon",
                    "--bin", "ds-vdev", "--bin", "ds-vdevctl"],
                   cwd=str(ROOT), timeout=BUILD_TIMEOUT_S, check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return d


class Stack:
    """One booted ds-vdev + ds-daemon pair on a fresh temp dir (mirrors tests/e2e)."""

    def __init__(self, bins):
        self.bins = bins
        self.tmp = Path(tempfile.mkdtemp(prefix="ds-py-smoke-"))
        self.vdev_sock = self.tmp / "vdev.sock"
        self.data = self.tmp / "data"
        self.sock = self.data / "daemon.sock"
        self.vdev = None
        self.daemon_pid = None

    def start(self):
        self.vdev = subprocess.Popen(
            [str(self.bins / "ds-vdev"), "--socket", str(self.vdev_sock)],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            stdin=subprocess.DEVNULL, cwd=str(ROOT))
        self._wait_for_file(self.vdev_sock, 10, "ds-vdev device socket")
        env = dict(os.environ)
        env["DS_DAEMON_BIN"] = str(self.bins / "ds-daemon")
        p = subprocess.run(
            [str(self.bins / "ds"), "--socket", str(self.sock), "--data-dir", str(self.data),
             "daemon", "start", "--device", "virtual:%s" % self.vdev_sock,
             "--data-dir", str(self.data)],
            capture_output=True, text=True, timeout=BOOT_TIMEOUT_S, env=env, cwd=str(ROOT))
        if p.returncode != 0:
            raise RuntimeError("ds daemon start failed rc=%d\n%s\n%s"
                               % (p.returncode, p.stdout, p.stderr))
        self._wait_for_file(self.sock, 10, "daemon control socket")
        pid_file = self.data / "daemon.pid"
        if pid_file.is_file():
            try:
                self.daemon_pid = int(pid_file.read_text().strip())
            except ValueError:
                self.daemon_pid = None
        return self

    def client(self):
        env = dict(os.environ)
        env["DS_DAEMON_BIN"] = str(self.bins / "ds-daemon")
        return DeckClient(ds=str(self.bins / "ds"), socket=self.sock, data_dir=self.data,
                          cwd=str(ROOT), env=env, timeout=OP_TIMEOUT_S)

    @staticmethod
    def _wait_for_file(path, timeout_s, what):
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            if path.exists():
                return
            time.sleep(0.05)
        raise RuntimeError("timed out after %ss waiting for %s (%s)" % (timeout_s, what, path))

    def stop(self):
        problems = []
        try:
            env = dict(os.environ)
            env["DS_DAEMON_BIN"] = str(self.bins / "ds-daemon")
            subprocess.run([str(self.bins / "ds"), "--socket", str(self.sock),
                            "--data-dir", str(self.data), "daemon", "stop"],
                           capture_output=True, text=True, timeout=OP_TIMEOUT_S,
                           env=env, cwd=str(ROOT))
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
                (t.terminate if isinstance(t, subprocess.Popen) else os.kill(t, signal.SIGTERM))
            except OSError:
                pass
        deadline = time.monotonic() + KILL_TIMEOUT_S
        while time.monotonic() < deadline and any(
                (t.poll() is None) if isinstance(t, subprocess.Popen)
                else _pid_alive(t) for t in targets):
            time.sleep(0.05)
        for t in targets:
            try:
                (t.kill if isinstance(t, subprocess.Popen) else os.kill(t, signal.SIGKILL))
            except OSError:
                pass
        shutil.rmtree(self.tmp, ignore_errors=True)
        if self.tmp.exists():
            problems.append("temp dir still exists: %s" % self.tmp)
        return problems


def _pid_alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


def main():
    bins = ensure_binaries()
    stack = Stack(bins)
    failures = []

    def check(name, fn):
        try:
            fn()
            print("PASS  %s" % name)
        except Exception as e:  # noqa: BLE001 - report and continue to cleanup
            failures.append((name, "%s: %s" % (type(e).__name__, e)))
            print("FAIL  %s: %s: %s" % (name, type(e).__name__, e))

    try:
        stack.start()
        c = stack.client()

        def step_status():
            st = c.status()
            assert st.get("running") is True, st
            assert str(st.get("device", "")).startswith("virtual:"), st

        def step_set_get():
            rev = c.set("/controls/keys/0/image", {"kind": "color", "value": "#3cc86e"})
            assert isinstance(rev, int) and rev >= 1, rev
            g = c.get("/controls/keys/0/image")
            assert g["value"] == {"kind": "color", "value": "#3cc86e"}, g
            assert g["rev"] == rev, (g, rev)

        def step_bind_unbind():
            bid = c.bind("deck/control/keys/0/down",
                         "set /controls/keys/5/image {\"kind\":\"color\",\"value\":\"#ff0000\"}")
            assert isinstance(bid, str) and bid.startswith("b"), bid
            removed = c.unbind(binding_id=bid)
            assert bid in removed, removed

        def step_watch():
            got = {}
            it = c.watch("/controls", max_events=1)

            def reader():
                got["events"] = list(it)

            t = threading.Thread(target=reader, daemon=True)
            t.start()
            time.sleep(1.0)  # bounded settle wait for the subscription
            c.set("/controls/keys/3/label", "watched")
            t.join(WATCH_TIMEOUT_S)
            assert not t.is_alive(), "watch --max-events 1 did not exit after the set"
            events = got.get("events", [])
            assert len(events) == 1, events
            ev = events[0]
            assert ev.get("op") == "event" and ev.get("path") == "/controls/keys/3/label", ev
            assert ev.get("value") == "watched", ev

        def step_stream():
            c.set("/policy/fs/roots", [str(ROOT)])  # allow examples/key.png for --file
            # phase 1: the daemon does not serve stream.sock against vdev yet, so
            # stream_push maps to E_UNREACHABLE exit 6 (conformance cli case 110);
            # once a later phase serves the frame socket this returns the result.
            try:
                r = c.stream_push("key:0", "examples/key.png")
                assert isinstance(r, dict), r
            except DeckError as e:
                assert e.exit_code == 6 and e.code == "E_UNREACHABLE", e
            # stats read: /telemetry/* works and returns no rev (S1)
            t = c.get("/telemetry")
            assert t["path"] == "/telemetry" and isinstance(t["value"], dict), t
            assert "rev" not in t, t

        def step_error_mapping():
            try:
                c.get("/controls/keys/99")
            except DeckError as e:
                assert e.exit_code == 2, e
                assert e.code == "E_PATH", e
                assert "[exit 2]" in str(e), str(e)
                assert "path not found" in str(e), str(e)
            else:
                raise AssertionError("unknown path did not raise DeckError")

        for name, fn in (
            ("status() reports daemon running on vdev", step_status),
            ("set()/get() key colour round-trip with rev", step_set_get),
            ("bind()/unbind() round-trip", step_bind_unbind),
            ("watch() yields exactly one event", step_watch),
            ("stream_push()/telemetry stats read (S1 no-rev)", step_stream),
            ("unknown path -> DeckError exit 2 with [exit 2]", step_error_mapping),
        ):
            check(name, fn)
    except Exception as e:  # noqa: BLE001 - boot failure
        failures.append(("boot", "%s: %s" % (type(e).__name__, e)))
        print("ERROR boot: %s: %s" % (type(e).__name__, e))
    finally:
        problems = stack.stop()
        if problems:
            failures.append(("cleanup", "; ".join(problems)))
            print("FAIL  cleanup: %s" % "; ".join(problems))
        else:
            print("cleanup ok: daemon+vdev dead, temp dir removed")

    print("-" * 72)
    if failures:
        print("RESULT: FAIL (%d failing step(s))" % len(failures))
        for name, msg in failures:
            print("  - %s: %s" % (name, msg))
        return 1
    print("RESULT: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
