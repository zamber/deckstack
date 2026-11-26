#!/usr/bin/env python3
"""deckstack CLI self-description + error-contract conformance suite (ticket DS-062,
tests/conformance/cli/).

Black-box tests over the REAL `ds` binary only: boots ds-vdev + ds-daemon on a fresh
temp dir and drives the system through the `ds` binary (plus, for the exit codes that
are not reachable against a healthy vdev stack, a scripted fake control socket that
answers hello/auth and then returns a canned error envelope — that exercises the CLI's
envelope→exit-code mapping, which is the contract under test). It machine-checks
docs/contracts/cli.md §1-§5 (the normative contract):

    010 recursive help walk: parse `ds --help`, assert every advertised verb has
        `ds <verb> --help` exit 0, every group subcommand has `ds <group> <sub>
        --help` exit 0, and every discovered verb/sub is covered by a test (the walk
        parses the help text, never string-matches; the coverage report is printed)
    020 per-verb help mentions the flags/verbs the contract lists for it (spot-check
        of every phase-1 verb: get set watch schema describe daemon status stream
        bind unbind, plus daemon start|stop|status and stream push|stats|stop)
    030 later-phase verbs (emit events plugin token) are refused exit 1 until their
        tickets land; the moment one answers --help with exit 0 the suite FAILS and
        demands a walk entry — coverage never silently passes
    040 `ds schema <name>` for all five schemas parses, carries the $id of
        schemas/<file>.json and is semantically equal to the file on disk;
        `--json`/`--compact` shapes; unknown name -> exit 1 with the valid list
    050 `ds describe <path>` prints schema + current value + allowed verbs (human
        and --json); unknown path -> exit 2
    060 exit 0: success shapes (human `ok rev=N`, --json one object)
    070 exit 1: usage/schema — unknown verb, unknown flag (actionable "try `ds
        --help`"), `watch --json`, schema-violating set, `schema nope`, bind w/o --do
    080 exit 2 E_PATH: unknown paths, human `[exit 2]` suffix + --json envelope
    090 exit 3 E_CONFLICT: `get --rev` mismatch and `set --if-rev` mismatch, with
        `current_rev` in the payload
    100 exit 4 E_AUTH/E_SCOPE/E_CAP: unreadable token file, unknown token, read-scope
        token doing a set (get stays 0), `set --file` outside /policy/fs/roots
    110 exit 5 E_LIMIT: a >16 MB stream-push file (client-side wire cap)
    120 exit 6 E_UNREACHABLE: missing control socket (message carries the exact
        `ds daemon start` hint); stream push with no frame socket also exits 6
    130 output modes: `--json` prints exactly one JSON object on stdout (empty
        stderr) for get/schema/describe/status/daemon status; `--compact` one line;
        stream stats on a never-streamed target -> exit 2 with one JSON envelope
    140 `--ndjson` streams for watch: one JSON object per line after a set; human
        watch prints `event <path> rev=N`
    150 no `migrate` verb: `ds migrate …` exits 1 with the scope message
    160 `set {}` clears an object path. A `null` value is rejected with E_SCHEMA
         and exit 1.
    200/210/220 (fake control socket) the full E_* -> exit-code table 0-8 with the
        exact `ds: …  [exit N]` stderr shape and the one-object --json error envelope:
        E_DEVICE -> 7, E_VERSION/E_INTERNAL -> 8, malformed reply line -> 8, and every
        other code pinned (E_SCHEMA 1, E_PATH 2, E_CONFLICT 3, E_AUTH/E_SCOPE/E_CAP 4,
        E_LIMIT 5, E_UNREACHABLE 6)

Usage:
    python3 tests/conformance/cli/run.py [--verbose] [-k SUBSTR ...]
    python3 tests/conformance/run.py            # runs this suite too (registered there)
    python3 tests/conformance/run.py -k cli     # only the CLI contract cases

Exit code 0 = all green. Every wait carries a timeout; the daemon and vdev are always
killed (SIGTERM then SIGKILL) and the temp dir removed, so the run can never hang or
leak processes.
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
import threading
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
VERBOSE = "--verbose" in sys.argv

BUILD_TIMEOUT_S = 900
OP_TIMEOUT_S = 20
BOOT_TIMEOUT_S = 30
WATCH_TIMEOUT_S = 20
KILL_TIMEOUT_S = 5
SUITE_TIMEOUT_S = 600

BINARIES = ("ds", "ds-daemon", "ds-vdev", "ds-vdevctl")

# The phase-1 CLI surface (bins/ds/src/cli.rs VERBS + the DS-032 stream group) — pinned
# from day one (DS-062 risk note: exit-code/verb drift must fail loudly here).
ADVERTISED_VERBS = {
    "get", "set", "watch", "schema", "describe", "daemon", "status", "bind", "unbind",
    "stream", "colors",
}
EXPECTED_SUBS = {
    "daemon": {"start", "stop", "status"},
    "stream": {"push", "stats", "stop"},
}
# Recognized but refused with exit 1 until their tickets land (cli.rs LATER_PHASE).
LATER_PHASE_VERBS = ("emit", "events", "plugin", "token")

# ds schema <name> -> schemas/<file>
SCHEMA_FILES = {
    "state": "state.json",
    "ipc": "ipc-message.json",
    "stream-frame": "stream-frame.json",
    "plugin-manifest": "plugin-manifest.json",
    "binding": "binding.json",
}

# Per-topic substrings the contract (docs/contracts/cli.md §1) lists for each help page.
HELP_NEEDLES = {
    "top": ("--socket", "--token-file", "--data-dir", "--json", "--ndjson", "--compact",
            "--help", "--version", "2 E_PATH", "3 E_CONFLICT", "4 E_AUTH", "5 E_LIMIT",
            "6 E_UNREACHABLE", "7 E_DEVICE", "8 E_INTERNAL", "migrate"),
    "get": ("--fields", "--rev", "--json"),
    "set": ("--if-rev", "--file", "--json"),
    "colors": ("--if-rev", "--json"),
    "watch": ("--ndjson", "--from-rev", "--max-events", "--json"),
    "schema": tuple(SCHEMA_FILES),
    "describe": ("--json", "verbs", "schema", "value"),
    "status": ("--json",),
    "bind": ("--do", "--when", "--json"),
    "unbind": ("--topic", "--json"),
    "daemon": ("start", "stop", "status"),
    "daemon start": ("--device", "--foreground"),
    "daemon stop": ("--json",),
    "daemon status": ("--json", "exit 6"),
    "stream": ("push", "stats", "stop"),
    "stream push": ("--file", "--dir", "--fps", "--loop", "--json"),
    "stream stats": ("--json",),
    "stream stop": ("--json",),
}

# Every advertised verb/sub mapped to the test(s) that exercise it — the walk fails if
# a discovered verb has no entry here (coverage is never silently passed).
VERB_COVERAGE = {
    "get": ("test_cli_060_exit_0_success_shapes", "test_cli_130_json_single_object"),
    "set": ("test_cli_060_exit_0_success_shapes", "test_cli_090_exit_3_conflict"),
    "watch": ("test_cli_140_ndjson_streaming_for_watch",),
    "schema": ("test_cli_040_schema_verb_matches_schemas_on_disk",),
    "describe": ("test_cli_050_describe_prints_schema_value_verbs",),
    "daemon": ("test_cli_060_exit_0_success_shapes", "test_cli_120_exit_6_unreachable"),
    "status": ("test_cli_130_json_single_object",),
    "bind": ("test_cli_070_exit_1_usage_and_schema",),
    "unbind": ("test_cli_020_help_mentions_contract_flags",),
    "stream": ("test_cli_110_exit_5_limit", "test_cli_120_exit_6_unreachable"),
    "daemon start": ("test_cli_120_exit_6_unreachable",),
    "daemon stop": ("test_cli_060_exit_0_success_shapes",),
    "daemon status": ("test_cli_130_json_single_object",),
    "stream push": ("test_cli_110_exit_5_limit",),
    "stream stats": ("test_cli_130_json_single_object",),
    "stream stop": ("test_cli_020_help_mentions_contract_flags",),
    # DS-111: bulk key colours; test_cli_170 covers the verb.
    "colors": ("test_cli_170_colors_verb_bulk_panel_push",),
}

# The full E_* -> exit table of docs/contracts/cli.md §3 (driven through a scripted
# fake control socket in CliEnvelopeMapping, and on the real stack where reachable).
EXIT_TABLE = [
    ("E_SCHEMA", 1), ("E_PATH", 2), ("E_CONFLICT", 3),
    ("E_AUTH", 4), ("E_SCOPE", 4), ("E_CAP", 4),
    ("E_LIMIT", 5), ("E_UNREACHABLE", 6), ("E_DEVICE", 7),
    ("E_VERSION", 8), ("E_INTERNAL", 8),
]


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
        self.tmp = Path(tempfile.mkdtemp(prefix="ds-cli-conf-"))
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
        """Run the real `ds` binary against this stack. Returns CompletedProcess."""
        env = dict(os.environ)
        env["DS_DAEMON_BIN"] = str(self.bins / "ds-daemon")
        return subprocess.run(
            [str(self.bins / "ds"), "--socket", str(self.sock), "--data-dir", str(self.data)]
            + list(args),
            capture_output=True, text=True, timeout=timeout, env=env,
        )

    def ds_raw(self, *args, timeout=OP_TIMEOUT_S):
        """Run `ds` with exactly the given argv (no injected --socket/--data-dir)."""
        env = dict(os.environ)
        env["DS_DAEMON_BIN"] = str(self.bins / "ds-daemon")
        return subprocess.run([str(self.bins / "ds")] + list(args),
                              capture_output=True, text=True, timeout=timeout, env=env)

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


def parse_help_verbs(help_text):
    """Parse the `Verbs (<phase>):` blocks of `ds --help` — the walk parses, it does
    not string-match. Returns {verb: phase_label}."""
    verbs = {}
    lines = help_text.splitlines()
    i = 0
    while i < len(lines):
        m = re.match(r"^Verbs \(([^)]+)\):$", lines[i])
        if m:
            phase = m.group(1)
            for l in lines[i + 1:]:
                if not l.startswith("  "):
                    break
                if l.startswith("   ") or l[2:3] == " ":
                    continue  # wrapped description of the previous verb
                verbs.setdefault(l.split()[0], phase)
        i += 1
    return verbs


def parse_group_subs(verb, help_text):
    """Parse the `Usage:` block of `ds <verb> --help` for `ds <verb> <sub> …` lines."""
    subs = []
    in_usage = False
    for l in help_text.splitlines():
        if l.startswith("Usage:"):
            in_usage = True
            continue
        if not in_usage:
            continue
        if not l.startswith("  "):
            break
        m = re.match(r"^\s+ds " + re.escape(verb) + r" (\S+)", l)
        if m and re.fullmatch(r"[a-z][a-z0-9-]*", m.group(1)):
            if m.group(1) not in subs:
                subs.append(m.group(1))
    return subs


class FakeControl:
    """A scripted fake ds-daemon control socket on a temp path: answers `hello` and
    `auth`, then replies to the first real op with the canned error envelope (or a
    malformed line). Used to drive the exit codes not reachable against a healthy
    vdev stack (E_DEVICE/E_VERSION/E_INTERNAL) and to pin the whole E_* table."""

    def __init__(self, reply):
        self.tmp = Path(tempfile.mkdtemp(prefix="ds-cli-fake-"))
        self.path = self.tmp / "daemon.sock"
        self.reply = reply
        self.token_file = self.tmp / "token"
        self.token_file.write_text("f" * 64 + "\n")
        self.token_file.chmod(0o600)
        self.srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.srv.bind(str(self.path))
        self.srv.listen(4)
        self._thread = threading.Thread(target=self._loop, daemon=True)
        self._thread.start()

    def _loop(self):
        while True:
            try:
                conn, _ = self.srv.accept()
            except OSError:
                return
            threading.Thread(target=self._handle, args=(conn,), daemon=True).start()

    def _handle(self, conn):
        try:
            f = conn.makefile("rw", encoding="utf-8", newline="\n")
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    req = json.loads(line)
                except ValueError:
                    continue
                rid, op = req.get("id"), req.get("op")
                if op == "hello":
                    f.write(json.dumps({
                        "id": rid, "ok": True,
                        "result": {"op": "welcome", "protocol": 1, "rev": 0,
                                   "server": "ds-daemon/fake/0"},
                    }) + "\n")
                elif op == "auth":
                    f.write(json.dumps({"id": rid, "ok": True,
                                        "result": {"scope": "admin"}}) + "\n")
                else:
                    if self.reply == "MALFORMED":
                        f.write("this is not json\n")
                    else:
                        f.write(json.dumps({"id": rid, "ok": False,
                                            "error": self.reply}) + "\n")
                    f.flush()
                    return
                f.flush()
        except OSError:
            pass
        finally:
            try:
                conn.close()
            except OSError:
                pass

    def ds(self, bins, *args, timeout=OP_TIMEOUT_S):
        env = dict(os.environ)
        env["DS_DAEMON_BIN"] = str(bins / "ds-daemon")
        return subprocess.run(
            [str(bins / "ds"), "--socket", str(self.path),
             "--token-file", str(self.token_file)] + list(args),
            capture_output=True, text=True, timeout=timeout, env=env,
        )

    def close(self):
        try:
            self.srv.close()
        except OSError:
            pass
        shutil.rmtree(self.tmp, ignore_errors=True)


class CliContract(unittest.TestCase):
    """The DS-062 CLI self-description/error-contract cases over a real vdev stack."""

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
            self.fail("cli stack did not boot: %s" % self.boot_error)
        self.ds = self.stack.ds

    # ---- helpers -----------------------------------------------------------

    def jout(self, p):
        self.assertEqual(p.returncode, 0, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        return json.loads(p.stdout)

    def get_json(self, path):
        return self.jout(self.ds("get", path, "--json"))

    def rev(self):
        return self.get_json("/meta")["rev"]

    def assert_exit(self, p, code, *needles):
        self.assertEqual(p.returncode, code,
                         "rc=%d (want %d) stderr=%s" % (p.returncode, code, p.stderr))
        self.assertIn("[exit %d]" % code, p.stderr, "stderr=%s" % p.stderr)
        for n in needles:
            self.assertIn(n, p.stderr, "stderr=%s" % p.stderr)

    def err_envelope(self, p, code):
        """The --json error contract: stderr is exactly one JSON object {ok:false,…}."""
        env = json.loads(p.stderr)
        self.assertEqual(env["ok"], False)
        self.assertEqual(env["error"]["code"], code)
        return env

    # ---- 010 help walk ------------------------------------------------------

    def test_cli_010_help_walk_covers_every_advertised_verb(self):
        p = self.ds("--help")
        self.assertEqual(p.returncode, 0, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        top = p.stdout
        verbs = parse_help_verbs(top)
        self.assertEqual(set(verbs), ADVERTISED_VERBS,
                         "ds --help advertises %s, expected %s — pin the walk or the "
                         "contract changed" % (sorted(verbs), sorted(ADVERTISED_VERBS)))
        # every advertised verb: `ds <verb> --help` exits 0 with a Usage block
        for v in sorted(verbs):
            hv = self.ds(v, "--help")
            self.assertEqual(hv.returncode, 0, "%s --help rc=%d" % (v, hv.returncode))
            self.assertIn("Usage:", hv.stdout, "%s --help has no Usage:" % v)
            self.assertIn("ds", hv.stdout)
            self.assertTrue(hv.stdout.strip())
        # every group subcommand: `ds <group> <sub> --help` exits 0
        subs_found = {}
        for v in sorted(verbs):
            subs = parse_group_subs(v, self.ds(v, "--help").stdout)
            if subs:
                subs_found[v] = subs
        self.assertEqual({k: set(v) for k, v in subs_found.items()}, EXPECTED_SUBS,
                         "group subcommands discovered: %s" % subs_found)
        for v, subs in subs_found.items():
            for s in subs:
                hv = self.ds(v, s, "--help")
                self.assertEqual(hv.returncode, 0,
                                 "%s %s --help rc=%d stderr=%s"
                                 % (v, s, hv.returncode, hv.stderr))
                self.assertIn("Usage:", hv.stdout)
        # top-level aliases
        self.assertEqual(self.ds("-h").returncode, 0)
        pv = self.ds("--version")
        self.assertEqual(pv.returncode, 0)
        self.assertRegex(pv.stdout.strip(), r"^ds \d+\.\d+\.\d+")
        # coverage: every discovered verb/sub must have a test (never silently pass)
        discovered = set(verbs) | {"%s %s" % (v, s) for v, subs in subs_found.items()
                                  for s in subs}
        missing = discovered - set(VERB_COVERAGE)
        self.assertEqual(missing, set(),
                         "help walk discovered %s with no covering test in VERB_COVERAGE"
                         % sorted(missing))
        print("cli help-walk coverage: verbs tested=%s subcommands=%s pending(later "
              "phase)=%s" % (sorted(verbs),
                             {v: s for v, s in subs_found.items()},
                             list(LATER_PHASE_VERBS)))

    # ---- 020 per-verb help mentions the contract surface --------------------

    def test_cli_020_help_mentions_contract_flags(self):
        pages = {"top": self.ds("--help").stdout}
        for v in sorted(ADVERTISED_VERBS):
            pages[v] = self.ds(v, "--help").stdout
            for s in parse_group_subs(v, pages[v]):
                pages["%s %s" % (v, s)] = self.ds(v, s, "--help").stdout
        for topic, needles in HELP_NEEDLES.items():
            self.assertIn(topic, pages, "no help page parsed for %r" % topic)
            for n in needles:
                self.assertIn(n, pages[topic],
                              "ds %s --help does not mention %r (contract §1):\n%s"
                              % (topic, n, pages[topic]))

    # ---- 030 later-phase verbs ----------------------------------------------

    def test_cli_030_later_phase_verbs_refused_until_landed(self):
        for v in LATER_PHASE_VERBS:
            p = self.ds(v, "--help")
            if p.returncode == 0:
                self.fail("verb %r now answers --help with exit 0 — its ticket landed; "
                          "move it into ADVERTISED_VERBS/VERB_COVERAGE and add cases"
                          % v)
            self.assertEqual(p.returncode, 1, "%s rc=%d" % (v, p.returncode))
            self.assertIn("not implemented", p.stderr, "%s stderr=%s" % (v, p.stderr))
            self.assertIn("[exit 1]", p.stderr)
        print("cli coverage report: later-phase verbs pending (refused exit 1): %s"
              % ", ".join(LATER_PHASE_VERBS))

    # ---- 040 ds schema <name> ------------------------------------------------

    def test_cli_040_schema_verb_matches_schemas_on_disk(self):
        for name, fname in SCHEMA_FILES.items():
            p = self.ds("schema", name)
            self.assertEqual(p.returncode, 0, "%s rc=%d stderr=%s"
                             % (name, p.returncode, p.stderr))
            self.assertEqual(p.stderr, "", "schema stdout must be data only")
            printed = json.loads(p.stdout)  # exactly one JSON object
            on_disk = json.loads((ROOT / "schemas" / fname).read_text())
            self.assertEqual(printed["$id"], on_disk["$id"], name)
            self.assertEqual(printed, on_disk,
                             "ds schema %s differs from schemas/%s" % (name, fname))
            # --json: same single object; --compact: one line
            pj = self.ds("schema", name, "--json")
            self.assertEqual(pj.returncode, 0)
            self.assertEqual(json.loads(pj.stdout), on_disk, name)
            pc = self.ds("schema", name, "--compact")
            self.assertEqual(pc.returncode, 0)
            self.assertEqual(len(pc.stdout.strip().splitlines()), 1, "--compact one line")
            self.assertEqual(json.loads(pc.stdout), on_disk, name)
        # unknown name -> exit 1 with the valid list (actionable)
        p = self.ds("schema", "nope")
        self.assert_exit(p, 1, "unknown schema name", "state", "binding")
        p = self.ds("schema")
        self.assert_exit(p, 1, "needs a name")
        # --json error envelope on stderr, stdout stays empty
        p = self.ds("schema", "nope", "--json")
        self.assertEqual(p.returncode, 1)
        self.assertEqual(p.stdout, "")
        self.err_envelope(p, "E_SCHEMA")

    # ---- 050 ds describe -----------------------------------------------------

    def test_cli_050_describe_prints_schema_value_verbs(self):
        # a path must exist to be describable: write a value first
        p = self.ds("set", "/controls/keys/0/label", '"CliSuite"')
        self.assertEqual(p.returncode, 0, p.stderr)
        # human mode: path / rev / value / verbs / schema lines
        p = self.ds("describe", "/controls/keys/0")
        self.assertEqual(p.returncode, 0, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        for needle in ("path: /controls/keys/0", "rev:", "value:", "CliSuite", "verbs:",
                       "schema:"):
            self.assertIn(needle, p.stdout, "describe human output:\n%s" % p.stdout)
        # --json: one object with schema + current value + allowed verbs
        d = self.jout(self.ds("describe", "/controls/keys/0", "--json"))
        self.assertEqual(d["path"], "/controls/keys/0")
        self.assertEqual(d["value"].get("label"), "CliSuite")
        self.assertIsInstance(d["rev"], int)
        self.assertEqual(set(d["verbs"]), {"get", "set", "watch", "describe"})
        self.assertIsInstance(d["schema"], dict)
        self.assertIn("$ref", d["schema"])
        self.assertIn("#/$defs/screenControl", d["schema"]["$ref"])
        # leaf path: the visual subschema
        p = self.ds("set", "/controls/keys/0/image", "--json",
                    '{"kind":"color","value":"#ff0000"}')
        self.assertEqual(p.returncode, 0, p.stderr)
        d = self.jout(self.ds("describe", "/controls/keys/0/image", "--json"))
        self.assertIn("#/$defs/visual", d["schema"]["$ref"])
        self.assertEqual(d["value"], {"kind": "color", "value": "#ff0000"})
        # telemetry paths: get/watch/describe only (no set)
        d = self.jout(self.ds("describe", "/telemetry", "--json"))
        self.assertEqual(set(d["verbs"]), {"get", "watch", "describe"})
        self.assertNotIn("rev", d, "telemetry describe carries no rev (S1)")
        # unknown path -> exit 2 (message text varies by path validation level; the
        # stable contract is the exit code, the [exit 2] suffix and the path itself)
        p = self.ds("describe", "/nope")
        self.assert_exit(p, 2, "/nope")
        p = self.ds("describe", "/nope", "--json")
        self.assertEqual(p.returncode, 2)
        self.err_envelope(p, "E_PATH")

    # ---- 060 exit 0 ----------------------------------------------------------

    def test_cli_060_exit_0_success_shapes(self):
        # human get: the value, one line per fact
        p = self.ds("get", "/controls/keys/0")
        self.assertEqual(p.returncode, 0, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        self.assertIn("CliSuite", p.stdout)
        self.assertEqual(p.stderr, "")
        # human set: `ok rev=N`
        p = self.ds("set", "/controls/keys/1/label", '"ok-shape"')
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertRegex(p.stdout.strip(), r"^ok rev=\d+$")
        # --json set: exactly one object {"ok":true,"rev":N}
        env = self.jout(self.ds("set", "/controls/keys/1/label", '"ok-json"', "--json"))
        self.assertEqual(env["ok"], True)
        self.assertIsInstance(env["rev"], int)
        # status / daemon status human shapes
        p = self.ds("status")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertIn("ok running", p.stdout)
        self.assertIn("client=ds/", p.stdout)
        p = self.ds("daemon", "status")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertIn("ok running", p.stdout)
        self.assertIn("rev=", p.stdout)
        self.assertIn("device=virtual:", p.stdout)

    # ---- 070 exit 1 (usage/schema) -------------------------------------------

    def test_cli_070_exit_1_usage_and_schema(self):
        # unknown verb: actionable (points at ds --help)
        p = self.ds("frobnicate")
        self.assert_exit(p, 1, "unknown verb", "try `ds --help`")
        # unknown flag: actionable
        p = self.ds("get", "/controls", "--bogus")
        self.assert_exit(p, 1, "unknown argument '--bogus'", "try `ds --help`")
        # --json on a streaming verb: the contract usage error (S9), exit 1; with
        # --json in argv the stderr error is the one-object JSON envelope
        p = self.ds("watch", "/controls", "--json")
        self.assertEqual(p.returncode, 1, "rc=%d" % p.returncode)
        self.assertIn("--json is not valid for streaming verbs; use --ndjson", p.stderr)
        env = json.loads(p.stderr)
        self.assertEqual(env["ok"], False)
        self.assertEqual(env["error"]["code"], "E_SCHEMA")
        # --ndjson on a non-streaming verb
        p = self.ds("get", "/controls", "--ndjson")
        self.assert_exit(p, 1, "--ndjson is only valid for streaming verbs")
        # schema violation on set (daemon-side validation): exit 1, E_SCHEMA
        p = self.ds("set", "/controls/keys/0/image", "--json",
                    '{"kind":"color","value":"red"}')
        self.assertEqual(p.returncode, 1, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        self.assertIn("red", p.stderr, "the error names the offending value")
        self.err_envelope(p, "E_SCHEMA")
        # human mode (no --json): human text with the [exit 1] suffix
        p = self.ds("set", "/controls/keys/0/image", '{"kind":"color","value":"red"}')
        self.assert_exit(p, 1, "red")
        # missing arguments
        self.assert_exit(self.ds("get"), 1, "needs a path")
        self.assert_exit(self.ds("bind", "deck/x"), 1, "at least one --do")
        self.assert_exit(self.ds("schema", "nope"), 1, "unknown schema name")

    # ---- 080 exit 2 (E_PATH) --------------------------------------------------

    def test_cli_080_exit_2_E_PATH(self):
        # message text varies by path validation level ("unknown top-level path" /
        # "path not found: …"); the stable contract is exit 2, the [exit 2] suffix and
        # the offending path in the message
        for path in ("/nope", "/controls/keys/99", "/does/not/exist"):
            # the message names the offending (top-level) path segment
            seg = "/" + path.strip("/").split("/")[0]
            p = self.ds("get", path)
            self.assert_exit(p, 2, seg)
            p = self.ds("get", path, "--json")
            self.assertEqual(p.returncode, 2, "%s --json rc=%d" % (path, p.returncode))
            self.assertEqual(p.stdout, "", "errors never go to stdout")
            self.err_envelope(p, "E_PATH")

    # ---- 090 exit 3 (E_CONFLICT) ----------------------------------------------

    def test_cli_090_exit_3_conflict(self):
        cur = self.rev()
        # get --rev mismatch
        p = self.ds("get", "/controls", "--rev", str(cur + 1000))
        self.assert_exit(p, 3, "rev mismatch")
        env = self.err_envelope(self.ds("get", "/controls", "--rev", str(cur + 1000),
                                        "--json"), "E_CONFLICT")
        self.assertEqual(env["error"]["current_rev"], cur,
                         "E_CONFLICT payload carries current_rev (contract §3)")
        # set --if-rev mismatch
        p = self.ds("set", "/controls/keys/2/label", '"x"', "--if-rev", str(cur + 500))
        self.assert_exit(p, 3)
        env = self.err_envelope(
            self.ds("set", "/controls/keys/2/label", '"x"', "--if-rev", str(cur + 500),
                    "--json"), "E_CONFLICT")
        self.assertEqual(env["error"]["current_rev"], cur)
        # matching --if-rev succeeds
        p = self.ds("set", "/controls/keys/2/label", '"x"', "--if-rev", str(cur))
        self.assertEqual(p.returncode, 0, "rc=%d stderr=%s" % (p.returncode, p.stderr))

    # ---- 100 exit 4 (E_AUTH / E_SCOPE / E_CAP) ---------------------------------

    def test_cli_100_exit_4_auth_scope_cap(self):
        # E_AUTH: unreadable token file — never an unauthenticated fallback
        p = self.ds("--token-file", str(self.stack.tmp / "missing-token"),
                    "get", "/controls")
        self.assert_exit(p, 4, "cannot read token file")
        env = self.err_envelope(
            self.ds("--token-file", str(self.stack.tmp / "missing-token"),
                    "get", "/controls", "--json"), "E_AUTH")
        self.assertIn("E_AUTH", json.dumps(env))
        # E_AUTH: well-formed but unknown token
        badtok = self.stack.tmp / "badtok"
        badtok.write_text("b" * 64 + "\n")
        p = self.ds("--token-file", str(badtok), "get", "/controls")
        self.assert_exit(p, 4, "bad token")
        self.err_envelope(
            self.ds("--token-file", str(badtok), "get", "/controls", "--json"), "E_AUTH")
        # E_SCOPE: a read-scope token may read but not mutate (scoped tokens land in
        # <data>/tokens.json, 0600; the daemon re-reads it per request)
        readtok = "r" * 64
        tokens_json = self.stack.data / "tokens.json"
        tokens_json.write_text(json.dumps(
            {"tokens": [{"id": "t-read", "token": readtok, "scope": "read"}]}))
        tokens_json.chmod(0o600)
        rtok_file = self.stack.tmp / "readtok"
        rtok_file.write_text(readtok + "\n")
        p = self.ds("--token-file", str(rtok_file), "get", "/controls", "--json")
        self.assertEqual(p.returncode, 0, "read scope may read: %s" % p.stderr)
        p = self.ds("--token-file", str(rtok_file),
                    "set", "/controls/keys/3/label", '"nope"')
        self.assert_exit(p, 4, "scope")
        env = self.err_envelope(
            self.ds("--token-file", str(rtok_file), "set", "/controls/keys/3/label",
                    '"nope"', "--json"), "E_SCOPE")
        self.assertIn("mutate", env["error"]["message"])
        # E_CAP: set --file outside /policy/fs/roots (and /tmp) is refused, exit 4,
        # with an actionable message
        outside = ROOT / "schemas" / "state.json"
        self.assertFalse(str(outside).startswith("/tmp"))
        p = self.ds("set", "/controls/keys/0/image", "--file", str(outside))
        self.assert_exit(p, 4, "/policy/fs/roots")
        self.err_envelope(
            self.ds("set", "/controls/keys/0/image", "--file", str(outside), "--json"),
            "E_CAP")

    # ---- 110 exit 5 (E_LIMIT) ---------------------------------------------------

    def test_cli_110_exit_5_limit(self):
        # a frame over the 16 MB wire cap is refused client-side with E_LIMIT, exit 5
        big = self.stack.tmp / "big.bin"
        with open(big, "wb") as f:
            f.write(b"\x89PNG\r\n\x1a\n")
            f.write(b"0" * (17 * 1024 * 1024))
        p = self.ds("stream", "push", "keys", "--file", str(big))
        self.assert_exit(p, 5, "16 MB")
        env = self.err_envelope(
            self.ds("stream", "push", "keys", "--file", str(big), "--json"), "E_LIMIT")
        self.assertIn("wire cap", env["error"]["message"])

    # ---- 120 exit 6 (E_UNREACHABLE) ---------------------------------------------

    def test_cli_120_exit_6_unreachable(self):
        missing = self.stack.tmp / "missing" / "daemon.sock"
        # human: the exact hint from contract §5
        p = self.stack.ds_raw("--socket", str(missing), "get", "/controls")
        self.assert_exit(p, 6, "daemon unreachable", "ds daemon start")
        # --json: one E_UNREACHABLE envelope on stderr
        p = self.stack.ds_raw("--socket", str(missing), "get", "/controls", "--json")
        self.assertEqual(p.returncode, 6)
        self.assertEqual(p.stdout, "")
        self.err_envelope(p, "E_UNREACHABLE")
        # daemon status against a dead socket: same exit 6 + hint
        p = self.stack.ds_raw("--socket", str(missing), "daemon", "status")
        self.assert_exit(p, 6, "daemon unreachable", "ds daemon start")
        # stream push against a dead control socket: exit 6 too (the frame plane itself
        # is bound by the daemon, so a live daemon never has a missing stream socket)
        p = self.stack.ds_raw(
            "--socket", str(missing), "stream", "push", "key:0",
            "--file", str(ROOT / "examples/key.png"))
        self.assert_exit(p, 6, "daemon unreachable", "ds daemon start")

    # ---- 130 output modes --------------------------------------------------------

    def test_cli_130_json_single_object(self):
        p = self.ds("set", "/controls/keys/4/label", '"json-mode"')
        self.assertEqual(p.returncode, 0, p.stderr)
        # --json: EXACTLY one JSON object on stdout (json.loads fails on trailing
        # data), stderr empty — for get / schema / describe / status / daemon status
        cases = [
            (("get", "/controls/keys/4"), ("path", "value", "rev")),
            (("schema", "state"), ("$id",)),
            (("describe", "/controls/keys/4"), ("path", "value", "verbs", "schema", "rev")),
        ]
        for args, keys in cases:
            p = self.ds(*args, "--json")
            self.assertEqual(p.returncode, 0, "%s rc=%d stderr=%s"
                             % (args, p.returncode, p.stderr))
            self.assertEqual(p.stderr, "", "%s: stdout=data only" % (args,))
            obj = json.loads(p.stdout)
            self.assertIsInstance(obj, dict, "%s --json must print one object" % (args,))
            for k in keys:
                self.assertIn(k, obj, "%s --json object keys: %s" % (args, sorted(obj)))
        for args in (("status",), ("daemon", "status")):
            p = self.ds(*args, "--json")
            self.assertEqual(p.returncode, 0, "%s rc=%d" % (args, p.returncode))
            obj = json.loads(p.stdout)
            self.assertEqual(obj["running"], True, args)
        self.assertTrue(str(obj["device"]).startswith("virtual:"), obj)
        # --compact: a single line that still parses as one object
        p = self.ds("get", "/controls/keys/4", "--json", "--compact")
        self.assertEqual(p.returncode, 0)
        self.assertEqual(len(p.stdout.strip().splitlines()), 1)
        json.loads(p.stdout)
        # stream stats on a target with no stream yet: E_PATH exit 2, one JSON
        # envelope on stderr (the daemon does not serve the frame socket yet, so a
        # successful stats read is not reachable black-box in this phase)
        p = self.ds("stream", "stats", "key:0", "--json")
        self.assertEqual(p.returncode, 2, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        self.err_envelope(p, "E_PATH")
        p = self.ds("stream", "stats", "key:0")
        self.assert_exit(p, 2)

    # ---- 140 --ndjson streaming ---------------------------------------------------

    def test_cli_140_ndjson_streaming_for_watch(self):
        watch = subprocess.Popen(
            [str(self.stack.bins / "ds"), "--socket", str(self.stack.sock),
             "--data-dir", str(self.stack.data),
             "watch", "/controls", "--ndjson", "--max-events", "1"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            env={**os.environ, "DS_DAEMON_BIN": str(self.stack.bins / "ds-daemon")},
        )
        try:
            time.sleep(1.0)
            self.assertIsNone(watch.poll(), "watch exited before any set")
            p = self.ds("set", "/controls/keys/5/label", '"ndjson-watch"')
            self.assertEqual(p.returncode, 0, p.stderr)
            try:
                out, err = watch.communicate(timeout=WATCH_TIMEOUT_S)
            except subprocess.TimeoutExpired:
                watch.kill()
                out, err = watch.communicate()
                self.fail("watch --max-events 1 did not exit after the set")
            self.assertEqual(watch.returncode, 0,
                             "watch rc=%d stderr=%s" % (watch.returncode, err))
            lines = [l for l in out.splitlines() if l.strip()]
            self.assertEqual(len(lines), 1, "--ndjson: one object per line, no wrapper: %r"
                             % out)
            ev = json.loads(lines[0])
            self.assertEqual(ev["op"], "event")
            self.assertEqual(ev["path"], "/controls/keys/5/label")
            self.assertEqual(ev["value"], "ndjson-watch")
            self.assertIsInstance(ev["rev"], int)
        finally:
            if watch.poll() is None:
                watch.kill()
                watch.communicate(timeout=KILL_TIMEOUT_S)
        # human watch: one line per event (`event <path> rev=N`)
        watch = subprocess.Popen(
            [str(self.stack.bins / "ds"), "--socket", str(self.stack.sock),
             "--data-dir", str(self.stack.data),
             "watch", "/controls", "--max-events", "1"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            env={**os.environ, "DS_DAEMON_BIN": str(self.stack.bins / "ds-daemon")},
        )
        try:
            time.sleep(1.0)
            p = self.ds("set", "/controls/keys/6/label", '"human-watch"')
            self.assertEqual(p.returncode, 0, p.stderr)
            try:
                out, err = watch.communicate(timeout=WATCH_TIMEOUT_S)
            except subprocess.TimeoutExpired:
                watch.kill()
                out, err = watch.communicate()
                self.fail("human watch did not exit after the set")
            self.assertEqual(watch.returncode, 0, err)
            self.assertRegex(out, r"event /controls/keys/6/label rev=\d+")
            self.assertIn("ok events=1", out)
        finally:
            if watch.poll() is None:
                watch.kill()
                watch.communicate(timeout=KILL_TIMEOUT_S)

    # ---- 150 no migrate verb -------------------------------------------------------

    def test_cli_150_no_migrate_verb_exits_1(self):
        # scope delta 2 / cli.md §1: there is deliberately no `migrate` verb
        for args in (("migrate",), ("migrate", "/tmp/old"), ("migrate", "--help")):
            p = self.ds(*args)
            self.assert_exit(p, 1, "no 'migrate' verb")

    # ---- 160 set {} clears, null is E_SCHEMA ---------------------------------

    def test_cli_160_set_empty_object_clears(self):
        p = self.ds("set", "--help")
        self.assertEqual(p.returncode, 0, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        self.assertIn("{} clears an object path", p.stdout)
        # a whole-object write replaces the key
        self.jout(self.ds("set", "/controls/keys/12", '{"label":"before"}', "--json"))
        self.assertEqual(self.get_json("/controls/keys/12")["value"]["label"], "before")
        # {} clears it. Exit 0 and the value is an empty object
        env = self.jout(self.ds("set", "/controls/keys/12", "{}", "--json"))
        self.assertEqual(env["ok"], True)
        self.assertEqual(self.get_json("/controls/keys/12")["value"], {})
        # null is not a valid screenControl. It returns E_SCHEMA and exit 1
        p = self.ds("set", "/controls/keys/12", "null", "--json")
        self.assertEqual(p.returncode, 1, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        self.err_envelope(p, "E_SCHEMA")

    # ---- 170 ds colors: one bulk panel push (DS-111) ---------------------------

    def _vdevctl_frame_sha(self, target):
        """sha256 of the vdev's current reassembled frame for `target`, or None when the
        vdev has no frame for it yet (`frames` errors with 'no frame' in that case; the
        vdev keeps one frame per target, so growth is observed as a sha change)."""
        p = subprocess.run(
            [str(self.bins / "ds-vdevctl"), "--socket", str(self.stack.vdev_sock),
             "frames", target],
            capture_output=True, text=True, timeout=OP_TIMEOUT_S,
        )
        if p.returncode != 0 and "no frame" not in p.stderr:
            self.fail("vdevctl rc=%d stderr=%s" % (p.returncode, p.stderr))
        lines = [l for l in p.stdout.splitlines() if l.strip()]
        return lines[0].split()[1] if lines else None

    def _wait_for(self, cond, timeout_s, what):
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            if cond():
                return
            time.sleep(0.05)
        self.fail("timed out after %ss waiting for %s" % (timeout_s, what))

    def test_cli_170_colors_verb_bulk_panel_push(self):
        hexes = ["#%02x%02x00" % (i, 255 - i) for i in range(15)]
        arg = json.dumps(hexes)
        panel_before = self._vdevctl_frame_sha("panel")
        keys_before = {n: self._vdevctl_frame_sha("key:%d" % n) for n in range(15)}

        # `ds colors '<json-array>'`: one rev, one panel frame, no key frames
        rev0 = self.rev()
        p = self.ds("colors", arg)
        self.assertEqual(p.returncode, 0, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        self.assertRegex(p.stdout.strip(), r"^ok rev=\d+$", "human output: %r" % p.stdout)
        self.assertEqual(self.rev(), rev0 + 1, "one colors call commits exactly one rev")
        # the state carries the colors visual on /controls/panel/image
        self.assertEqual(self.get_json("/controls/panel/image")["value"],
                         {"kind": "colors", "value": hexes})
        # the device saw exactly one new panel frame and no per-key 0x36 reports: the
        # panel frame's sha changed, every key frame's sha is untouched (or absent)
        self._wait_for(lambda: self._vdevctl_frame_sha("panel") not in (None, panel_before),
                       10, "a new panel frame on the vdev")
        for n in range(15):
            self.assertEqual(self._vdevctl_frame_sha("key:%d" % n), keys_before[n],
                             "frames key:%d must not change on a colors push" % n)
        # --json prints the usual one-object envelope
        out = self.jout(self.ds("colors", arg, "--json"))
        self.assertEqual(out["ok"], True)
        self.assertIn("rev", out)
        # the equivalent `ds set /controls/panel/image --json …` accepts the same value
        rev_before = self.rev()
        p = self.ds("set", "/controls/panel/image", "--json",
                    json.dumps({"kind": "colors", "value": hexes}))
        self.assertEqual(p.returncode, 0, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        self.assertEqual(self.rev(), rev_before + 1)
        self.assertEqual(self.get_json("/controls/panel/image")["value"],
                         {"kind": "colors", "value": hexes})

        # rejections: wrong arity / bad JSON / bad hex exit 1 with rev unchanged
        rev_before = self.rev()
        bad_args = [
            json.dumps(hexes[:14]),                       # 14 entries
            json.dumps(hexes + ["#000000"]),              # 16 entries
            json.dumps(["red"] * 15),                     # not #rrggbb
            json.dumps([1] * 15),                         # not strings
            "not json",
            json.dumps({"kind": "colors", "value": hexes}),
        ]
        for bad in bad_args:
            p = self.ds("colors", bad)
            self.assertEqual(p.returncode, 1, "bad=%s rc=%d" % (bad[:40], p.returncode))
            self.assertIn("[exit 1]", p.stderr, "stderr=%s" % p.stderr)
        self.assertEqual(self.rev(), rev_before, "rejected colors calls must not move rev")

        # kind:colors on a key is rejected by the schema before the transaction
        p = self.ds("set", "/controls/keys/0/image", "--json",
                    json.dumps({"kind": "colors", "value": hexes}))
        self.assertEqual(p.returncode, 1, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        self.err_envelope(p, "E_SCHEMA")
        self.assertEqual(self.rev(), rev_before, "E_SCHEMA set must not move rev")
        # … and on the dial
        p = self.ds("set", "/controls/dial/image", "--json",
                    json.dumps({"kind": "colors", "value": hexes}))
        self.assertEqual(p.returncode, 1)
        self.err_envelope(p, "E_SCHEMA")

        # `ds colors --help` resolves at the verb level (exit 0, Usage block)
        p = self.ds("colors", "--help")
        self.assertEqual(p.returncode, 0, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        self.assertIn("Usage:", p.stdout)


class CliEnvelopeMapping(unittest.TestCase):
    """Exit codes 7/8 (and the whole E_* table) are not reachable against a healthy
    vdev stack, so they are driven through a scripted fake control socket: the CLI's
    envelope→exit-code mapping + stderr shape IS the contract under test (cli.md §3/§5)."""

    bins = None

    @classmethod
    def setUpClass(cls):
        cls.bins = ensure_binaries()

    def _run_with_reply(self, reply, *args):
        fake = FakeControl(reply)
        try:
            return fake.ds(self.bins, *args), fake
        except Exception:
            fake.close()
            raise

    def test_cli_200_E_DEVICE_maps_to_exit_7(self):
        reply = {"code": "E_DEVICE", "message": "device write failed: vdev refused"}
        p, fake = self._run_with_reply(reply, "get", "/controls")
        try:
            self.assertEqual(p.returncode, 7, "rc=%d stderr=%s" % (p.returncode, p.stderr))
            self.assertIn("[exit 7]", p.stderr)
            self.assertTrue(p.stderr.startswith("ds: "), p.stderr)
            self.assertEqual(p.stdout, "")
            # --json: one error envelope on stderr with code E_DEVICE
            p = fake.ds(self.bins, "get", "/controls", "--json")
            self.assertEqual(p.returncode, 7)
            env = json.loads(p.stderr)
            self.assertEqual(env, {"ok": False,
                                   "error": {"code": "E_DEVICE",
                                             "message": "device write failed: vdev refused"}})
        finally:
            fake.close()

    def test_cli_210_E_VERSION_E_INTERNAL_map_to_exit_8(self):
        for reply, needle in (
            ({"code": "E_VERSION", "message": "protocol version 99 not supported"},
             "protocol version 99"),
            ({"code": "E_INTERNAL", "message": "handler panicked"}, "handler panicked"),
            ("MALFORMED", "bad response line"),
        ):
            p, fake = self._run_with_reply(reply, "get", "/controls")
            try:
                self.assertEqual(p.returncode, 8,
                                 "reply=%r rc=%d stderr=%s" % (reply, p.returncode, p.stderr))
                self.assertIn("[exit 8]", p.stderr)
                self.assertIn(needle, p.stderr)
                self.assertEqual(p.stdout, "")
            finally:
                fake.close()

    def test_cli_220_full_E_code_to_exit_table(self):
        # the complete §3 table, pinned black-box: every E_ code maps to its exit code
        # with the `[exit N]` suffix on stderr, and to the same code in the --json
        # envelope on stderr
        for code, exit_code in EXIT_TABLE:
            reply = {"code": code, "message": "contract probe %s" % code}
            p, fake = self._run_with_reply(reply, "get", "/controls")
            try:
                self.assertEqual(p.returncode, exit_code,
                                 "%s -> rc=%d (want %d) stderr=%s"
                                 % (code, p.returncode, exit_code, p.stderr))
                self.assertIn("[exit %d]" % exit_code, p.stderr, code)
                pj = fake.ds(self.bins, "get", "/controls", "--json")
                self.assertEqual(pj.returncode, exit_code, code)
                self.assertEqual(pj.stdout, "", code)
                env = json.loads(pj.stderr)
                self.assertEqual(env["ok"], False)
                self.assertEqual(env["error"]["code"], code)
            finally:
                fake.close()


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


def _alarm(signum, frame):
    raise TimeoutError("cli conformance suite exceeded the %ss watchdog" % SUITE_TIMEOUT_S)


def cli_suite():
    """The suite object registered by tests/conformance/run.py."""
    loader = unittest.defaultTestLoader
    suite = unittest.TestSuite()
    suite.addTests(loader.loadTestsFromTestCase(CliContract))
    suite.addTests(loader.loadTestsFromTestCase(CliEnvelopeMapping))
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
        suite = cli_suite()
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
    print("cli conformance: %d tests, %d passed, %d failed/errored, %d skipped"
          % (n, n - bad - len(result.skipped), bad, len(result.skipped)))
    if bad:
        print("RESULT: FAIL")
        return 1
    print("RESULT: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
