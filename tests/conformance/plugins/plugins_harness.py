"""Shared harness for the DS-053 plugin-abuse conformance suite.

Black-box only: the real `ds`, `ds-daemon`, `ds-vdev` binaries, the raw NDJSON
control socket, and plugin fixtures that are tiny raw-stdio Python scripts generated
by the test itself (stdlib only, no third-party deps, no ds-plugin-py needed).

One `Stack` per test case: a fresh temp dir, `XDG_DATA_HOME=<tmp>/xdg` (so the
daemon's plugin host scans `<tmp>/xdg/deckstack/plugins/`), ds-vdev on
`<tmp>/vdev.sock`, `ds daemon start --device virtual:<sock> --data-dir <tmp>/data`.
Plugins are installed by writing `<id>/manifest.json` + `plugin.py` + `behavior.json`
under the xdg plugins dir BEFORE the first plugin step (the host is created lazily on
the first `plugin` binding step and scans the dir once).

Invocation path (phase-1 surface): `ds bind app/abuse/<case> --do 'plugin <id>.<action>'`
then IPC `{"op":"emit","topic":"app/abuse/<case>"}`; the step's typed error is
published as a `deck/system/binding-error` event (`/deck/system/*` subscription), the
plugin lifecycle/stats are read from `get /plugins/<id>`, plugin stderr/log lines land
in `<data>/daemon.log`.

Every wait carries a timeout; `Stack.stop()` always stops the daemon, SIGTERMs then
SIGKILLs daemon+vdev, sweeps any leftover plugin process (cmdline contains the fixture
`plugin.py` and cwd under the stack's xdg), and removes the temp dir, so the run can
never hang or leak processes.
"""

import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]

BUILD_TIMEOUT_S = 900
OP_TIMEOUT_S = 20
BOOT_TIMEOUT_S = 30
READY_TIMEOUT_S = 25
CASE_TIMEOUT_S = 90
KILL_TIMEOUT_S = 5

BINARIES = ("ds", "ds-daemon", "ds-vdev", "ds-vdevctl")

#: the interpreter the fixture plugins run under (must live under /usr so the
#: Landlock policy of the plugin child — read+execute on /usr — lets it start)
PLUGIN_PYTHON = "/usr/bin/python3" if os.path.isfile("/usr/bin/python3") else sys.executable


def log(msg):
    if "--verbose" in sys.argv:
        print(msg, file=sys.stderr)


def bin_dir():
    env = os.environ.get("DS_BIN_DIR")
    if env:
        return Path(env)
    return ROOT / "target" / "debug"


def ensure_binaries():
    """Build the four binaries if any is missing (bounded)."""
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
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    missing = [b for b in BINARIES if not (d / b).is_file()]
    if missing:
        raise RuntimeError("cargo build finished but still missing: %s" % ", ".join(missing))
    return d


# ---------------------------------------------------------------------------
# the raw-stdio fixture plugin (generated into every plugin dir as plugin.py)
# ---------------------------------------------------------------------------

