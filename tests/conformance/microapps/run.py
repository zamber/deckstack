#!/usr/bin/env python3
"""deckstack micro-app authoring-route conformance suite (ticket DS-099,
tests/conformance/microapps/).

Black-box over the REAL binaries only: boots ds-vdev + ds-daemon on a fresh temp
dir and machine-checks the DS-099 deliverables:

    010 help CONTENT for every verb (not just exit 0 — DS-062 asserted presence,
        this asserts content): for all 17 help topics (top + 10 verbs + daemon
        start|stop|status + stream push|stats|stop) — `ds <topic> --help` exits 0,
        contains "Usage:", an "Exit codes:" line, and a pointer to `ds schema` /
        `ds describe`; every non-group verb help additionally carries an
        "Arguments"/flags section that names a default, exactly ONE runnable
        Example (`  ds …` line), the contract's per-verb flags, and stays
        <= 25 lines; group helps keep one usage line per subcommand
    020 gitdeck.py --once end to end against a temp git repo (clone with an
        upstream): clean → keys 0-3 state asserted via `ds get … --json`
        (colour + label), then ahead=1, then dirty, then behind=1 — each run
        re-asserted
    030 focus_panel.py: --once renders /focus after a raw-IPC `emit
        app/focus/changed` (dial label + text image asserted), and --loop
        re-renders on a watched event (the `ds watch --ndjson` path)
    040 status_strip.py --once: keys 0-3 get load/mem/time/uptime labels
        (asserted via `ds get`), one JSON line on stdout

Usage:
    python tests/conformance/microapps/run.py [--verbose] [-k SUBSTR ...]
    python tests/conformance/run.py            # runs this suite too (registered there)
    python tests/conformance/run.py -k microapps   # only these cases

Exit code 0 = all green (prints RESULT: OK), nonzero = at least one failure.
Every wait carries a timeout; the daemon and vdev are always killed (SIGTERM
then SIGKILL) and the temp dir removed, so the run can never hang or leak
processes.
"""

import json
import os
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
VERBOSE = "--verbose" in sys.argv

BUILD_TIMEOUT_S = 900
OP_TIMEOUT_S = 20
BOOT_TIMEOUT_S = 30
KILL_TIMEOUT_S = 5
SUITE_TIMEOUT_S = 600

BINARIES = ("ds", "ds-daemon", "ds-vdev", "ds-vdevctl")

EXAMPLES = ROOT / "examples" / "microapps"

# The 17 help topics of the phase-1/3 CLI (bins/ds/src/cli.rs VERBS + subs).
VERB_TOPICS = [
    "get", "set", "watch", "schema", "describe", "status", "bind", "unbind",
    "daemon", "daemon start", "daemon stop", "daemon status",
    "stream", "stream push", "stream stats", "stream stop",
]
GROUP_TOPICS = {"daemon", "stream"}
GROUP_SUBS = {
    "daemon": ("ds daemon start", "ds daemon stop", "ds daemon status"),
    "stream": ("ds stream push", "ds stream stats", "ds stream stop"),
}

# Per-topic contract flags the verb help must mention (docs/contracts/cli.md §1 +
# the DS-099 content contract: arguments/flags with defaults).
HELP_ARGS = {
    "get": ("--fields", "--rev", "--json", "default"),
    "set": ("--if-rev", "--file", "--json", "default"),
    "watch": ("--ndjson", "--from-rev", "--max-events", "--json", "default"),
    "schema": ("state", "ipc", "stream-frame", "plugin-manifest", "binding", "default"),
    "describe": ("--json", "verbs", "schema", "value", "default"),
    "status": ("--json", "default"),
    "bind": ("--do", "--when", "--json", "default"),
    "unbind": ("--topic", "--json", "default"),
    "daemon start": ("--device", "--foreground", "--data-dir", "default"),
    "daemon stop": ("--json", "default"),
    "daemon status": ("--json", "exit 6", "default"),
    "stream push": ("--file", "--dir", "--fps", "--loop", "--credits", "--json", "default"),
    "stream stats": ("--json", "default"),
    "stream stop": ("--json", "default"),
}


