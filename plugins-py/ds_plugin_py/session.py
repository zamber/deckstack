"""The serve loop: handshake, register, dispatch, request/reply, shutdown.

Single-threaded and re-entrant-safe: while an action handler waits for a ``reply`` to
one of its own ``request``s, the pump reads host messages and queues everything that is
not the awaited reply; queued messages are dispatched by the top-level loop afterwards,
so a handler may freely call :meth:`Sdk.get_state`, :meth:`Sdk.set_state` and
:meth:`Sdk.http`.

Malformed input (bad JSON, unknown op, oversized line) is logged to stderr and skipped —
a protocol violation never panics. **stdout is the protocol channel and nothing else**:
every diagnostic goes to stderr.

Mirror of ``crates/ds-plugin-sdk/src/session.rs`` (DS-051).
"""

from __future__ import annotations

import inspect
import os
import sys
from collections import deque
from typing import Any, Callable, Optional

from . import http as http_mod
from . import params as params_mod
from . import wire
from .errors import ErrorCode, PluginError, ServeError
from .http import HttpResponse
from .manifest import Manifest

ReadChunk = Callable[[int], bytes]
WriteBytes = Callable[[bytes], None]


class HostEvent:
    """A host event delivered to the plugin's event handler (see :func:`on_event`)."""

    def __init__(self, ev: wire.Event):
        self.event = ev.event
        self.control = ev.control
        self.topic = ev.topic
        self.state = ev.state
        self.payload = ev.payload
        self.request_id = ev.request_id

    @property
    def control_or_topic(self) -> Optional[str]:
        return self.control if self.control is not None else self.topic

    @property
    def state_or_payload(self) -> Any:
        return self.state if self.state is not None else self.payload

    def __repr__(self) -> str:  # pragma: no cover - debugging aid
        return "HostEvent(event=%r, control=%r, topic=%r, request_id=%r)" % (
            self.event, self.control, self.topic, self.request_id)


class ActionEntry:
    def __init__(self, params: Any, handler: Callable, typed: Any = None,
                 name: Optional[str] = None, description: Optional[str] = None):
        self.params = params if params is not None else {"type": "object"}
        self.handler = handler
        self.typed = typed
        self.name = name
        self.description = description

    def def_dict(self) -> dict:
        d: dict = {"id": self.id, "params": self.params}
        if self.name is not None:
            d["name"] = self.name
        if self.description is not None:
            d["description"] = self.description
        return d

    id: str = ""


def _run_with_active(sdk: "Sdk", fn: Callable, *a):
    """Run a handler with ``ds_plugin_py._ACTIVE`` bound to this session, so the
    module-level ``sdk.http``/``sdk.get_state``/… helpers work inside handlers."""
    pkg = sys.modules.get("ds_plugin_py")
    prev = getattr(pkg, "_ACTIVE", None) if pkg is not None else None
    if pkg is not None:
        pkg._ACTIVE = sdk
    try:
        return fn(*a)
    finally:
        if pkg is not None:
            pkg._ACTIVE = prev


def _call_handler(entry: ActionEntry, sdk: "Sdk", args: Any):
    """Call the handler, supporting ``fn(args)``, ``fn(sdk, args)`` and a ``typed``
    conversion of ``args`` first."""
    if entry.typed is not None:
        args = _coerce(entry.typed, args)
    try:
        sig = inspect.signature(entry.handler)
        n = len(
            [p for p in sig.parameters.values()
             if p.kind in (p.POSITIONAL_ONLY, p.POSITIONAL_OR_KEYWORD)]
        )
    except (TypeError, ValueError):
        n = 1
    if n >= 2:
        return entry.handler(sdk, args)
    return entry.handler(args)


def _coerce(typed: Any, args: Any) -> Any:
    """Convert ``args`` into the handler's declared type; mismatch -> ``E_SCHEMA``."""
    if not isinstance(args, dict):
        raise PluginError.schema("args do not deserialize into %r: not an object" % (typed,))
    model_validate = getattr(typed, "model_validate", None)  # pydantic v2
    if callable(model_validate):
        try:
            return model_validate(args)
        except Exception as e:
            raise PluginError.schema("args do not deserialize into %r: %s" % (typed, e))
    if hasattr(typed, "__dataclass_fields__"):
        import dataclasses

        try:
            return typed(**{k: v for k, v in args.items()
                            if k in {f.name for f in dataclasses.fields(typed)}})
        except Exception as e:
            raise PluginError.schema("args do not deserialize into %r: %s" % (typed, e))
    try:
        return typed(**args)
    except Exception as e:
        raise PluginError.schema("args do not deserialize into %r: %s" % (typed, e))