PLUGIN_PY = r'''"""Raw-stdio ds plugin fixture (tests/conformance/plugins) - stdlib only.

Speaks the wire protocol of docs/contracts/plugins.md section 1 directly:
hello -> register (echoing the on-disk manifest so the host's equality check
passes) -> invoke/event/reply loop. Behavior is data: behavior.json next to this
file selects the mode and the parameters. Probe replies are recorded into the
plugin's own state subtree /plugins/<id>/state/probe (writable subset) so the test
can read them with `ds get`; log lines land in the daemon log.
"""
import json
import os
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
with open(os.path.join(HERE, "manifest.json")) as _f:
    MANIFEST = json.load(_f)
with open(os.path.join(HERE, "behavior.json")) as _f:
    BEHAVIOR = json.load(_f)
PID = MANIFEST["id"]
TAG = BEHAVIOR.get("tag", "FIX")

RESULTS = []
_probes = {}       # request_id -> what (net/fs probes recorded on reply)
_internal = set()  # request ids of our own state-recording requests


def send(obj):
    sys.stdout.write(json.dumps(obj, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def log(msg):
    send({"op": "log", "message": "%s %s" % (TAG, msg)})


def record(entry):
    RESULTS.append(entry)
    rid = 9000 + len(RESULTS)
    _internal.add(rid)
    try:
        send({"op": "request", "request_id": rid, "kind": "set",
              "path": "/plugins/%s/state/probe" % PID, "value": RESULTS})
    except Exception:
        pass


def read_msg():
    line = sys.stdin.readline()
    if not line:
        return None
    try:
        return json.loads(line)
    except Exception:
        return {"op": "__bad__"}


def handle(msg):
    if msg.get("op") == "reply":
        rid = msg.get("request_id")
        if rid in _probes:
            record({"rid": rid, "what": _probes[rid], "ok": msg.get("ok"),
                    "code": msg.get("code"), "message": msg.get("message"),
                    "status": msg.get("status"), "body": msg.get("body")})
        # replies to our own state-set requests and to the invoke: ignored
    # events and anything else: ignored


def wait_reply(rid, timeout):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        msg = read_msg()
        if msg is None:
            return None
        handle(msg)
        if msg.get("op") == "reply" and msg.get("request_id") == rid:
            return msg
    return None


def reply_effects(rid, effects):
    send({"op": "effects", "request_id": rid, "effects": effects})


def direct_connect():
    """Try to open a raw TCP connection from the plugin process itself (S4/T11:
    under Landlock ABI>=4 this must fail; the host-mediated path is the only way)."""
    try:
        import socket
        s = socket.create_connection(
            (BEHAVIOR["direct_host"], BEHAVIOR["direct_port"]), timeout=3)
        s.close()
        record({"rid": 999, "what": "direct-connect", "ok": True})
    except Exception as e:
        record({"rid": 999, "what": "direct-connect", "ok": False,
                "error": "%s: %s" % (type(e).__name__, e)})


def handle_invoke(msg):
    rid = msg.get("request_id")
    mode = BEHAVIOR["mode"]
    if mode == "hang":
        while True:  # never answer the invoke; the host times it out
            time.sleep(60)
    if mode == "crash":
        log("CRASH exiting now")
        sys.stdout.flush()
        os._exit(3)
    if mode == "forkbomb":
        ok = err = 0
        log("FORKBOMB start")
        while ok + err < 300:  # bounded burst, then the summary line must land
            try:
                pid = os.fork()
                if pid == 0:
                    os._exit(0)  # children die immediately
                ok += 1
            except OSError:
                err += 1
            time.sleep(0.001)
        log("FORKBOMB forks_ok=%d fork_errors=%d" % (ok, err))
        while True:
            pass  # burn CPU until RLIMIT_CPU kills the process; never reply
    if mode == "membomb":
        log("MEMBOMB start")
        chunk = BEHAVIOR.get("chunk_mb", 8) << 20
        chunks = []
        n = 0
        try:
            while True:
                chunks.append(bytearray(chunk))
                n += BEHAVIOR.get("chunk_mb", 8)
        except MemoryError:
            log("MEMBOMB allocated_mb=%d then MemoryError (RLIMIT_AS)" % n)
            sys.stdout.flush()
            os._exit(7)
    if mode == "capvio":
        send({"op": "request", "request_id": 100, "kind": "get", "path": "/controls"})
        r = wait_reply(100, 5)
        log("CAPVIO get_reply ok=%s code=%s" % (r and r.get("ok"), r and r.get("code")))
        reply_effects(rid, [
            {"effect": "set", "path": "/controls/keys/0/label", "value": "pwned"},
            {"effect": "set", "path": "/policy/net/allow_hosts", "value": ["evil.com"]},
            {"effect": "emit", "topic": "plugin/%s/hit" % PID, "payload": {"ok": True}},
        ])
        return
    if mode == "fsprobe":
        for i, key in enumerate(("outside", "traversal", "inside")):
            prid = 100 + i
            _probes[prid] = "fs:" + key
            send({"op": "request", "request_id": prid, "kind": "fs",
                  "path": BEHAVIOR[key]})
        for i in range(3):
            wait_reply(100 + i, 5)
        direct = {}
        for key in ("outside", "inside"):
            try:
                with open(BEHAVIOR[key], "rb") as f:
                    direct[key] = "read_bytes=%d" % len(f.read())
            except Exception as e:
                direct[key] = "denied:%s" % type(e).__name__
        reply_effects(rid, [
            {"effect": "set", "path": "/plugins/%s/state/direct" % PID, "value": direct},
            {"effect": "set", "path": "/policy/net/allow_hosts", "value": ["evil.com"]},
            {"effect": "set", "path": "/controls/keys/5/label", "value": "fs-ok"},
        ])
        return
    if mode == "netprobe":
        for i, url in enumerate(BEHAVIOR.get("urls", [])):
            prid = 100 + i
            _probes[prid] = url
            send({"op": "request", "request_id": prid, "kind": "net",
                  "method": "GET", "url": url})
        direct_connect()
        reply_effects(rid, [])  # complete the invoke now; net replies recorded async
        return
    reply_effects(rid, [])


def main():
    hello = read_msg()
    if not hello or hello.get("op") != "hello":
        os._exit(120)
    send({"op": "register", "id": PID, "manifest": MANIFEST})
    while True:
        msg = read_msg()
        if msg is None:
            break
        if msg.get("op") == "invoke":
            try:
                handle_invoke(msg)
            except Exception as e:
                log("HANDLER error=%r" % (e,))
                try:
                    reply_effects(msg.get("request_id"), [])
                except Exception:
                    pass
        else:
            handle(msg)


if __name__ == "__main__":
    main()
'''


