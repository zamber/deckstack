#!/usr/bin/env python3
"""deckstack black-box conformance harness (ticket DS-018).

Single entry point, plain python3 stdlib only (unittest; no pytest, no third-party
packages -- see tests/requirements.txt). It builds the phase-1 binaries when they are
missing, boots ds-vdev + ds-daemon on a fresh temp dir, and drives the system ONLY
through the real `ds` binary and the raw NDJSON control socket (black box), asserting
the contracts of docs/contracts/{cli,ipc,state}.md -- including the scope invariants
(empty state at start, no `ds migrate`, rev semantics, S1 telemetry no-rev).

It also registers the DS-029 safety suite (tests/conformance/safety/, imported below), so
this one command machine-checks the mh-protocol safety invariants too (forbidden HID
commands unrepresentable + refused with zero bytes on the wire, software-mode handshake
before any read on keys/dial, backlight-0 never sent, no raw escape hatch in the CLI/IPC).

It also registers the DS-062 CLI self-description + error-contract suite
(tests/conformance/cli/): `--help` at every level (recursive walk with a coverage
report), `ds schema <name>` vs schemas/, `ds describe`, exit codes 0-8 with the exact
stderr shapes, `--json`/`--ndjson` rules, and the no-`migrate` invariant. Use
`-k cli` to run only those cases.

It also registers the DS-098 agent-onboarding suite
(tests/conformance/agent-onboarding/): boots its own stack, drives the real `ds-mcp`
over stdio (initialize, tools/list, deck_status, deck_set, deck_get) and then the same
three calls through clients/ds-py — the scripted proof of the docs/agents.md route.
Use `-k onboarding` to run only those cases.

It also registers the DS-099 micro-app authoring suite (tests/conformance/microapps/):
boots its own stack and asserts the CONTENT of `ds <verb> --help` for every verb
(usage, arguments with defaults, exactly one runnable example, exit codes, the
`ds schema`/`ds describe` pointer, <= 25 lines), then runs examples/microapps/
end to end against vdev (gitdeck.py --once against a temp git repo with ahead/
behind/dirty transitions, focus_panel.py --once/--loop on app/focus/changed,
status_strip.py --once) and asserts the resulting key/dial state.
Use `-k microapps` to run only those cases.
It also registers the DS-053 plugin-abuse suite (tests/conformance/plugins/): fork
bomb, memory bomb, capability violation, fs traversal, oversized/bad net URL, invoke
timeout -> degraded -> restart, net glob edge matrix, crash -> watchdog disable, each
on a fresh ds-vdev + daemon stack with generated raw-stdio fixture plugins
(docs/20-security.md T3/T5/T6/T11). Use `-k plugins` to run only those cases.

It also registers the DS-113 /preview debug-surface suite (tests/conformance/preview/):
default-off, HTTP+WS auth/T7/read-only checks, the action-metadata aggregate with the
demo plugin, and the zero-write guarantee (rev + ds-vdevctl writes unchanged).
Use `-k preview` to run only those cases.

It also registers the DS-081 security conformance mapping
(tests/conformance/security/): one test per threat row T1-T12 of docs/20-security.md
§2, each executing or referencing the concrete case that proves the row (the rows
subprocess the suites above with `-k` selectors, plus a live T9 token-rotation glue
case and a doc↔runner 1:1 assertion). Use `-k security` to run only the mapping.
Row checks that re-invoke this runner set DS_SECURITY_CHILD=1, which makes the child
skip registering the security suite (no recursion).

Usage:
    python3 tests/conformance/run.py [--verbose] [-k SUBSTR ...]

    -k SUBSTR   only run tests whose id contains SUBSTR (e.g. `-k safety` runs only the
                DS-029 safety cases; may be given multiple times)

Exit code 0 = all green, nonzero = at least one failure (or the stack did not boot).
Every wait carries a timeout; the daemon and vdev are always killed (SIGTERM then
SIGKILL) and the temp dir is removed, so CI can never hang or leak processes.
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

ROOT = Path(__file__).resolve().parents[2]
VERBOSE = "--verbose" in sys.argv

BUILD_TIMEOUT_S = 900
OP_TIMEOUT_S = 20
BOOT_TIMEOUT_S = 30
WATCH_TIMEOUT_S = 20
KILL_TIMEOUT_S = 5
# the safety suite (DS-029) may compile extra cargo test targets on a cold cache;
# the DS-081 security mapping (registered below) additionally subprocesses the
# referenced suites (bindings/exec/limits/fuzz/streaming subsets), so the watchdog
# carries headroom for those on top of the in-process suites
SUITE_TIMEOUT_S = 2400

BINARIES = ("ds", "ds-daemon", "ds-vdev", "ds-vdevctl")


def log(msg):
    if VERBOSE:
        print(msg, file=sys.stderr)


def bin_dir():
    env = os.environ.get("DS_BIN_DIR")
    if env:
        return Path(env)
    return ROOT / "target" / "debug"


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
        self.tmp = Path(tempfile.mkdtemp(prefix="ds-conformance-"))
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

    def ipc(self):
        return Ipc(self.sock)


    def stop(self):
        """Mandatory cleanup: stop the daemon, SIGTERM then SIGKILL both processes,
        remove the temp dir. Returns a list of problems (empty = clean)."""
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
    """Raw NDJSON control-socket client (black box: no ds-client code involved)."""

    def __init__(self, sock_path, timeout=OP_TIMEOUT_S):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(timeout)
        self.sock.connect(str(sock_path))
        self.f = self.sock.makefile("rw", encoding="utf-8", newline="\n")

    def send(self, obj):
        self.f.write(json.dumps(obj) + "\n")
        self.f.flush()

    def recv(self):
        line = self.f.readline()
        if not line:
            raise AssertionError("connection closed by daemon while waiting for a reply")
        return json.loads(line)

    def rpc(self, obj):
        self.send(obj)
        return self.recv()

    def close(self):
        try:
            self.f.close()
        finally:
            self.sock.close()


class Conformance(unittest.TestCase):
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
            self.fail("phase-1 stack did not boot: %s" % self.boot_error)
        self.ds = self.stack.ds

    # ---- helpers -----------------------------------------------------------

    def jout(self, p):
        self.assertEqual(p.returncode, 0, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        return json.loads(p.stdout)

    def get_json(self, path):
        return self.jout(self.ds("get", path, "--json"))

    def rev(self):
        return self.get_json("/meta")["rev"]

    # ---- CLI grammar / usage / exit codes ---------------------------------

    def test_010_help_at_every_level_exits_0(self):
        for args in (["--help"], ["get", "--help"], ["set", "--help"], ["watch", "--help"],
                     ["schema", "--help"], ["describe", "--help"], ["daemon", "--help"],
                     ["daemon", "start", "--help"], ["daemon", "stop", "--help"],
                     ["daemon", "status", "--help"], ["status", "--help"]):
            p = self.ds(*args)
            self.assertEqual(p.returncode, 0,
                             "%s rc=%d stderr=%s" % (args, p.returncode, p.stderr))
            self.assertTrue(p.stdout.strip(), "%s printed no help text" % args)

    def test_020_no_migrate_verb_exits_1(self):
        # scope invariant: no `ds migrate` (docs/00-scope.md, cli.md section 1)
        for args in (["migrate"], ["migrate", "/tmp/old"], ["migrate", "--help"]):
            p = self.ds(*args)
            self.assertEqual(p.returncode, 1, "%s rc=%d" % (args, p.returncode))
            self.assertIn("no 'migrate' verb", p.stderr, "%s stderr=%s" % (args, p.stderr))

    def test_030_unknown_path_exits_2_with_E_PATH(self):
        # cli.md §3: unknown path -> E_PATH, exit 2 (message text varies by path
        # validation level; the stable contract is the exit code and the E_ code)
        for path in ("/nope", "/controls/keys/99", "/does/not/exist"):
            p = self.ds("get", path)
            self.assertEqual(p.returncode, 2,
                             "%s rc=%d stderr=%s" % (path, p.returncode, p.stderr))
            self.assertIn("[exit 2]", p.stderr, "%s stderr=%s" % (path, p.stderr))
            p = self.ds("get", path, "--json")
            self.assertEqual(p.returncode, 2, "%s --json rc=%d" % (path, p.returncode))
            env = json.loads(p.stderr)
            self.assertEqual(env["ok"], False)
            self.assertEqual(env["error"]["code"], "E_PATH", path)

    def test_040_daemon_down_exits_6_with_start_hint(self):
        missing = self.stack.tmp / "missing" / "daemon.sock"
        env = dict(os.environ)
        env["DS_DAEMON_BIN"] = str(self.stack.bins / "ds-daemon")
        p = subprocess.run(
            [str(self.stack.bins / "ds"), "--socket", str(missing), "get", "/controls"],
            capture_output=True, text=True, timeout=OP_TIMEOUT_S, env=env)
        self.assertEqual(p.returncode, 6, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        self.assertIn("daemon unreachable", p.stderr)
        self.assertIn("ds daemon start", p.stderr)

    def test_050_streaming_verb_rejects_json_with_exit_1(self):
        # cli.md section 2 / S9: --json on a streaming verb is a usage error, exit 1
        p = self.ds("watch", "/controls", "--json")
        self.assertEqual(p.returncode, 1, "rc=%d" % p.returncode)
        self.assertIn("--json is not valid for streaming verbs", p.stderr)


    # ---- empty-state invariant (scope) -------------------------------------

    def test_100_empty_state_on_fresh_start(self):
        # docs/00-scope.md invariant 1: fresh start => /controls, /bindings,
        # /plugins empty and rev 0
        controls = self.get_json("/controls")
        self.assertEqual(controls["value"], {})
        self.assertEqual(controls["rev"], 0)
        self.assertEqual(self.get_json("/bindings")["value"], [])
        self.assertEqual(self.get_json("/plugins")["value"], {})
        meta = self.get_json("/meta")
        self.assertEqual(meta["value"]["rev"], 0)
        self.assertEqual(meta["value"]["version"], 1)
        self.assertEqual(meta["value"]["protocol"], 1)
        # same via raw IPC
        c = self.stack.ipc()
        try:
            w = c.rpc({"id": 1, "op": "hello", "protocol": 1, "client": "conf/1"})
            self.assertEqual(w["ok"], True)
            self.assertEqual(w["result"]["rev"], 0, "welcome.rev must be 0 on fresh state")
            c.rpc({"id": 2, "op": "auth", "token": self.stack.token})
            for i, (path, want) in enumerate((("/controls", {}), ("/bindings", []),
                                              ("/plugins", {}))):
                r = c.rpc({"id": 3 + i, "op": "get", "path": path})
                self.assertEqual(r["ok"], True)
                self.assertEqual(r["result"]["value"], want, path)
                self.assertEqual(r["rev"], 0, path)
        finally:
            c.close()

    # ---- raw NDJSON handshake: hello / auth --------------------------------

    def test_110_hello_auth_over_raw_ndjson(self):
        # op before hello -> E_VERSION (handshake rule, ipc.md section 2)
        c = self.stack.ipc()
        try:
            r = c.rpc({"id": 7, "op": "ping"})
            self.assertEqual(r["ok"], False)
            self.assertEqual(r["error"]["code"], "E_VERSION")
            self.assertEqual(r["id"], 7)
        finally:
            c.close()
        # wrong protocol -> E_VERSION
        c = self.stack.ipc()
        try:
            r = c.rpc({"id": 1, "op": "hello", "protocol": 99, "client": "conf/1"})
            self.assertEqual(r["ok"], False)
            self.assertEqual(r["error"]["code"], "E_VERSION")
        finally:
            c.close()
        # good hello -> welcome; then the auth rules
        c = self.stack.ipc()
        try:
            r = c.rpc({"id": 1, "op": "hello", "protocol": 1, "client": "conf/1"})
            self.assertEqual(r["ok"], True)
            self.assertEqual(r["id"], 1)
            res = r["result"]
            self.assertEqual(res["op"], "welcome")
            self.assertEqual(res["protocol"], 1)
            self.assertEqual(res["rev"], 0)
            self.assertTrue(str(res["server"]).startswith("ds-daemon/"), res)
            # op before auth -> E_AUTH
            r = c.rpc({"id": 2, "op": "ping"})
            self.assertEqual(r["ok"], False)
            self.assertEqual(r["error"]["code"], "E_AUTH")
            # bad token -> E_AUTH
            r = c.rpc({"id": 3, "op": "auth", "token": "0" * 64, "scope": "admin"})
            self.assertEqual(r["ok"], False)
            self.assertEqual(r["error"]["code"], "E_AUTH")
            # token from <dir>/token -> admin scope, then ops work
            r = c.rpc({"id": 4, "op": "auth", "token": self.stack.token, "scope": "admin"})
            self.assertEqual(r["ok"], True)
            self.assertEqual(r["result"]["scope"], "admin")
            r = c.rpc({"id": 5, "op": "ping"})
            self.assertEqual(r["ok"], True)
            self.assertEqual(r["result"]["pong"], True)
        finally:
            c.close()

    def test_120_ipc_request_response_shapes(self):
        # id echo, ok flag, stable E_* error envelope (ipc-message.json)
        c = self.stack.ipc()
        try:
            c.rpc({"id": 0, "op": "hello", "protocol": 1, "client": "conf/1"})
            c.rpc({"id": 1, "op": "auth", "token": self.stack.token})
            r = c.rpc({"id": 42, "op": "ping"})
            self.assertEqual(r["id"], 42)
            self.assertEqual(r["ok"], True)
            self.assertEqual(r["result"], {"pong": True})
            r = c.rpc({"id": 43, "op": "get", "path": "/no/such/path"})
            self.assertEqual(r["id"], 43)
            self.assertEqual(r["ok"], False)
            self.assertEqual(r["error"]["code"], "E_PATH")
            self.assertIsInstance(r["error"]["message"], str)
            # a request missing its path gets an error envelope; connection stays open
            r = c.rpc({"id": 44, "op": "get"})
            self.assertEqual(r["ok"], False)
            self.assertIn(r["error"]["code"], ("E_SCHEMA", "E_PATH"))
            r = c.rpc({"id": 45, "op": "ping"})
            self.assertEqual(r["ok"], True)
        finally:
            c.close()


    # ---- CLI get/set round-trip --------------------------------------------

    def test_200_set_get_roundtrip_color(self):
        color = {"kind": "color", "value": "#ff0000"}
        # human mode: `ok rev=N` on stdout (cli.md §2: one line per fact)
        p = self.ds("set", "/controls/keys/0/image", json.dumps(color))
        self.assertEqual(p.returncode, 0, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        m = re.match(r"^ok rev=(\d+)$", p.stdout.strip())
        self.assertIsNotNone(m, "human set output must be 'ok rev=N', got %r" % p.stdout)
        rev = int(m.group(1))
        self.assertGreaterEqual(rev, 1)
        # --json mode: exactly one JSON object {"ok":true,"rev":N}
        p = self.ds("set", "/controls/keys/1/image", "--json", json.dumps(color))
        self.assertEqual(p.returncode, 0, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        env = json.loads(p.stdout)
        self.assertEqual(env["ok"], True)
        self.assertIsInstance(env["rev"], int)
        rev = env["rev"]
        got = self.get_json("/controls/keys/0/image")
        self.assertEqual(got["path"], "/controls/keys/0/image")
        self.assertEqual(got["value"], color)
        self.assertEqual(got["rev"], rev)
        p = self.ds("get", "/controls/keys/0/image")
        self.assertEqual(p.returncode, 0)
        self.assertIn("#ff0000", p.stdout)
        # raw IPC sees the same value
        c = self.stack.ipc()
        try:
            c.rpc({"id": 0, "op": "hello", "protocol": 1, "client": "conf/1"})
            c.rpc({"id": 1, "op": "auth", "token": self.stack.token})
            r = c.rpc({"id": 2, "op": "get", "path": "/controls/keys/0/image"})
            self.assertEqual(r["ok"], True)
            self.assertEqual(r["result"]["value"], color)
            self.assertEqual(r["rev"], rev)
        finally:
            c.close()

    def test_210_get_json_shape(self):
        got = self.get_json("/controls/keys/0")
        self.assertEqual(set(got.keys()), {"path", "value", "rev"}, got)
        self.assertEqual(got["path"], "/controls/keys/0")
        self.assertIsInstance(got["rev"], int)
        # /telemetry reads carry no rev (S1, state.md section 6)
        tel = self.get_json("/telemetry")
        self.assertEqual(set(tel.keys()), {"path", "value"}, tel)
        self.assertNotIn("rev", tel)

    def test_220_rev_semantics(self):
        r0 = self.rev()
        p = self.ds("set", "/controls/keys/1/label", '"Hello"')
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(self.rev(), r0 + 1, "a set must bump rev by exactly 1")
        before = self.rev()
        self.get_json("/controls")
        self.get_json("/controls/keys/0")
        self.get_json("/telemetry")
        self.get_json("/meta")
        self.assertEqual(self.rev(), before, "reads must not bump rev")
        p = self.ds("get", "/controls", "--rev", str(before + 1000))
        self.assertEqual(p.returncode, 3, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        p = self.ds("get", "/controls", "--rev", str(before))
        self.assertEqual(p.returncode, 0, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        # raw IPC: telemetry get response has no rev key at all
        c = self.stack.ipc()
        try:
            c.rpc({"id": 0, "op": "hello", "protocol": 1, "client": "conf/1"})
            c.rpc({"id": 1, "op": "auth", "token": self.stack.token})
            r = c.rpc({"id": 2, "op": "get", "path": "/telemetry"})
            self.assertEqual(r["ok"], True)
            self.assertNotIn("rev", r)
            r = c.rpc({"id": 3, "op": "get", "path": "/controls"})
            self.assertIn("rev", r)
        finally:
            c.close()

    def test_230_set_if_rev_conflict_exits_3(self):
        cur = self.rev()
        p = self.ds("set", "/controls/keys/2/label", '"x"',
                    "--if-rev", str(cur + 500), "--json")
        self.assertEqual(p.returncode, 3, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        env = json.loads(p.stderr)
        self.assertEqual(env["ok"], False)
        self.assertEqual(env["error"]["code"], "E_CONFLICT")
        self.assertEqual(env["error"]["current_rev"], cur)
        p = self.ds("set", "/controls/keys/2/label", '"x"', "--if-rev", str(cur))
        self.assertEqual(p.returncode, 0, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        self.assertEqual(self.rev(), cur + 1)

    def test_240_set_schema_violation_exits_1(self):
        p = self.ds("set", "/controls/keys/0/image", "--json",
                    '{"kind":"color","value":"red"}')
        self.assertEqual(p.returncode, 1, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        env = json.loads(p.stderr)
        self.assertEqual(env["ok"], False)
        self.assertEqual(env["error"]["code"], "E_SCHEMA")


    # ---- watch (streaming verb) --------------------------------------------

    def test_300_watch_ndjson_receives_event_after_set(self):
        watch = subprocess.Popen(
            [str(self.stack.bins / "ds"), "--socket", str(self.stack.sock),
             "--data-dir", str(self.stack.data),
             "watch", "/controls", "--ndjson", "--max-events", "1"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            env={**os.environ, "DS_DAEMON_BIN": str(self.stack.bins / "ds-daemon")},
        )
        try:
            # let the subscription reach the daemon (bounded settle wait)
            time.sleep(1.0)
            self.assertIsNone(watch.poll(), "watch exited before any set")
            p = self.ds("set", "/controls/keys/3/label", '"watched"')
            self.assertEqual(p.returncode, 0, p.stderr)
            try:
                out, err = watch.communicate(timeout=WATCH_TIMEOUT_S)
            except subprocess.TimeoutExpired:
                watch.kill()
                out, err = watch.communicate()
                self.fail("watch --max-events 1 did not exit after the set (timeout)")
            self.assertEqual(watch.returncode, 0,
                             "watch rc=%d stderr=%s" % (watch.returncode, err))
            lines = [l for l in out.splitlines() if l.strip()]
            self.assertEqual(len(lines), 1,
                             "expected exactly 1 NDJSON event line, got %r" % out)
            ev = json.loads(lines[0])
            self.assertEqual(ev["op"], "event")
            self.assertEqual(ev["path"], "/controls/keys/3/label")
            self.assertEqual(ev["value"], "watched")
            self.assertIsInstance(ev["rev"], int)
            self.assertGreaterEqual(ev["rev"], 1)
        finally:
            if watch.poll() is None:
                watch.kill()
                watch.communicate(timeout=KILL_TIMEOUT_S)

    # ---- schema / describe (self-description contract) ----------------------

    def test_400_schema_state_prints_the_schema(self):
        p = self.ds("schema", "state")
        self.assertEqual(p.returncode, 0, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        schema = json.loads(p.stdout)
        self.assertEqual(schema["$id"], "https://deckstack.dev/schemas/state.json/v1")
        self.assertIn("$defs", schema)
        self.assertIn("visual", schema["$defs"])
        p = self.ds("schema", "ipc")
        self.assertEqual(p.returncode, 0)
        self.assertEqual(json.loads(p.stdout)["$id"],
                         "https://deckstack.dev/schemas/ipc-message.json/v1")
        p = self.ds("schema", "nope")
        self.assertEqual(p.returncode, 1)

    def test_410_describe_shows_schema_value_verbs(self):
        p = self.ds("describe", "/controls/keys/0", "--json")
        self.assertEqual(p.returncode, 0, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        d = json.loads(p.stdout)
        self.assertEqual(d["path"], "/controls/keys/0")
        self.assertIn("value", d)
        self.assertIn("rev", d)
        self.assertEqual(set(d["verbs"]), {"get", "set", "watch", "describe"})
        self.assertIsInstance(d["schema"], dict)
        d = self.jout(self.ds("describe", "/controls/keys/0/image", "--json"))
        self.assertIn("#/$defs/visual", d["schema"]["$ref"])
        p = self.ds("describe", "/controls/keys/0")
        self.assertEqual(p.returncode, 0)
        for needle in ("path:", "value:", "verbs:", "schema:"):
            self.assertIn(needle, p.stdout)

    # ---- daemon ops ---------------------------------------------------------

    def test_500_daemon_status_reports_running(self):
        p = self.ds("daemon", "status")
        self.assertEqual(p.returncode, 0, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        self.assertIn("ok running", p.stdout)
        self.assertIn("rev=", p.stdout)
        d = self.jout(self.ds("daemon", "status", "--json"))
        self.assertEqual(d["running"], True)
        self.assertIsInstance(d["rev"], int)
        self.assertTrue(str(d["device"]).startswith("virtual:"), d)
        p = self.ds("status")
        self.assertEqual(p.returncode, 0)
        self.assertIn("client=ds/", p.stdout)


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
    raise TimeoutError("conformance suite exceeded the %ss watchdog" % SUITE_TIMEOUT_S)


def _safety_suite():
    """The DS-029 safety suite (tests/conformance/safety/), registered into this run so
    `python tests/conformance/run.py` machine-checks the safety invariants too."""
    conf_dir = str(ROOT / "tests" / "conformance")
    if conf_dir not in sys.path:
        sys.path.insert(0, conf_dir)
    from safety.run import safety_suite
    return safety_suite()


def _cli_suite():
    """The DS-062 CLI self-description + error-contract suite (tests/conformance/cli/),
    registered into this run so `python tests/conformance/run.py` machine-checks the
    cli.md contract (help at every level, ds schema/describe, exit codes 0-8,
    --json/--ndjson rules, no migrate verb) too. `-k cli` runs only these cases."""
    conf_dir = str(ROOT / "tests" / "conformance")
    if conf_dir not in sys.path:
        sys.path.insert(0, conf_dir)
    from cli.run import cli_suite
    return cli_suite()


def _microapps_suite():
    """The DS-099 micro-app authoring-route suite (tests/conformance/microapps/),
    registered into this run so `python tests/conformance/run.py` also machine-checks
    the help CONTENT of every verb (DS-099 asserts content; DS-062 asserted presence)
    and the runnable examples in examples/microapps/ end to end against vdev.
    `-k microapps` runs only these cases."""
    conf_dir = str(ROOT / "tests" / "conformance")
    if conf_dir not in sys.path:
        sys.path.insert(0, conf_dir)
    from microapps.run import microapps_suite
    return microapps_suite()


def _agent_onboarding_suite():
    """The DS-098 agent-onboarding suite (tests/conformance/agent-onboarding/),
    registered into this run so `python tests/conformance/run.py` also machine-checks
    the docs/agents.md route: ds-mcp over stdio (initialize, tools/list, deck_status,
    deck_set, deck_get on a booted vdev+daemon) and the same three calls through
    clients/ds-py. `-k onboarding` runs only these cases. The directory name has a
    hyphen, so the module is loaded by path with importlib."""
    import importlib.util
    path = ROOT / "tests" / "conformance" / "agent-onboarding" / "run.py"
    spec = importlib.util.spec_from_file_location("agent_onboarding", path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod.agent_onboarding_suite()
def _plugins_suite():
    """The DS-053 plugin-abuse suite (tests/conformance/plugins/), registered into this
    run so `python tests/conformance/run.py` also machine-checks the T3/T5/T6/T11
    plugin-abuse invariants (fork bomb, memory bomb, capability violation, fs
    traversal, oversized/bad net URL, invoke timeout -> degraded -> restart, net glob
    edges, crash -> watchdog disable) on ds-vdev + the daemon. `-k plugins` runs only
    these cases."""
    conf_dir = str(ROOT / "tests" / "conformance")
    if conf_dir not in sys.path:
        sys.path.insert(0, conf_dir)
    from plugins.run import plugins_suite
    return plugins_suite()


def _preview_suite():
    """The DS-113 /preview + action-metadata suite (tests/conformance/preview/),
    registered into this run so `python tests/conformance/run.py` also machine-checks
    the read-only localhost debug surface on ds-vdev + the daemon (HTML + bearer +
    T7 Host refusal, action metadata incl. the demo plugin, WS live state, no writes
    accepted, /policy disable). `-k preview` runs only these cases."""
    conf_dir = str(ROOT / "tests" / "conformance")
    if conf_dir not in sys.path:
        sys.path.insert(0, conf_dir)
    from preview.run import preview_suite
    return preview_suite()


def _docs_markers_suite():
    """The DS-116..119 doc-marker suite (tests/conformance/docs_markers/): one shared
    home for the doc-wording assertions of the doc tickets (DS-116 asserts the OPEN
    O6/O11 markers are gone from docs/ and the rows are RESOLVED; DS-117 adds the
    cli.md later-phase "recognized but refused" markers; DS-118/119 add theirs here).
    `-k docs_markers` runs only these cases."""
    conf_dir = str(ROOT / "tests" / "conformance")
    if conf_dir not in sys.path:
        sys.path.insert(0, conf_dir)
    from docs_markers.run import docs_markers_suite
    return docs_markers_suite()


def _security_suite():
    """The DS-081 security conformance mapping (tests/conformance/security/): one test
    per threat row T1-T12 of docs/20-security.md §2, each executing or referencing the
    concrete case that proves the row (the row checks subprocess the suites registered
    here with `-k` selectors — they set DS_SECURITY_CHILD=1 so the child does not
    register this suite again — plus a live T9 rotation glue case and the doc↔runner
    1:1 assertion). `-k security` runs only these cases."""
    conf_dir = str(ROOT / "tests" / "conformance")
    if conf_dir not in sys.path:
        sys.path.insert(0, conf_dir)
    from security.run import security_suite
    return security_suite()


def _filter_suite(suite, patterns):
    """Keep only the tests whose id contains one of the `-k` substrings."""
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


def main():
    patterns = _parse_k(sys.argv)
    signal.signal(signal.SIGALRM, _alarm)
    signal.alarm(SUITE_TIMEOUT_S)
    try:
        suite = unittest.TestSuite()
        suite.addTests(unittest.defaultTestLoader.loadTestsFromTestCase(Conformance))
        suite.addTests(_safety_suite())
        suite.addTests(_cli_suite())
        suite.addTests(_agent_onboarding_suite())
        suite.addTests(_microapps_suite())
        suite.addTests(_plugins_suite())
        suite.addTests(_preview_suite())
        # DS-116..119 doc-marker assertions (shared location: tests/conformance/docs_markers/)
        suite.addTests(_docs_markers_suite())
        # DS-081 security mapping (T1-T12). Skipped in children spawned by the mapping's
        # own row checks (DS_SECURITY_CHILD=1) so they never recurse into it.
        if not os.environ.get("DS_SECURITY_CHILD"):
            suite.addTests(_security_suite())
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
    print("conformance: %d tests, %d passed, %d failed/errored, %d skipped"
          % (n, n - bad - len(result.skipped), bad, len(result.skipped)))
    if bad:
        print("RESULT: FAIL")
        return 1
    print("RESULT: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
