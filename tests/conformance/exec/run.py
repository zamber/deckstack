#!/usr/bin/env python3
"""deckstack exec-policy conformance suite (ticket DS-043, tests/conformance/exec/).

Black-box tests over the REAL binaries only: boots ds-vdev + ds-daemon on a fresh
temp dir and drives the system through the `ds` binary, the raw NDJSON control socket
and `ds-vdevctl inject` — no ds-client/ds-daemon code involved. It proves the T5
exec-policy claims of docs/20-security.md §2 and docs/contracts/actions.md §4 on the
wire:

    010 an allowlisted absolute binary runs fire-and-forget (/usr/bin/touch creates
        its file), audited as deck/system/exec {decision:"allow", argv, cwd, pid, rev},
        and no binding-error is emitted
    020 a denied shell (["/bin/sh","-c","touch <tmp>/pwn"]) is refused: binding-error
        E_CAP + deck/system/exec {decision:"refused", code:"E_CAP"}, no pid, the file
        is never created and no exec-child process exists
    030 /usr/bin/env (wrapper) is refused the same way -> E_CAP
    040 a relative argv[0] is rejected at bind time with E_SCHEMA (exit 1)
    050 a client-origin (app/*) trigger is non-admin: E_SCOPE + refused audit record
    060 a binding without allow_exec:true -> E_CAP
    070 cwd outside /policy/fs/roots + /tmp -> refused (E_CAP)
    080 a metacharacter argument arrives literally: /usr/bin/touch "<tmp>/meta$(touch
        pwned)`id`;x" creates exactly that file; the shell-interpolated "pwned" file
        is never created (no shell ever sees the argv)
    090 rlimit kill: an allowlisted CPU hog (/usr/bin/yes) is SIGKILLed at the CPU
        limit (RLIMIT_CPU 10 s) within ~15 s wall clock
    100 a refused exec step emits deck/system/binding-error and the remaining steps
        of the binding still run

Usage:
    python3 tests/conformance/exec/run.py [--verbose]

Exit code 0 = all green. Every wait carries a timeout; the daemon and vdev are always
killed (SIGTERM then SIGKILL) and the temp dir removed, so the run can never hang or
leak processes. The main suite (tests/conformance/run.py) is unaffected by this
directory (it registers only safety/ and cli/); run this suite directly, like
tests/conformance/bindings/run.py.
"""

import json
import os
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
EVENT_TIMEOUT_S = 10
KILL_TIMEOUT_S = 5
SUITE_TIMEOUT_S = 300
RLIMIT_KILL_TIMEOUT_S = 20

BINARIES = ("ds", "ds-daemon", "ds-vdev", "ds-vdevctl")
EXEC_TOPIC = "deck/system/exec"
BINDING_ERROR_TOPIC = "deck/system/binding-error"
EXEC_CHILD_MARKER = "__ds-exec-child"


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
        raise RuntimeError("cargo not found and binaries missing from %s" % d)
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
        self.tmp = Path(tempfile.mkdtemp(prefix="ds-exec-"))
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

    def vdevctl(self, *args, timeout=OP_TIMEOUT_S):
        env = dict(os.environ)
        env["DS_DAEMON_BIN"] = str(self.bins / "ds-daemon")
        return subprocess.run(
            [str(self.bins / "ds-vdevctl"), "--socket", str(self.vdev_sock)] + list(args),
            capture_output=True, text=True, timeout=timeout, env=env,
        )

    def inject(self, events, timeout=OP_TIMEOUT_S):
        f = self.tmp / ("inject-%d.ndjson" % time.monotonic_ns())
        f.write_text("".join(json.dumps(e) + "\n" for e in events))
        return self.vdevctl("inject", str(f), timeout=timeout)

    def ipc(self, timeout=OP_TIMEOUT_S):
        return Ipc(self.sock, timeout)

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
    """Raw NDJSON control-socket client (black box). Select-based reads so a timeout
    never poisons the connection."""

    def __init__(self, sock_path, timeout=OP_TIMEOUT_S):
        import select
        self._select = select
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(timeout)
        self.sock.connect(str(sock_path))
        self.timeout = timeout
        self.buf = bytearray()

    def send(self, obj):
        self.sock.sendall((json.dumps(obj) + "\n").encode("utf-8"))

    def recv(self, timeout=None):
        timeout = self.timeout if timeout is None else timeout
        deadline = time.monotonic() + timeout
        while True:
            nl = self.buf.find(b"\n")
            if nl >= 0:
                line = bytes(self.buf[:nl])
                del self.buf[: nl + 1]
                if not line.strip():
                    continue
                return json.loads(line)
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise socket.timeout("recv timed out after %ss" % timeout)
            r, _, _ = self._select.select([self.sock], [], [], remaining)
            if not r:
                raise socket.timeout("recv timed out after %ss" % timeout)
            chunk = self.sock.recv(65536)
            if not chunk:
                raise AssertionError("connection closed by daemon while waiting for a reply")
            self.buf.extend(chunk)

    def rpc(self, obj, timeout=None):
        self.send(obj)
        return self.recv(timeout)

    def hello_auth(self, token):
        r = self.rpc({"id": 0, "op": "hello", "protocol": 1, "client": "exec/1"})
        assert r["ok"], r
        r = self.rpc({"id": 1, "op": "auth", "token": token, "scope": "admin"})
        assert r["ok"], r
        return self

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