def _as_effects(result: Any) -> list:
    if result is None:
        return []
    if isinstance(result, dict):
        result = [result]
    if not isinstance(result, list):
        raise PluginError.internal("handler returned %r, expected a list of effects" % (result,))
    out = []
    for item in result:
        if not isinstance(item, dict) or not isinstance(item.get("effect"), str):
            raise PluginError.schema("handler effect %r is not an effect object" % (item,))
        out.append(item)
    return out


class Sdk:
    """The plugin side of one host session. Handlers receive it (or use the module-level
    helpers) for host-mediated requests and to inspect the granted capabilities."""

    def __init__(
        self,
        read_chunk: ReadChunk,
        write: WriteBytes,
        manifest: Manifest,
        actions: dict,
        event_handler: Optional[Callable] = None,
    ):
        self._read_chunk = read_chunk
        self._write = write
        self.manifest = manifest
        self.actions = actions
        self.event_handler = event_handler
        self.granted_caps: list = []
        self._queue: deque = deque()
        self._next_request_id = 1
        self.violations = 0
        self._buf = b""

    # -- diagnostics (stderr only — stdout is the protocol channel) -----------
    def warn(self, msg: str) -> None:
        self.violations += 1
        sys.stderr.write("[ds-plugin-py %s] %s\n" % (self.manifest.id, msg))
        sys.stderr.flush()

    # -- accessors -------------------------------------------------------------
    def granted_capabilities(self) -> list:
        """Capability strings granted by the host in ``hello`` (the host enforces them)."""
        return list(self.granted_caps)

    # -- transport -------------------------------------------------------------
    def _read_line(self):
        """Returns ``(kind, line)``: kind in {"line", "too_long", "eof"}."""
        while True:
            idx = self._buf.find(b"\n")
            if idx >= 0:
                line = self._buf[: idx + 1]
                self._buf = self._buf[idx + 1:]
                return "line", line
            if len(self._buf) > wire.MAX_LINE_BYTES:
                # over-long line: drain the remainder without retaining it
                self._buf = b""
                while True:
                    chunk = self._read_chunk(65536)
                    if not chunk:
                        return "too_long", None
                    j = chunk.find(b"\n")
                    if j >= 0:
                        self._buf = chunk[j + 1:]
                        return "too_long", None
            chunk = self._read_chunk(65536)
            if not chunk:
                if not self._buf:
                    return "eof", None
                line = self._buf
                self._buf = b""
                return "line", line
            self._buf += chunk

    def _next_line(self):
        if self._queue:
            return "line", self._queue.popleft()
        return self._read_line()

    def _send(self, msg: dict) -> None:
        self._write((wire.dumps(msg) + "\n").encode("utf-8"))

    # -- serve loop ------------------------------------------------------------
    def run(self) -> None:
        # 1. handshake: the first host message must be hello with protocol 1.
        kind, raw = self._read_line()
        if kind == "eof":
            raise ServeError(ServeError.UNEXPECTED_EOF, "host closed stdin before the handshake")
        if kind == "too_long":
            raise ServeError(ServeError.EXPECTED_HELLO, "oversized first line")
        line = raw.decode("utf-8", "replace").strip()
        try:
            msg = wire.parse_host_msg(line)
        except ValueError as e:
            raise ServeError(ServeError.EXPECTED_HELLO, "unparseable first line: %s" % e)
        if not isinstance(msg, wire.Hello):
            raise ServeError(ServeError.EXPECTED_HELLO, "first message was %r" % (line[:200],))
        if msg.protocol != wire.PROTOCOL_VERSION:
            raise ServeError(
                ServeError.VERSION,
                "protocol version mismatch: host sent %d, SDK speaks %d"
                % (msg.protocol, wire.PROTOCOL_VERSION),
                got=msg.protocol, want=wire.PROTOCOL_VERSION,
            )
        self.granted_caps = msg.caps
        # 2. register with the manifest.
        self._send(wire.register_msg(self.manifest.id, self.manifest.to_dict()))
        # 3. dispatch until shutdown (explicit op or EOF on stdin).
        while True:
            kind, raw = self._next_line()
            if kind == "eof":
                return  # host closed stdin: clean shutdown
            if kind == "too_long":
                self.warn("skipped oversized line (> 1 MiB)")
                continue
            line = raw.decode("utf-8", "replace").strip()
            if not line:
                continue
            try:
                msg = wire.parse_host_msg(line)
            except ValueError as e:
                self.warn("skipped malformed line: %s" % e)
                continue
            if msg is None:
                self.warn("skipped unrecognized line: %s" % line[:200])
                continue
            if not self._dispatch(msg):
                return

    def _dispatch(self, msg) -> bool:
        """Dispatch one host message. Returns False on shutdown."""
        if isinstance(msg, wire.Hello):
            self.warn("duplicate hello ignored")
        elif isinstance(msg, wire.Shutdown):
            return False
        elif isinstance(msg, wire.Event):
            if self.event_handler is not None:
                ev = HostEvent(msg)
                try:
                    effects = _as_effects(_run_with_active(self, self.event_handler, self, ev))
                except PluginError as e:
                    if msg.request_id is not None:
                        self._send(wire.error_msg(msg.request_id, e))
                    else:
                        self.warn("event handler failed: %s" % e)
                    return True
                except Exception as e:  # handler bug -> E_INTERNAL
                    err = PluginError.internal("event handler raised: %s" % e)
                    if msg.request_id is not None:
                        self._send(wire.error_msg(msg.request_id, err))
                    else:
                        self.warn("event handler failed: %s" % err)
                    return True
                self._send(wire.effects_msg(effects, msg.request_id))
        elif isinstance(msg, wire.Invoke):
            self._dispatch_invoke(msg)
        elif isinstance(msg, wire.Reply):
            self.warn("orphaned reply for request_id %d ignored" % msg.request_id)
        return True

    def _dispatch_invoke(self, msg: wire.Invoke) -> None:
        # The host addresses actions as "<plugin id>.<action id>"; accept the bare id too.
        prefix = self.manifest.id + "."
        bare = msg.id[len(prefix):] if msg.id.startswith(prefix) else msg.id
        key = msg.id if msg.id in self.actions else bare
        entry = self.actions.get(key)
        if entry is None:
            self._send(wire.error_msg(msg.request_id,
                                      PluginError.schema("unknown action: %s" % msg.id)))
            return
        try:
            params_mod.validate_params(entry.params, msg.args)
        except PluginError as e:
            self._send(wire.error_msg(msg.request_id, e))
            return
        try:
            result = _run_with_active(self, _call_handler, entry, self, msg.args)
            effects = _as_effects(result)
        except PluginError as e:
            self._send(wire.error_msg(msg.request_id, e))
            return
        except Exception as e:  # handler bug -> E_INTERNAL, never a crash of the loop
            self._send(wire.error_msg(
                msg.request_id, PluginError.internal("action handler raised: %s" % e)))
            return
        self._send(wire.effects_msg(effects, msg.request_id))

    # -- host-mediated requests (callable from handlers) ------------------------
    def _alloc_request_id(self) -> int:
        rid = self._next_request_id
        self._next_request_id += 1
        return rid

    def _request(self, msg: dict) -> wire.Reply:
        """Send a request and pump host messages until the matching ``reply`` arrives.
        Everything else read meanwhile is queued and dispatched by the top-level loop."""
        rid = msg["request_id"]
        try:
            self._send(msg)
        except OSError as e:
            raise PluginError.unreachable("could not send request: %s" % e)
        while True:
            kind, raw = self._read_line()
            if kind == "eof":
                raise PluginError.unreachable("host closed stdin while waiting for a reply")
            if kind == "too_long":
                self.warn("skipped oversized line while waiting for a reply")
                continue
            line = raw.decode("utf-8", "replace").strip()
            if not line:
                continue
            try:
                m = wire.parse_host_msg(line)
            except ValueError as e:
                self.warn("skipped malformed line: %s" % e)
                continue
            if m is None:
                self.warn("skipped unrecognized line: %s" % line[:200])
                continue
            if isinstance(m, wire.Reply) and m.request_id == rid:
                return m
            # not the awaited reply (event/invoke/other reply): queue for the top loop
            self._queue.append(line.encode("utf-8"))

    def get_state(self, path: str) -> Any:
        """``{"op":"request","kind":"get","path":…}`` -> the value at ``path`` (null if absent)."""
        rid = self._alloc_request_id()
        reply = self._request(wire.request_msg(rid, "get", path=path))
        if reply.ok:
            return reply.value
        raise reply.error_or(PluginError.internal("get failed"))

    def set_state(self, path: str, value: Any) -> int:
        """``{"op":"request","kind":"set","path":…,"value":…}`` -> the new revision."""
        rid = self._alloc_request_id()
        reply = self._request(wire.request_msg(rid, "set", path=path, value=value))
        if reply.ok:
            return reply.rev or 0
        raise reply.error_or(PluginError.internal("set failed"))

    def http(self, method: str, url: str) -> HttpResponse:
        """Host-mediated HTTP (S4): ``{"op":"request","kind":"net",…}``. The daemon checks
        the URL's host against the plugin's ``net:<glob>`` grant and performs the request
        itself; the plugin never opens a socket."""
        return self.http_request(method, url)

    def http_request(self, method: str, url: str, body: Any = None,
                     headers: Optional[dict] = None) -> HttpResponse:
        method = http_mod.check_request(method, url, body, headers)
        rid = self._alloc_request_id()
        reply = self._request(wire.request_msg(
            rid, "net", method=method, url=url, body=body, headers=headers))
        if not reply.ok:
            raise reply.error_or(PluginError.internal("net request failed"))
        status = reply.status or 0
        if not 0 <= status <= 65535:
            raise PluginError.internal("status out of u16 range")
        body_val = reply.body
        if isinstance(body_val, str):
            text = body_val
        elif body_val is None:
            text = ""
        else:
            import json as _json

            text = _json.dumps(body_val, separators=(",", ":"))
        http_mod.check_reply_body(text)
        return HttpResponse(status, text)


