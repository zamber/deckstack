"""deckstack — thin stdlib-only Python helper over the `ds` CLI.

The helper is optional sugar (R1): everything it does is reachable with the `ds`
CLI alone. It never reimplements daemon logic and never opens the control socket
directly — every call is one `ds` subprocess (argv-only, never ``shell=True``),
so the CLI contract (docs/contracts/cli.md) stays the single source of truth.

Discovery contract (how an agent finds capabilities): run ``ds --help``,
``ds <group> --help``, ``ds schema <name>`` and ``ds describe <path>``; MCP
clients use ``ds-mcp`` ``tools/list`` (docs/components/mcp-bridge.md).
"""

import json
import subprocess

from .errors import DeckError

DEFAULT_TIMEOUT_S = 20.0


class WatchStream:
    """A live ``ds watch … --ndjson`` / ``ds events --ndjson`` process.

    Iterating yields one parsed JSON event object per line. Closing (or exiting
    the ``with`` block) terminates the watcher. Streaming verbs accept only
    ``--ndjson`` — passing ``--json`` to them is a usage error (exit 1, S9).
    """

    def __init__(self, popen):
        self._p = popen
        self._stdout = popen.stdout

    def __iter__(self):
        return self

    def __next__(self):
        line = self._stdout.readline()
        if not line:
            self._finish()
            raise StopIteration
        line = line.strip()
        if not line:
            return self.__next__()
        return json.loads(line)

    def _finish(self):
        if self._p.poll() is None:
            self._p.terminate()
            try:
                self._p.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self._p.kill()
                self._p.wait(timeout=5)

    def close(self):
        self._finish()

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()
        return False