def exec_child_processes():
    """Pids whose cmdline contains the private exec-child marker (zero expected when
    every exec was refused)."""
    out = []
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            cmd = (Path("/proc") / entry / "cmdline").read_bytes()
        except OSError:
            continue
        if EXEC_CHILD_MARKER.encode() in cmd:
            out.append(int(entry))
    return out


def pid_alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


class Exec(unittest.TestCase):
    """The DS-043 exec-policy conformance cases (T5)."""

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
            self.fail("exec stack did not boot: %s" % self.boot_error)
        self.stack = Exec.stack
        self.token = self.stack.token
        self.tmp = self.stack.tmp
        if not hasattr(self, "_policy_done"):
            # one-time policy setup: fs roots = the temp dir, exec allowlist = the
            # test binaries. /policy is admin-writable (install token = admin).
            p = self.stack.ds("set", "/policy/fs/roots", "--json",
                              json.dumps([str(self.tmp)]))
            self.assertEqual(p.returncode, 0, p.stderr)
            p = self.stack.ds("set", "/policy/exec/allow", "--json",
                              json.dumps(["/bin/true", "/usr/bin/touch", "/usr/bin/yes"]))
            self.assertEqual(p.returncode, 0, p.stderr)
            Exec._policy_done = True

    # ---- helpers -----------------------------------------------------------

    def jout(self, p):
        self.assertEqual(p.returncode, 0, "rc=%d stderr=%s" % (p.returncode, p.stderr))
        return json.loads(p.stdout)

    def get_json(self, path):
        return self.jout(self.stack.ds("get", path, "--json"))

    def rev(self):
        return self.get_json("/meta")["rev"]

    def conn(self):
        return Ipc(self.stack.sock).hello_auth(self.token)

    def bind_raw(self, binding):
        """Raw IPC bind (the ds CLI has no --allow-exec flag): returns the reply."""
        c = self.conn()
        try:
            return c.rpc({"id": 2, "op": "bind", "binding": binding})
        finally:
            c.close()

    def bind_exec(self, topic, argv, key_topic=True, allow_exec=True, cwd=None,
                  extra_steps=()):
        """Bind `topic` to an exec step (plus optional extra steps); returns the id."""
        step = {"do": "exec", "argv": argv}
        if cwd is not None:
            step["cwd"] = cwd
        binding = {
            "id": "b0000",
            "topic": topic,
            "allow_exec": allow_exec,
            "steps": [step] + list(extra_steps),
        }
        r = self.bind_raw(binding)
        self.assertEqual(r.get("ok"), True, r)
        return r["result"]["id"]

    def next_event(self, c, want_topic=None, timeout=EVENT_TIMEOUT_S):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            line = c.recv(timeout=max(0.1, deadline - time.monotonic()))
            if line.get("op") == "event":
                if want_topic is not None and line.get("topic") != want_topic:
                    continue
                return line
        raise AssertionError("no matching event within %ss (want topic=%r)"
                             % (timeout, want_topic))

    def collect_system_events(self, c, timeout=EVENT_TIMEOUT_S):
        """Drain /deck/system/* events for a short while; returns (exec, errors)."""
        execs, errors = [], []
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                line = c.recv(timeout=max(0.05, deadline - time.monotonic()))
            except socket.timeout:
                break
            if line.get("op") != "event":
                continue
            if line.get("topic") == EXEC_TOPIC:
                execs.append(line["payload"])
            elif line.get("topic") == BINDING_ERROR_TOPIC:
                errors.append(line["payload"])
        return execs, errors

    def sub_system(self):
        c = self.conn()
        r = c.rpc({"id": 2, "op": "subscribe", "path": "/deck/system/*"})
        self.assertEqual(r.get("ok"), True, r)
        time.sleep(0.3)  # let the subscription register before the trigger
        return c


    def wait_system(self, c, want_exec=True, want_err=True, timeout=EVENT_TIMEOUT_S):
        """Wait until both the exec audit event and the binding-error event (in any
        order) have arrived; returns (execs, errors)."""
        execs, errors = [], []
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if want_exec and not execs and not (want_err and errors):
                pass
            if (not want_exec or execs) and (not want_err or errors):
                break
            try:
                line = c.recv(timeout=max(0.05, deadline - time.monotonic()))
            except socket.timeout:
                break
            if line.get("op") != "event":
                continue
            if line.get("topic") == EXEC_TOPIC:
                execs.append(line["payload"])
            elif line.get("topic") == BINDING_ERROR_TOPIC:
                errors.append(line["payload"])
        return execs, errors

    # ---- cases -------------------------------------------------------------

    def test_010_allowlisted_absolute_binary_runs_and_is_audited(self):
        target = self.tmp / "exec-ok"
        bid = self.bind_exec("deck/control/keys.5/down",
                             ["/usr/bin/touch", str(target)])
        sub = self.sub_system()
        try:
            inj = self.stack.inject([{"module": "keys", "keys": [5]}])
            self.assertEqual(inj.returncode, 0, inj.stderr)
            ev = self.next_event(sub, want_topic=EXEC_TOPIC)
            p = ev["payload"]
            self.assertEqual(p["decision"], "allow", p)
            self.assertEqual(p["argv"], ["/usr/bin/touch", str(target)])
            self.assertEqual(p["binding"], bid)
            self.assertGreater(p["pid"], 0, p)
            self.assertIsInstance(p["rev"], int)
            self.assertTrue(str(p["cwd"]).startswith("/tmp") or str(p["cwd"]) == str(self.tmp), p)
            # no binding-error: the step was allowed
            execs, errors = self.collect_system_events(sub, timeout=2)
            self.assertEqual(errors, [], "an allowed exec must not raise binding-error")
            self.assertEqual(len(execs), 0, "exactly one audit event: %r" % execs)
            # the binary really ran (fire-and-forget): the file appears
            deadline = time.monotonic() + EVENT_TIMEOUT_S
            while not target.exists() and time.monotonic() < deadline:
                time.sleep(0.05)
            self.assertTrue(target.exists(), "allowlisted /usr/bin/touch must create the file")
            # the child exited cleanly (reaped, not a zombie)
            deadline = time.monotonic() + 5
            while pid_alive(p["pid"]) and time.monotonic() < deadline:
                time.sleep(0.05)
            self.assertFalse(pid_alive(p["pid"]), "the exec child must be reaped, not a zombie")
        finally:
            sub.close()

    def test_020_denied_shell_is_refused_with_zero_processes(self):
        pwn = self.tmp / "pwn"
        bid = self.bind_exec("deck/control/keys.6/down",
                             ["/bin/sh", "-c", "touch %s" % pwn])
        sub = self.sub_system()
        try:
            inj = self.stack.inject([{"module": "keys", "keys": [6]}])
            self.assertEqual(inj.returncode, 0, inj.stderr)
            execs, errors = self.wait_system(sub)
            self.assertEqual(len(errors), 1, "errors=%r execs=%r" % (errors, execs))
            ep = errors[0]
            self.assertEqual(ep["error"]["code"], "E_CAP", ep)
            self.assertEqual(ep["binding"], bid)
            self.assertEqual(ep["step"], 0)
            self.assertEqual(len(execs), 1, "execs=%r" % execs)
            p = execs[0]
            self.assertEqual(p["decision"], "refused", p)
            self.assertEqual(p["code"], "E_CAP", p)
            self.assertEqual(p["argv"], ["/bin/sh", "-c", "touch %s" % pwn])
            self.assertNotIn("pid", p, "a refused exec must not carry a pid")
            self.assertIn("deny list", p["reason"])
            time.sleep(0.5)
            self.assertFalse(pwn.exists(), "the shell command must never run")
            self.assertEqual(exec_child_processes(), [],
                             "no exec-child process may exist after a refusal")
        finally:
            sub.close()

    def test_030_env_wrapper_is_refused(self):
        pwn2 = self.tmp / "pwn2"
        self.bind_exec("deck/control/keys.7/down",
                       ["/usr/bin/env", "touch", str(pwn2)])
        sub = self.sub_system()
        try:
            inj = self.stack.inject([{"module": "keys", "keys": [7]}])
            self.assertEqual(inj.returncode, 0, inj.stderr)
            execs, errors = self.wait_system(sub)
            self.assertEqual(len(execs), 1, "execs=%r" % execs)
            p = execs[0]
            self.assertEqual(p["decision"], "refused")
            self.assertEqual(p["code"], "E_CAP")
            self.assertEqual(p["argv"][0], "/usr/bin/env")
            self.assertEqual(len(errors), 1)
            self.assertEqual(errors[0]["error"]["code"], "E_CAP")
            time.sleep(0.3)
            self.assertFalse(pwn2.exists())
            self.assertEqual(exec_child_processes(), [])
        finally:
            sub.close()

    def test_040_relative_argv0_is_rejected_at_bind_time_with_e_schema(self):
        r = self.bind_raw({
            "id": "b0000",
            "topic": "deck/control/keys.8/down",
            "allow_exec": True,
            "steps": [{"do": "exec", "argv": ["sh", "-c", "x"]}],
        })
        self.assertEqual(r.get("ok"), False, r)
        self.assertEqual(r["error"]["code"], "E_SCHEMA", r)
        # and via the CLI: exit 1
        p = self.stack.ds("bind", "deck/control/keys.8/down", "--json",
                          "--do", '{"do":"exec","argv":["sh","-c","x"]}')
        self.assertEqual(p.returncode, 1, "rc=%d stderr=%s" % (p.returncode, p.stderr))

    def test_050_client_origin_trigger_is_non_admin_e_scope(self):
        target = self.tmp / "scope-target"
        bid = self.bind_exec("app/demo/execscope", ["/usr/bin/touch", str(target)])
        sub = self.sub_system()
        ctl = self.conn()
        try:
            r = ctl.rpc({"id": 3, "op": "emit", "topic": "app/demo/execscope", "payload": {}})
            self.assertEqual(r.get("ok"), True, r)
            execs, errors = self.wait_system(sub)
            self.assertEqual(len(errors), 1, "errors=%r" % errors)
            self.assertEqual(errors[0]["error"]["code"], "E_SCOPE", errors[0])
            self.assertEqual(errors[0]["binding"], bid)
            self.assertEqual(len(execs), 1, "execs=%r" % execs)
            p = execs[0]
            self.assertEqual(p["decision"], "refused")
            self.assertEqual(p["code"], "E_SCOPE")
            self.assertEqual(p["trigger"], "app/demo/execscope")
            time.sleep(0.3)
            self.assertFalse(target.exists(), "a non-admin trigger must never spawn")
            self.assertEqual(exec_child_processes(), [])
        finally:
            sub.close()
            ctl.close()

    def test_060_binding_without_allow_exec_is_e_cap(self):
        self.bind_exec("deck/control/keys.9/down", ["/bin/true"], allow_exec=False)
        sub = self.sub_system()
        try:
            inj = self.stack.inject([{"module": "keys", "keys": [9]}])
            self.assertEqual(inj.returncode, 0, inj.stderr)
            execs, errors = self.wait_system(sub)
            self.assertEqual(len(errors), 1, "errors=%r" % errors)
            self.assertEqual(errors[0]["error"]["code"], "E_CAP", errors[0])
            self.assertIn("allow_exec", errors[0]["error"]["message"])
            self.assertEqual(len(execs), 1, "execs=%r" % execs)
            p = execs[0]
            self.assertEqual(p["decision"], "refused")
            self.assertEqual(p["code"], "E_CAP")
            self.assertNotIn("pid", p)
        finally:
            sub.close()

    def test_070_cwd_outside_fs_roots_is_refused(self):
        self.bind_exec("deck/control/keys.10/down", ["/bin/true"], cwd="/etc")
        sub = self.sub_system()
        try:
            inj = self.stack.inject([{"module": "keys", "keys": [10]}])
            self.assertEqual(inj.returncode, 0, inj.stderr)
            execs, errors = self.wait_system(sub)
            self.assertEqual(len(execs), 1, "execs=%r" % execs)
            p = execs[0]
            self.assertEqual(p["decision"], "refused")
            self.assertEqual(p["code"], "E_CAP")
            self.assertEqual(p["cwd"], "/etc")
            self.assertIn("outside", p["reason"])
            self.assertEqual(len(errors), 1)
            self.assertEqual(errors[0]["error"]["code"], "E_CAP")
            self.assertEqual(exec_child_processes(), [])
        finally:
            sub.close()

    def test_080_metacharacter_argument_arrives_literally(self):
        weird = str(self.tmp / "meta$(touch pwned3)`id`; rm -rf")
        self.bind_exec("deck/control/keys.11/down", ["/usr/bin/touch", weird])
        sub = self.sub_system()
        try:
            inj = self.stack.inject([{"module": "keys", "keys": [11]}])
            self.assertEqual(inj.returncode, 0, inj.stderr)
            ev = self.next_event(sub, want_topic=EXEC_TOPIC)
            self.assertEqual(ev["payload"]["decision"], "allow")
            self.assertEqual(ev["payload"]["argv"][1], weird)
            deadline = time.monotonic() + EVENT_TIMEOUT_S
            while not Path(weird).exists() and time.monotonic() < deadline:
                time.sleep(0.05)
            self.assertTrue(Path(weird).exists(),
                            "the file must exist with the LITERAL metacharacter name")
            self.assertFalse((self.tmp / "pwned3").exists(),
                             "a shell would have run $(touch pwned3) — no interpolation may happen")
        finally:
            sub.close()

    def test_090_rlimit_cpu_kills_a_busy_binary(self):
        bid = self.bind_exec("deck/control/keys.12/down", ["/usr/bin/yes"])
        sub = self.sub_system()
        try:
            t0 = time.monotonic()
            inj = self.stack.inject([{"module": "keys", "keys": [12]}])
            self.assertEqual(inj.returncode, 0, inj.stderr)
            ev = self.next_event(sub, want_topic=EXEC_TOPIC)
            p = ev["payload"]
            self.assertEqual(p["decision"], "allow", p)
            pid = p["pid"]
            self.assertGreater(pid, 0)
            self.assertTrue(pid_alive(pid), "the CPU hog starts")
            # RLIMIT_CPU 10 s (SIGKILL at the hard limit): dead within ~15 s wall
            deadline = time.monotonic() + RLIMIT_KILL_TIMEOUT_S
            while pid_alive(pid) and time.monotonic() < deadline:
                time.sleep(0.25)
            elapsed = time.monotonic() - t0
            self.assertFalse(pid_alive(pid),
                             "/usr/bin/yes must be killed by RLIMIT_CPU (pid %d alive after %.1fs)"
                             % (pid, elapsed))
            self.assertLess(elapsed, RLIMIT_KILL_TIMEOUT_S,
                            "the kill must come from the CPU limit (~10 s), not a hang")
            self.assertGreater(elapsed, 5,
                               "the child must not be killed instantly (%.1fs) — the rlimit did it"
                               % elapsed)
            # the daemon is still healthy
            self.assertIsInstance(self.rev(), int)
        finally:
            sub.close()

    def test_100_refused_exec_still_runs_remaining_steps(self):
        label = "/controls/keys/13/label"
        bid = self.bind_exec(
            "deck/control/keys.13/down",
            ["/bin/sh", "-c", "touch %s" % (self.tmp / "pwn4")],
            extra_steps=[{"do": "set", "path": label, "value": "after-refusal"}],
        )
        sub = self.sub_system()
        try:
            rev0 = self.rev()
            inj = self.stack.inject([{"module": "keys", "keys": [13]}])
            self.assertEqual(inj.returncode, 0, inj.stderr)
            execs, errors = self.wait_system(sub)
            self.assertEqual(len(errors), 1, "errors=%r" % errors)
            self.assertEqual(errors[0]["binding"], bid)
            self.assertEqual(errors[0]["error"]["code"], "E_CAP")
            self.assertEqual(len(execs), 1, "execs=%r" % execs)
            self.assertEqual(execs[0]["decision"], "refused")
            # the remaining set step still ran
            deadline = time.monotonic() + EVENT_TIMEOUT_S
            val = None
            while time.monotonic() < deadline:
                r = self.get_json(label)
                val = r["value"]
                if val == "after-refusal":
                    break
                time.sleep(0.05)
            self.assertEqual(val, "after-refusal",
                             "remaining steps still run after a refused exec")
            self.assertEqual(self.rev(), rev0 + 1, "the set step bumped rev exactly once")
            self.assertFalse((self.tmp / "pwn4").exists())
        finally:
            sub.close()


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
    raise TimeoutError("exec conformance suite exceeded the %ss watchdog" % SUITE_TIMEOUT_S)


def main():
    signal.signal(signal.SIGALRM, _alarm)
    signal.alarm(SUITE_TIMEOUT_S)
    try:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(Exec)
        runner = unittest.TextTestRunner(stream=sys.stdout, verbosity=0,
                                         resultclass=LineResult)
        result = runner.run(suite)
    finally:
        signal.alarm(0)
    n = result.testsRun
    bad = len(result.failures) + len(result.errors)
    print("-" * 60)
    print("exec conformance: %d tests, %d passed, %d failed/errored, %d skipped"
          % (n, n - bad - len(result.skipped), bad, len(result.skipped)))
    if bad:
        print("RESULT: FAIL")
        return 1
    print("RESULT: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