def make_manifest(plugin_id, capabilities, actions=("go",), resources=None,
                  fs_roots=None, events=None, name=None):
    """Build a manifest.json dict for one fixture plugin."""
    m = {
        "id": plugin_id,
        "name": name or plugin_id,
        "version": "1.0.0",
        "command": [PLUGIN_PYTHON, "plugin.py"],
        "capabilities": list(capabilities),
        "actions": [{"id": a, "params": {"type": "object"}} for a in actions],
    }
    if resources:
        m["resources"] = resources
    if fs_roots:
        m["fs_roots"] = list(fs_roots)
    if events:
        m["events"] = list(events)
    return m


def install_plugin(xdg, manifest, behavior, extra_files=None):
    """Write one fixture plugin under <xdg>/deckstack/plugins/<id>/."""
    pid = manifest["id"]
    d = Path(xdg) / "deckstack" / "plugins" / pid
    d.mkdir(parents=True, exist_ok=True)
    (d / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    (d / "plugin.py").write_text(PLUGIN_PY)
    (d / "behavior.json").write_text(json.dumps(behavior) + "\n")
    for name, content in (extra_files or {}).items():
        (d / name).write_text(content)
    return d


# ---------------------------------------------------------------------------
# process inspection helpers (no pgrep dependency)
# ---------------------------------------------------------------------------

def pid_alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


def uid_proc_count(uid=None):
    uid = os.getuid() if uid is None else uid
    n = 0
    for p in os.listdir("/proc"):
        if not p.isdigit():
            continue
        try:
            with open("/proc/%s/status" % p) as f:
                for line in f:
                    if line.startswith("Uid:"):
                        if line.split()[1] == str(uid):
                            n += 1
                        break
        except OSError:
            pass
    return n


def scan_procs(match_cmd=None, under_cwd=None):
    """All /proc pids whose cmdline contains `match_cmd` and whose cwd is under
    `under_cwd` (any cwd when None). Returns [(pid, cwd, cmdline)]."""
    out = []
    for p in os.listdir("/proc"):
        if not p.isdigit():
            continue
        try:
            with open("/proc/%s/cmdline" % p, "rb") as f:
                cl = f.read()
            if match_cmd and match_cmd.encode() not in cl:
                continue
            cwd = os.readlink("/proc/%s/cwd" % p)
            if under_cwd and not cwd.startswith(str(under_cwd)):
                continue
            out.append((int(p), cwd, cl.decode(errors="replace")))
        except OSError:
            continue
    return out


# ---------------------------------------------------------------------------
# fake lights server (T11: the allowed host, end to end)
# ---------------------------------------------------------------------------

class FakeLights:
    """A tiny HTTP server on 127.0.0.1:<ephemeral> standing in for lights.lan."""

    def __init__(self, body=b"lamp-on"):
        self.body = body
        outer = self

        class H(BaseHTTPRequestHandler):
            def do_GET(self):
                if self.path.startswith("/api/lamp"):
                    self.send_response(200)
                    self.send_header("Content-Type", "text/plain")
                    self.send_header("Content-Length", str(len(outer.body)))
                    self.end_headers()
                    self.wfile.write(outer.body)
                else:
                    self.send_response(404)
                    self.send_header("Content-Length", "0")
                    self.end_headers()

            def log_message(self, *a):
                pass

        self.httpd = HTTPServer(("127.0.0.1", 0), H)
        self.port = self.httpd.server_address[1]
        self.thread = threading.Thread(target=self.httpd.serve_forever,
                                       name="fake-lights", daemon=True)
        self.thread.start()

    def stop(self):
        self.httpd.shutdown()
        self.httpd.server_close()
        self.thread.join(timeout=KILL_TIMEOUT_S)


# ---------------------------------------------------------------------------
# IPC client
# ---------------------------------------------------------------------------

class Ipc:
    """Raw NDJSON control-socket client (black box). Line reading goes through raw
    recv into a buffer (never socket.makefile + changing timeouts: a timed-out
    makefile object is permanently unusable). The connection carries subscriptions,
    so events interleave with replies: `rpc` skips event lines (queued in
    `pending_events`, drained by `next_event`)."""

    def __init__(self, sock_path, timeout=OP_TIMEOUT_S):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(timeout)
        self.sock.connect(str(sock_path))
        self.buf = b""
        self.timeout = timeout
        self.pending_events = []

    def send(self, obj):
        self.sock.sendall(json.dumps(obj).encode() + b"\n")

    def recv(self, timeout=None):
        t = self.timeout if timeout is None else timeout
        deadline = time.monotonic() + t
        while b"\n" not in self.buf:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise socket.timeout("recv timed out after %ss" % t)
            self.sock.settimeout(remaining)
            chunk = self.sock.recv(65536)
            if not chunk:
                raise AssertionError("daemon closed the control connection")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        return json.loads(line.decode())

    def rpc(self, obj, timeout=None):
        """Send a request and read until ITS reply (id match) arrives; interleaved
        subscription events are queued in pending_events."""
        rid = obj.get("id")
        self.send(obj)
        deadline = time.monotonic() + (self.timeout if timeout is None else timeout)
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise socket.timeout("rpc %r timed out" % obj.get("op"))
            msg = self.recv(timeout=remaining)
            if msg.get("id") == rid and ("ok" in msg or "error" in msg):
                return msg
            if msg.get("op") == "event":
                self.pending_events.append(msg)
            # anything else (e.g. a late reply to an older request): ignore

    def next_event(self, timeout=None):
        """The next subscription event (queued ones first)."""
        if self.pending_events:
            return self.pending_events.pop(0)
        t = self.timeout if timeout is None else timeout
        deadline = time.monotonic() + t
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise socket.timeout("no event within %ss" % t)
            msg = self.recv(timeout=remaining)
            if msg.get("op") == "event":
                return msg
            # a stray non-event line: ignore

    def hello_auth(self, token):
        r = self.rpc({"id": 0, "op": "hello", "protocol": 1, "client": "ds-053/1"})
        assert r["ok"], r
        r = self.rpc({"id": 1, "op": "auth", "token": token, "scope": "admin"})
        assert r["ok"], r
        return self

    def subscribe(self, pattern):
        r = self.rpc({"id": 2, "op": "subscribe", "path": pattern})
        assert r["ok"], r

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


# ---------------------------------------------------------------------------
# the stack
# ---------------------------------------------------------------------------

class Stack:
    """One booted ds-vdev + ds-daemon pair on a fresh temp dir with its own
    XDG_DATA_HOME (the daemon's plugin dir)."""

    def __init__(self, bins):
        self.bins = bins
        self.tmp = Path(tempfile.mkdtemp(prefix="ds-053-plugins-"))
        self.xdg = self.tmp / "xdg"
        self.data = self.tmp / "data"
        self.vdev_sock = self.tmp / "vdev.sock"
        self.sock = self.data / "daemon.sock"
        self.daemon_log = self.data / "daemon.log"
        self.token = ""
        self.vdev = None
        self.daemon_pid = None
        self.plugin_pids = set()

    # ---- setup -------------------------------------------------------------

    def env(self):
        env = dict(os.environ)
        env["DS_DAEMON_BIN"] = str(self.bins / "ds-daemon")
        env["XDG_DATA_HOME"] = str(self.xdg)
        return env

    def start(self):
        self.vdev = subprocess.Popen(
            [str(self.bins / "ds-vdev"), "--socket", str(self.vdev_sock)],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            stdin=subprocess.DEVNULL, cwd=str(ROOT),
        )
        self._wait_for_file(self.vdev_sock, 10, "ds-vdev device socket")
        p = subprocess.run(
            [str(self.bins / "ds"), "daemon", "start",
             "--device", "virtual:%s" % self.vdev_sock, "--data-dir", str(self.data)],
            capture_output=True, text=True, timeout=BOOT_TIMEOUT_S, env=self.env(),
            cwd=str(ROOT),
        )
        if p.returncode != 0:
            raise RuntimeError(
                "ds daemon start failed rc=%d\nstdout: %s\nstderr: %s\ndaemon.log: %s"
                % (p.returncode, p.stdout, p.stderr, self.read_log())
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
    def _wait_for_file(path, timeout_s, what):
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            if path.exists():
                return
            time.sleep(0.05)
        raise RuntimeError("timed out after %ss waiting for %s (%s)" % (timeout_s, what, path))

    def read_log(self, tail=6000):
        try:
            return self.daemon_log.read_text()[-tail:]
        except OSError:
            return "<no daemon log>"

    # ---- driving -----------------------------------------------------------

    def ds(self, *args, timeout=OP_TIMEOUT_S):
        return subprocess.run(
            [str(self.bins / "ds"), "--socket", str(self.sock), "--data-dir", str(self.data)]
            + list(args),
            capture_output=True, text=True, timeout=timeout, env=self.env(), cwd=str(ROOT),
        )

    def ipc(self):
        return Ipc(self.sock)

    def get_json(self, path):
        p = self.ds("get", path, "--json")
        if p.returncode != 0:
            raise RuntimeError("ds get %s rc=%d stderr=%s" % (path, p.returncode, p.stderr))
        return json.loads(p.stdout)

    def try_get_value(self, path):
        """Value at path, or None when the path does not exist (E_PATH, exit 2)."""
        p = self.ds("get", path, "--json")
        if p.returncode == 0:
            return json.loads(p.stdout)["value"]
        if p.returncode == 2:
            return None
        raise RuntimeError("ds get %s rc=%d stderr=%s" % (path, p.returncode, p.stderr))

    def set_json(self, path, value):
        p = self.ds("set", path, "--json", json.dumps(value))
        if p.returncode != 0:
            raise RuntimeError("ds set %s rc=%d stderr=%s" % (path, p.returncode, p.stderr))
        return json.loads(p.stdout)

    def bind(self, topic, *do_steps):
        args = ["bind", topic]
        for s in do_steps:
            args += ["--do", s]
        p = self.ds(*args)
        if p.returncode != 0:
            raise RuntimeError("ds bind %s rc=%d stderr=%s" % (topic, p.returncode, p.stderr))
        return p.stdout

    def emit(self, c, topic, payload=None):
        r = c.rpc({"id": 100, "op": "emit", "topic": topic,
                   "payload": payload if payload is not None else {}})
        if not r.get("ok"):
            raise RuntimeError("emit %s failed: %s" % (topic, r))
        return r

    def plugin_state(self, pid):
        """/plugins/<id> value, or None when the plugin is not in the host map."""
        return self.try_get_value("/plugins/%s" % pid)

    def wait_plugin(self, pid, want="running", timeout=READY_TIMEOUT_S):
        """Poll /plugins/<id> until status == want; returns the value."""
        deadline = time.monotonic() + timeout
        last = None
        while time.monotonic() < deadline:
            v = self.plugin_state(pid)
            last = v
            if v and v.get("status") == want:
                return v
            time.sleep(0.1)
        raise AssertionError(
            "plugin %s did not reach status=%s within %ss (last: %r; daemon.log tail: %s)"
            % (pid, want, timeout, last, self.read_log(2500))
        )

    def wait_plugin_pred(self, pid, pred, timeout=CASE_TIMEOUT_S, what="condition"):
        deadline = time.monotonic() + timeout
        last = None
        while time.monotonic() < deadline:
            v = self.plugin_state(pid)
            last = v
            if v and pred(v):
                return v
            time.sleep(0.1)
        raise AssertionError(
            "plugin %s did not satisfy %s within %ss (last: %r; log: %s)"
            % (pid, what, timeout, last, self.read_log(2500))
        )

    def wait_binding_error(self, c, timeout=OP_TIMEOUT_S):
        """Read subscribed events until the next deck/system/binding-error; returns
        its payload {binding, step, error:{code,message}, topic, depth}."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            remaining = max(0.2, deadline - time.monotonic())
            try:
                ev = c.next_event(timeout=remaining)
            except socket.timeout:
                break
            if ev.get("topic") == "deck/system/binding-error":
                return ev["payload"]
        raise AssertionError("no deck/system/binding-error within %ss" % timeout)

    def drain_events(self, c, seconds=0.4):
        """Consume pending events for a short bounded time (best effort)."""
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            try:
                c.next_event(timeout=max(0.05, deadline - time.monotonic()))
            except (socket.timeout, TimeoutError):
                break
            except AssertionError:
                break

    def daemon_alive(self, c):
        """The daemon still answers: raw ping + a CLI get."""
        try:
            r = c.rpc({"id": 900, "op": "ping"}, timeout=5)
            if not (r.get("ok") and r.get("result", {}).get("pong")):
                return False
            p = self.ds("get", "/meta", "--json", timeout=10)
            return p.returncode == 0
        except Exception:
            return False

    def pid_gone(self, pid):
        """The plugin pid is gone (no /proc entry, or not a plugin process any more)."""
        if pid is None:
            return True
        try:
            with open("/proc/%d/cmdline" % pid, "rb") as f:
                cl = f.read()
            if b"plugin.py" in cl:
                return False  # pid alive and still a plugin
            return True  # reused by an unrelated process
        except OSError:
            return True

    # ---- teardown ----------------------------------------------------------

    def stop(self):
        """Mandatory cleanup. Returns a list of problems (empty = clean)."""
        problems = []
        try:
            subprocess.run(
                [str(self.bins / "ds"), "--socket", str(self.sock),
                 "--data-dir", str(self.data), "daemon", "stop"],
                capture_output=True, text=True, timeout=OP_TIMEOUT_S,
                env=self.env(), cwd=str(ROOT),
            )
        except Exception as e:
            log("daemon stop op failed (continuing with signals): %r" % e)
        targets = []
        if self.vdev is not None and self.vdev.poll() is None:
            targets.append(("popen", self.vdev))
        if self.daemon_pid is not None and pid_alive(self.daemon_pid):
            targets.append(("pid", self.daemon_pid))
        for kind, t in targets:
            try:
                if kind == "popen":
                    t.terminate()
                else:
                    os.kill(t, signal.SIGTERM)
            except OSError:
                pass
        deadline = time.monotonic() + KILL_TIMEOUT_S
        while time.monotonic() < deadline and any(
                (t.poll() is None) if k == "popen" else pid_alive(t) for k, t in targets):
            time.sleep(0.05)
        for kind, t in targets:
            try:
                if kind == "popen":
                    if t.poll() is None:
                        t.kill()
                else:
                    if pid_alive(t):
                        os.kill(t, signal.SIGKILL)
            except OSError:
                pass
        # sweep any leftover plugin process of this stack (kill-on-exit should
        # have got them; a survivor is a finding)
        leftover = scan_procs(match_cmd="plugin.py", under_cwd=self.xdg)
        for pid, cwd, cl in leftover:
            try:
                os.kill(pid, signal.SIGKILL)
            except OSError:
                pass
        if leftover:
            time.sleep(0.3)
            still = scan_procs(match_cmd="plugin.py", under_cwd=self.xdg)
            if still:
                problems.append("plugin processes leaked after daemon stop: %s" % still)
        try:
            shutil.rmtree(self.tmp, ignore_errors=True)
        except Exception as e:
            problems.append("temp dir removal failed: %r" % e)
        if self.tmp.exists():
            problems.append("temp dir still exists: %s" % self.tmp)
        return problems