class DeckClient:
    """Typed helper for the `ds` CLI. Use as a context manager or plain object."""

    def __init__(self, ds="ds", socket=None, data_dir=None, token_file=None,
                 timeout=DEFAULT_TIMEOUT_S, cwd=None, env=None):
        self.ds = ds
        self.timeout = timeout
        self.cwd = cwd
        self._base = [ds]
        if socket is not None:
            self._base += ["--socket", str(socket)]
        if data_dir is not None:
            self._base += ["--data-dir", str(data_dir)]
        if token_file is not None:
            self._base += ["--token-file", str(token_file)]
        self._env = env

    # -- process plumbing ----------------------------------------------------

    def _run(self, args, json_output=False, timeout=None):
        """Run one `ds` invocation. Returns the CompletedProcess.

        Raises DeckError on any nonzero exit (message carries `[exit N]`).
        With json_output, stdout must parse as exactly one JSON object.
        """
        cmd = self._base + [str(a) for a in args]
        eff_timeout = self.timeout if timeout is None else timeout
        try:
            p = subprocess.run(
                cmd, capture_output=True, text=True, shell=False,
                timeout=eff_timeout,
                cwd=self.cwd, env=self._env,
            )
        except FileNotFoundError:
            raise DeckError("ds binary not found: %r (set ds= or PATH)" % self.ds)
        except subprocess.TimeoutExpired:
            raise DeckError("ds %s timed out after %ss" % (" ".join(map(str, args)),
                                                            eff_timeout))
        if p.returncode != 0:
            raise DeckError(self._error_message(p), exit_code=p.returncode,
                            stderr=p.stderr, stdout=p.stdout)
        if json_output:
            self._parse_one_object(p.stdout, args)
        return p

    @staticmethod
    def _error_message(p):
        """Human message from stderr: with --json the CLI prints a JSON envelope
        `{"ok":false,"error":{…}}` on stderr — unwrap it; human mode already has
        the `[exit N]` suffix which DeckError normalises."""
        text = (p.stderr.strip() or p.stdout.strip())
        try:
            obj = json.loads(text)
            if isinstance(obj, dict) and isinstance(obj.get("error"), dict):
                return str(obj["error"].get("message", text))
        except ValueError:
            pass
        return text

    @staticmethod
    def _parse_one_object(stdout, args):
        text = stdout.strip()
        try:
            obj = json.loads(text)
        except ValueError:
            raise DeckError("ds %s: stdout is not one JSON object: %.200r"
                            % (" ".join(map(str, args)), text))
        if not isinstance(obj, dict):
            raise DeckError("ds %s: stdout is not a JSON object: %.200r"
                            % (" ".join(map(str, args)), text))
        return obj

    def _json(self, args, timeout=None):
        """Run with --json appended; return the parsed single JSON object."""
        p = self._run(list(args) + ["--json"], json_output=True, timeout=timeout)
        return self._parse_one_object(p.stdout, args)

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False

    # -- discovery -----------------------------------------------------------

    def help(self, *args):
        """`ds --help` or `ds <group> --help` text — the capability index."""
        if args:
            return self._run(list(args) + ["--help"]).stdout
        return self._run(["--help"]).stdout

    def schema(self, name):
        """`ds schema <state|ipc|stream-frame|plugin-manifest|binding>` as a dict."""
        p = self._run(["schema", name])
        return self._parse_one_object(p.stdout, ["schema", name])

    def describe(self, path):
        """`ds describe <path>`: schema + current value + allowed verbs (text)."""
        return self._run(["describe", path]).stdout

    # -- core verbs ----------------------------------------------------------

    def status(self):
        """`ds daemon status --json` -> {"running":…, "device":…, "pid":…}."""
        return self._json(["daemon", "status"])

    def get(self, path, fields=None, rev=None):
        """`ds get <path> --json` -> {"path":…,"value":…,"rev":…} (/telemetry/* has no rev)."""
        args = ["get", path]
        if fields:
            args += ["--fields", ",".join(fields)]
        if rev is not None:
            args += ["--rev", str(rev)]
        return self._json(args)

    def set(self, path, value, if_rev=None, file=None):
        """`ds set <path> <json-value|--file f> [--if-rev N]` -> the new rev (int).

        `value` is any JSON-serialisable Python object (json-encoded for the CLI);
        pass file= to use `--file` instead (confined to /policy/fs/roots + /tmp).
        """
        args = ["set", path]
        if file is not None:
            args += ["--file", str(file)]
        else:
            args.append(json.dumps(value))
        if if_rev is not None:
            args += ["--if-rev", str(if_rev)]
        obj = self._json(args)
        return int(obj.get("rev", 0))

    def bind(self, topic, *steps, when=None):
        """`ds bind <topic> --do '<step>'… [--when '<cond>']` -> binding id (bNNNN)."""
        args = ["bind", topic]
        for step in steps:
            args += ["--do", step]
        if when is not None:
            args += ["--when", json.dumps(when)]
        obj = self._json(args)
        result = obj.get("result", obj)
        return result.get("id") or result.get("binding")

    def unbind(self, binding_id=None, topic=None):
        """`ds unbind <id> | --topic <topic>` -> list of removed ids."""
        if binding_id is not None:
            args = ["unbind", binding_id]
        elif topic is not None:
            args = ["unbind", "--topic", topic]
        else:
            raise DeckError("unbind needs binding_id= or topic=")
        obj = self._json(args)
        result = obj.get("result", obj)
        return result.get("removed", [])

    def emit(self, topic, payload=None):
        """`ds emit <topic> [json-payload]`.

        NOTE: refused with exit 1 in phase 1 ("not implemented in phase 1",
        bins/ds/src/cli.rs LATER_PHASE) — raises DeckError(exit_code=1) until a
        later ticket lands the verb; use the raw IPC op or a binding meanwhile.
        """
        args = ["emit", topic]
        if payload is not None:
            args.append(json.dumps(payload))
        return self._json(args)

    def watch(self, path, from_rev=None, max_events=None):
        """`ds watch <path> --ndjson` -> WatchStream yielding event objects.

        Streaming verbs accept only --ndjson (S9); --max-events N makes the
        process exit after N events, which also ends the iterator.
        """
        args = ["watch", path, "--ndjson"]
        if from_rev is not None:
            args += ["--from-rev", str(from_rev)]
        if max_events is not None:
            args += ["--max-events", str(max_events)]
        return self._spawn(args)

    def events(self, max_events=None):
        """`ds events --ndjson` -> WatchStream over the whole event bus."""
        args = ["events", "--ndjson"]
        if max_events is not None:
            args += ["--max-events", str(max_events)]
        return self._spawn(args)

    def _spawn(self, args):
        cmd = self._base + [str(a) for a in args]
        try:
            p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                 text=True, shell=False, cwd=self.cwd, env=self._env)
        except FileNotFoundError:
            raise DeckError("ds binary not found: %r (set ds= or PATH)" % self.ds)
        return WatchStream(p)

    # -- stream plane --------------------------------------------------------

    def stream_push(self, target, file, dir=None, fps=None, loop=False):
        """`ds stream push <target> --file f [--dir d --fps N] [--loop]` -> result object.

        Target is one of `keys|key:N|dial` (S6). Not an NDJSON control op: the CLI
        authorises via stream-open on the control socket, then sends DSF1 binary
        frames on stream.sock (docs/contracts/ipc.md section 1.2).
        """
        args = ["stream", "push", target, "--file", str(file)]
        if dir is not None:
            args += ["--dir", str(dir)]
        if fps is not None:
            args += ["--fps", str(fps)]
        if loop:
            args.append("--loop")
        return self._json(args)

    def stream_stats(self, target):
        """`ds stream stats <target> --json` -> stats object (no rev, S1)."""
        return self._json(["stream", "stats", target])

    def stream_stop(self, target):
        """`ds stream stop <target> --json` -> result object."""
        return self._json(["stream", "stop", target])