def log(msg):
    if VERBOSE:
        print(msg, file=sys.stderr)


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
    subprocess.run(
        [cargo, "build", "--bin", "ds", "--bin", "ds-daemon", "--bin", "ds-vdev",
         "--bin", "ds-vdevctl"],
        cwd=str(ROOT), timeout=BUILD_TIMEOUT_S, check=True,
        stdout=subprocess.DEVNULL if not VERBOSE else None,
        stderr=subprocess.DEVNULL if not VERBOSE else None,
    )
    missing = [b for b in BINARIES if not (d / b).is_file()]
    if missing:
        raise RuntimeError("cargo build finished but still missing: %s" % ", ".join(missing))
    return d


class Stack:
    """One booted ds-vdev + ds-daemon pair on a fresh temp dir."""

    def __init__(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="ds-microapps-conf-"))
        self.vdev_sock = self.tmp / "vdev.sock"
        self.data = self.tmp / "data"
        self.sock = self.data / "daemon.sock"
        self.token = ""
        self.vdev = None
        self.daemon_pid = None
        self.bins = None
        self.vdev_log = open(self.tmp / "vdev.log", "w+")
        self.daemon_log = self.data / "daemon.log"

    def start(self, bins):
        self.bins = bins
        self.vdev = subprocess.Popen(
            [str(bins / "ds-vdev"), "--socket", str(self.vdev_sock)],
            stdout=self.vdev_log, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL,
        )
        self._wait_for_file(self.vdev_sock, 10, "ds-vdev device socket")
        env = dict(os.environ)
        env["DS_DAEMON_BIN"] = str(bins / "ds-daemon")
        p = subprocess.run(
            [str(bins / "ds"), "daemon", "start",
             "--device", "virtual:%s" % self.vdev_sock, "--data-dir", str(self.data)],
            capture_output=True, text=True, timeout=BOOT_TIMEOUT_S, env=env,
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
        env = dict(os.environ)
        env["DS_DAEMON_BIN"] = str(self.bins / "ds-daemon")
        return subprocess.run(
            [str(self.bins / "ds"), "--socket", str(self.sock), "--data-dir", str(self.data)]
            + list(args),
            capture_output=True, text=True, timeout=timeout, env=env,
        )

    def ipc(self):
        return Ipc(self.sock)

    def stop(self):
        problems = []
        try:
            subprocess.run(
                [str(self.bins / "ds"), "--socket", str(self.sock),
                 "--data-dir", str(self.data), "daemon", "stop"],
                capture_output=True, text=True, timeout=OP_TIMEOUT_S,
                env={**os.environ, "DS_DAEMON_BIN": str(self.bins / "ds-daemon")},
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


class Ipc:
    """Raw NDJSON control-socket client (black box)."""

    def __init__(self, sock_path, timeout=OP_TIMEOUT_S):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(timeout)
        self.sock.connect(str(sock_path))
        self.f = self.sock.makefile("rw", encoding="utf-8", newline="\n")

    def rpc(self, obj):
        self.f.write(json.dumps(obj) + "\n")
        self.f.flush()
        line = self.f.readline()
        if not line:
            raise AssertionError("connection closed by daemon while waiting for a reply")
        return json.loads(line)

    def close(self):
        try:
            self.f.close()
        finally:
            self.sock.close()


def help_page(bins, topic):
    args = [str(bins / "ds")] + ([] if topic == "top" else topic.split()) + ["--help"]
    p = subprocess.run(args, capture_output=True, text=True, timeout=OP_TIMEOUT_S)
    return p


def example_commands(page_text):
    """The `  ds …` command lines of the `Example:` block."""
    out = []
    in_example = False
    for line in page_text.splitlines():
        if line.startswith("Example:"):
            in_example = True
            continue
        if in_example:
            if line.strip() == "":
                break
            if line.startswith("  ds "):
                out.append(line.strip())
    return out


def run_example(script, *extra, timeout=60):
    """Run one examples/microapps/ program against the suite stack."""
    return subprocess.run(
        [sys.executable, str(EXAMPLES / script)] + list(extra),
        capture_output=True, text=True, timeout=timeout,
    )


class MicroApps(unittest.TestCase):
    """DS-099 cases over a real vdev stack."""

    stack = None
    boot_error = None

    @classmethod
    def setUpClass(cls):
        try:
            bins = ensure_binaries()
            cls.bins = bins
            cls.stack = Stack()
            cls.stack.start(bins)
        except Exception as e:
            cls.boot_error = "%s: %s" % (type(e).__name__, e)
            if cls.stack is not None and VERBOSE:
                cls.stack.dump_logs()

    @classmethod
    def tearDownClass(cls):
        if cls.stack is not None:
            problems = cls.stack.stop()
            if VERBOSE:
                cls.stack.dump_logs()
            if problems:
                print("CLEANUP PROBLEMS: %s" % "; ".join(problems), file=sys.stderr)
            else:
                print("cleanup ok: daemon+vdev dead, temp dir removed, no leaked sockets")

    def setUp(self):
        if self.boot_error:
            self.fail("microapps stack did not boot: %s" % self.boot_error)
        self.ds = self.stack.ds

    # ---- helpers -----------------------------------------------------------

    def jout(self, p):
        self.assertEqual(p.returncode, 0, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        return json.loads(p.stdout)

    def get_json(self, path):
        return self.jout(self.ds("get", path, "--json"))

    def key(self, k, leaf):
        return self.get_json("/controls/keys/%d/%s" % (k, leaf))["value"]

    # ---- 010 help CONTENT for every verb ------------------------------------

    def test_010_help_content_for_every_verb(self):
        # top: the index — exit codes table + the authoring-route pointer
        p = help_page(self.bins, "top")
        self.assertEqual(p.returncode, 0, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        top = p.stdout
        for needle in ("Usage:", "Exit codes:", "2 E_PATH", "6 E_UNREACHABLE",
                       "migrate", "docs/microapps.md", "examples/microapps/"):
            self.assertIn(needle, top, "ds --help missing %r" % needle)
        # every verb topic: usage, exit codes, schema/describe pointer, <= 25 lines
        for topic in VERB_TOPICS:
            p = help_page(self.bins, topic)
            self.assertEqual(p.returncode, 0,
                             "%s --help rc=%d stderr=%s" % (topic, p.returncode, p.stderr))
            page = p.stdout
            self.assertIn("Usage:", page, "%s help has no Usage:" % topic)
            self.assertIn("Exit codes:", page, "%s help has no Exit codes line" % topic)
            self.assertTrue("ds schema" in page or "ds describe" in page,
                            "%s help lacks the ds schema/describe pointer:\n%s" % (topic, page))
            n_lines = len(page.splitlines())
            self.assertLessEqual(n_lines, 25, "%s help is %d lines (> 25)" % (topic, n_lines))
            if topic in GROUP_TOPICS:
                usage = page.split("Usage:", 1)[1]
                for sub in GROUP_SUBS[topic]:
                    self.assertIn(sub, usage,
                                  "%s group help missing the one-liner for %r" % (topic, sub))
            else:
                # arguments/flags with defaults + exactly ONE runnable example
                self.assertIn("default", page.lower(),
                              "%s help documents no flag defaults:\n%s" % (topic, page))
                ex = example_commands(page)
                self.assertEqual(len(ex), 1,
                                 "%s help must carry exactly one example, got %r" % (topic, ex))
                self.assertTrue(ex[0].startswith("ds "), "%s example: %r" % (topic, ex[0]))
                for needle in HELP_ARGS[topic]:
                    self.assertIn(needle, page,
                                  "%s help missing %r (contract args):\n%s" % (topic, needle, page))
        print("microapps help-content: %d verb topics + top asserted "
              "(usage/args+defaults/one example/exit codes/schema-describe pointer, <=25 lines)"
              % len(VERB_TOPICS))

    # ---- 020 gitdeck.py --once end to end against a temp git repo ------------

    def _git(self, cwd, *argv, check=True):
        p = subprocess.run(["git", "-C", str(cwd), *argv], capture_output=True, text=True,
                           timeout=OP_TIMEOUT_S)
        if check and p.returncode != 0:
            self.fail("git %s failed rc=%d: %s" % (" ".join(argv), p.returncode, p.stderr))
        return p.stdout.strip()

    def _make_repo_pair(self):
        """remote + clone (so the clone has an upstream for ahead/behind)."""
        tmp = Path(tempfile.mkdtemp(prefix="ds-gitdeck-", dir=self.stack.tmp))
        remote = tmp / "remote"
        work = tmp / "work"
        remote.mkdir()
        self._git(remote, "init", "-q")
        self._git(remote, "config", "user.email", "t@example.com")
        self._git(remote, "config", "user.name", "t")
        (remote / "f.txt").write_text("one\n")
        self._git(remote, "add", "f.txt")
        self._git(remote, "commit", "-qm", "c1")
        self._git(tmp, "clone", "-q", str(remote), str(work))
        self._git(work, "config", "user.email", "t@example.com")
        self._git(work, "config", "user.name", "t")
        branch = self._git(work, "branch", "--show-current")
        return work, branch

    def _gitdeck_once(self, repo):
        p = run_example("gitdeck.py", "--once", "--repo", str(repo),
                        "--socket", str(self.stack.sock), "--data-dir", str(self.stack.data),
                        "--ds-bin", str(self.bins / "ds"), timeout=60)
        self.assertEqual(p.returncode, 0,
                         "gitdeck rc=%d stderr=%s" % (p.returncode, p.stderr))
        return json.loads(p.stdout.strip().splitlines()[-1])

    def test_020_gitdeck_once_end_to_end(self):
        work, branch = self._make_repo_pair()
        # 1) clean clone: ahead=0 behind=0 dirty=False, branch shown on key 3
        s = self._gitdeck_once(work)
        self.assertEqual(s, {"dirty": False, "ahead": 0, "behind": 0, "branch": branch[:32]},
                         s)
        self.assertEqual(self.key(0, "image"), {"kind": "color", "value": "#00cc44"})
        self.assertEqual(self.key(0, "label"), "clean")
        self.assertEqual(self.key(1, "label"), "ahead 0")
        self.assertEqual(self.key(2, "label"), "behind 0")
        self.assertEqual(self.key(3, "label"), branch[:32])
        self.assertEqual(self.key(3, "image"), {"kind": "color", "value": "#3cc86e"})
        # 2) one local commit -> ahead 1 (amber key)
        (work / "f.txt").write_text("one\ntwo\n")
        self._git(work, "commit", "-qam", "c2")
        s = self._gitdeck_once(work)
        self.assertEqual(s["ahead"], 1, s)
        self.assertEqual(self.key(1, "label"), "ahead 1")
        self.assertEqual(self.key(1, "image"), {"kind": "color", "value": "#ffaa00"})
        # 3) uncommitted change -> dirty (red key 0)
        (work / "new.txt").write_text("x\n")
        s = self._gitdeck_once(work)
        self.assertTrue(s["dirty"], s)
        self.assertEqual(self.key(0, "label"), "dirty")
        self.assertEqual(self.key(0, "image"), {"kind": "color", "value": "#ff2222"})
        # 4) upstream moves -> behind 1 (magenta key 2)
        (work / "new.txt").unlink()
        remotes = list(self.stack.tmp.glob("ds-gitdeck-*/remote"))
        self.assertEqual(len(remotes), 1)
        (remotes[0] / "g.txt").write_text("g\n")
        self._git(remotes[0], "add", "g.txt")
        self._git(remotes[0], "commit", "-qm", "c9")
        self._git(work, "fetch", "-q")
        s = self._gitdeck_once(work)
        self.assertEqual(s["behind"], 1, s)
        self.assertEqual(self.key(2, "label"), "behind 1")
        self.assertEqual(self.key(2, "image"), {"kind": "color", "value": "#ff00aa"})
        print("gitdeck --once: clean/ahead/dirty/behind all asserted on keys 0-3")

    # ---- 030 focus_panel.py (--once on /focus, --loop on watch) -------------

    def _emit_focus(self, app, title):
        c = self.stack.ipc()
        try:
            c.rpc({"id": 1, "op": "hello", "protocol": 1, "client": "microapps/1"})
            c.rpc({"id": 2, "op": "auth", "token": self.stack.token})
            r = c.rpc({"id": 3, "op": "emit", "topic": "app/focus/changed",
                       "payload": {"app": app, "title": title, "pid": 4242}})
            self.assertTrue(r.get("ok"), r)
        finally:
            c.close()

    def test_030_focus_panel_once_and_loop_watch(self):
        self._emit_focus("code", "main.rs - deckstack")
        # /focus mirror exists now
        focus = self.get_json("/focus")["value"]
        self.assertEqual(focus["app"], "code")
        # --once renders /focus onto the dial + keys 0/1
        p = run_example("focus_panel.py", "--once",
                        "--socket", str(self.stack.sock), "--data-dir", str(self.stack.data),
                        "--ds-bin", str(self.bins / "ds"), timeout=60)
        self.assertEqual(p.returncode, 0, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        out = json.loads(p.stdout.strip().splitlines()[-1])
        self.assertEqual(out, {"app": "code", "title": "main.rs - deckstack"}, out)
        dial_label = self.get_json("/controls/dial/label")["value"]
        self.assertEqual(dial_label, "code")
        dial_img = self.get_json("/controls/dial/image")["value"]
        self.assertEqual(dial_img["kind"], "text")
        self.assertEqual(dial_img["value"], "code")
        self.assertEqual(self.key(0, "label"), "focus: code")
        # --loop watches app/focus/changed --ndjson and re-renders per event
        panel = subprocess.Popen(
            [sys.executable, str(EXAMPLES / "focus_panel.py"), "--loop",
             "--socket", str(self.stack.sock), "--data-dir", str(self.stack.data),
             "--ds-bin", str(self.bins / "ds")],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            time.sleep(1.5)  # let the watch subscription reach the daemon
            self._emit_focus("firefox", "Mozilla — example.org")
            deadline = time.monotonic() + 15
            label = key1 = None
            while time.monotonic() < deadline:
                label = self.get_json("/controls/dial/label")["value"]
                key1 = self.key(1, "label")
                if label == "firefox" and key1 == "Mozilla — example.org":
                    break
                time.sleep(0.25)
            self.assertEqual(label, "firefox",
                             "focus_panel --loop did not re-render the dial on the watched event")
            # the renderer writes several paths in order; poll the last one too instead of
            # reading it the instant the dial flips (raced the write order)
            self.assertEqual(key1, "Mozilla — example.org",
                             "focus_panel --loop did not re-render key 1 on the watched event")
        finally:
            panel.terminate()
            try:
                panel.communicate(timeout=KILL_TIMEOUT_S)
            except subprocess.TimeoutExpired:
                panel.kill()
                panel.communicate()
        print("focus_panel: --once (get /focus) and --loop (watch --ndjson) asserted")

    # ---- 040 status_strip.py --once ------------------------------------------

    def test_040_status_strip_once(self):
        p = run_example("status_strip.py", "--once",
                        "--socket", str(self.stack.sock), "--data-dir", str(self.stack.data),
                        "--ds-bin", str(self.bins / "ds"), timeout=60)
        self.assertEqual(p.returncode, 0, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        out = json.loads(p.stdout.strip().splitlines()[-1])
        for k in ("load", "mem_pct", "clock", "uptime"):
            self.assertIn(k, out, out)
        self.assertRegex(out["clock"], r"^\d{2}:\d{2}$")
        self.assertEqual(self.key(0, "label"), "load " + out["load"])
        self.assertEqual(self.key(1, "label"), "mem %d%%" % out["mem_pct"])
        self.assertEqual(self.key(2, "label"), out["clock"])
        self.assertEqual(self.key(3, "label"), "up " + out["uptime"])
        img0 = self.key(0, "image")
        self.assertEqual(img0["kind"], "color")
        self.assertRegex(img0["value"], r"^#[0-9a-f]{6}$")
        print("status_strip --once: keys 0-3 labels + colour asserted")


class LineResult(unittest.TextTestResult):
    """One clear line per test."""

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


def _alarm(signum, frame):
    raise TimeoutError("microapps suite exceeded the %ss watchdog" % SUITE_TIMEOUT_S)


def microapps_suite():
    """The suite object registered by tests/conformance/run.py."""
    suite = unittest.TestSuite()
    suite.addTests(unittest.defaultTestLoader.loadTestsFromTestCase(MicroApps))
    return suite


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


def main(argv):
    patterns = _parse_k(argv)
    signal.signal(signal.SIGALRM, _alarm)
    signal.alarm(SUITE_TIMEOUT_S)
    try:
        suite = microapps_suite()
        if patterns:
            suite = _filter_suite(suite, patterns)
        runner = unittest.TextTestRunner(stream=sys.stdout, verbosity=0,
                                         resultclass=LineResult)
        result = runner.run(suite)
    finally:
        signal.alarm(0)
    n = result.testsRun
    bad = len(result.failures) + len(result.errors)
    print("-" * 60)
    print("microapps conformance: %d tests, %d passed, %d failed/errored, %d skipped"
          % (n, n - bad - len(result.skipped), bad, len(result.skipped)))
    if bad:
        print("RESULT: FAIL")
        return 1
    print("RESULT: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