class Plugin:
    """The plugin builder: manifest + action/event handlers, then ``serve*``.

    ```python
    import ds_plugin_py as sdk

    @sdk.action("bump", params={"type": "object", "properties": {"by": {"type": "integer"}}})
    def bump(args):
        return [sdk.set("/controls/keys.0/label", str(args.get("by", 1)))]

    sdk.serve({"id": "demo", "name": "Demo", "version": "0.1.0",
               "capabilities": ["state.write"], "actions": [bump]})
    ```
    """

    def __init__(self, manifest: Manifest):
        self.manifest = manifest
        self.actions: dict = {}
        self.event_handler: Optional[Callable] = None

    def action(self, id: str, params: Any = None, handler: Optional[Callable] = None,
               *, name: Optional[str] = None, description: Optional[str] = None,
               typed: Any = None) -> "Plugin":
        """Register an action. Usable as ``plugin.action("bump", params, fn)`` or as a
        decorator ``@plugin.action("bump", params={...})``. The action is declared in the
        manifest (wire order = registration order), like the Rust SDK."""
        def declare(fn: Callable) -> Callable:
            entry = ActionEntry(params, fn, typed=typed, name=name, description=description)
            entry.id = id
            self.actions[id] = entry
            fn.__ds_action__ = entry.def_dict()
            declared = self.manifest.data.setdefault("actions", [])
            if not any(isinstance(a, dict) and a.get("id") == id for a in declared):
                declared.append(entry.def_dict())
            return fn

        if handler is not None:
            declare(handler)
            return self
        return declare

    def on_event(self, handler: Callable) -> "Plugin":
        """Register the event handler: ``fn(sdk, event) -> effects``."""
        self.event_handler = handler
        return self

    def serve_io(self, read_chunk: ReadChunk, write: WriteBytes) -> None:
        """Speak the protocol over any chunk reader / byte writer (tests, embedding)."""
        self.manifest.validate()
        sdk = Sdk(read_chunk, write, self.manifest, self.actions, self.event_handler)
        sdk.run()

    def serve_stdio(self) -> None:
        """Speak the protocol on stdin/stdout (the real transport: newline-JSON)."""
        def read_chunk(n: int) -> bytes:
            try:
                return os.read(0, n)
            except InterruptedError:
                return b""

        def write(b: bytes) -> None:
            os.write(1, b)

        self.serve_io(read_chunk, write)


def serve_stdio(plugin: Plugin) -> None:
    plugin.serve_stdio()
