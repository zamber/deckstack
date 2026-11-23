"""``ds_plugin_py`` — the Python SDK for deckstack plugins (``docs/contracts/plugins.md`` §8).

One OS process per plugin; the wire protocol is **newline-JSON over stdin/stdout**:

.. code-block:: text

    host->plugin  {"op":"hello","protocol":1,"plugin_token":"…","caps":[…],"roots":{…},"limits":{…}}
    plugin->host  {"op":"register","id":"demo","manifest":{…}}
    host->plugin  {"op":"event","event":"keyDown","topic":"deck/control/keys.0/down","payload":{…}}
    host->plugin  {"op":"invoke","id":"demo.bump","args":{"by":1},"request_id":7}
    plugin->host  {"op":"effects","request_id":7,"effects":[{"effect":"set","path":"…","value":…}]}
    plugin->host  {"op":"request","request_id":8,"kind":"get","path":"/controls/keys.0"}
    plugin->host  {"op":"request","request_id":9,"kind":"net","method":"GET","url":"http://lights.lan/api/…"}
    host->plugin  {"op":"reply","request_id":9,"ok":true,"status":200,"body":"…"}

Minimal plugin (mirrors ``docs/contracts/plugins.md`` §7 — this exact shape runs):

.. code-block:: python

    import ds_plugin_py as sdk
    count = {"n": 0}

    @sdk.action("bump", params={"type": "object", "properties": {"by": {"type": "integer", "default": 1}}})
    def bump(args):
        count["n"] += args.get("by", 1)
        return [sdk.set("/controls/keys.0/label", str(count["n"])),
                sdk.set("/controls/keys.0/image", {"kind": "color", "value": "#ff8800"})]

    @sdk.action("get_lamp")
    def get_lamp(args):
        r = sdk.http("GET", "http://lights.lan/api/lamp")   # host-mediated (S4)
        return [sdk.set("/plugins/demo/state/last", r.body)]

    sdk.serve({"id": "demo", "name": "Demo counter", "version": "0.1.0",
               "capabilities": ["state.write", "net:lights.lan"], "actions": [bump, get_lamp]})

- :func:`serve` runs the protocol loop on stdin/stdout; :class:`Plugin` is the explicit
  builder (``serve_io`` for tests/embedding).
- :func:`action` / :func:`on_event` register handlers; ``params`` JSON Schema is
  validated before dispatch (subset check; the host is the enforcing validator).
- :func:`set` / :func:`emit` / :func:`log` / :func:`sleep` build effects;
  :func:`get_state` / :func:`set_state` / :func:`http` are the host-mediated requests
  callable from handlers (module-level, they use the active session).
- Errors map to the stable codes of ``schemas/ipc-message.json`` (:class:`ErrorCode`).

Stdlib-only. stdout carries only protocol lines; all diagnostics go to stderr.
Same wire protocol as the Rust SDK (``crates/ds-plugin-sdk``, DS-051).
"""

from __future__ import annotations

import sys
from typing import Any, Callable, Optional

from . import wire
from .effects import emit, log, set, sleep
from .errors import ErrorCode, PluginError, ServeError
from .http import (
    ALLOWED_METHODS,
    MAX_NET_BODY_BYTES,
    MAX_NET_HEADERS,
    MAX_NET_REPLY_CHARS,
    MAX_NET_URL_CHARS,
    HttpResponse,
)
from .manifest import EVENT_KINDS, Manifest, valid_capability, valid_id, valid_version
from .params import validate_params
from .session import ActionEntry, HostEvent, Plugin, Sdk

__version__ = "0.1.0"

__all__ = [
    "action", "on_event", "serve", "build_manifest",
    "set", "emit", "log", "sleep",
    "get_state", "set_state", "http", "http_request",
    "Plugin", "Sdk", "HostEvent", "Manifest",
    "PluginError", "ServeError", "ErrorCode", "HttpResponse",
    "PROTOCOL_VERSION", "MAX_LINE_BYTES", "EVENT_KINDS",
    "ALLOWED_METHODS", "MAX_NET_URL_CHARS", "MAX_NET_BODY_BYTES",
    "MAX_NET_HEADERS", "MAX_NET_REPLY_CHARS",
    "validate_params", "valid_id", "valid_version", "valid_capability",
    "__version__",
]

PROTOCOL_VERSION = wire.PROTOCOL_VERSION
MAX_LINE_BYTES = wire.MAX_LINE_BYTES

# ---------------------------------------------------------------------------
# module-level registry filled by the @action / @on_event decorators and consumed
# by serve(manifest) — this is what makes the §7 ~40-line example work.
# ---------------------------------------------------------------------------

_REGISTRY: dict = {}
_EVENT_HANDLER: Optional[Callable] = None
_ACTIVE: Optional[Sdk] = None  # the session currently dispatching a handler


def action(id: str, params: Any = None, *, name: Optional[str] = None,
           description: Optional[str] = None, typed: Any = None) -> Callable:
    """Decorator: register an action handler.

    ``@sdk.action("bump", params={...})`` — ``params`` is the JSON Schema declared in the
    manifest; args are validated against it before the handler runs. The handler is
    called as ``fn(args)`` or ``fn(sdk, args)``; with ``typed=SomeClass`` the args dict is
    first converted into that class (pydantic model / dataclass / callable), a mismatch
    answers ``E_SCHEMA``.
    """
    def declare(fn: Callable) -> Callable:
        entry = ActionEntry(params, fn, typed=typed, name=name, description=description)
        entry.id = id
        _REGISTRY[id] = entry
        fn.__ds_action__ = entry.def_dict()
        return fn

    return declare


def on_event(fn: Callable) -> Callable:
    """Decorator: register the event handler ``fn(sdk, event) -> effects``."""
    global _EVENT_HANDLER
    _EVENT_HANDLER = fn
    return fn


def build_manifest(manifest: Any) -> dict:
    """Normalize a manifest dict (actions may be @action-decorated callables) into the
    wire-ready dict that ``register`` sends — use it to write the on-disk
    ``manifest.json`` so the two compare equal at the handshake."""
    data = manifest.data if isinstance(manifest, Manifest) else dict(manifest)
    actions = data.get("actions") or []
    norm_actions = []
    for a in actions:
        if isinstance(a, dict):
            norm_actions.append(a)
        else:
            entry = _REGISTRY.get(getattr(a, "__ds_action__", {}).get("id", "")) \
                if hasattr(a, "__ds_action__") else None
            if entry is not None:
                norm_actions.append(entry.def_dict())
            else:
                d = getattr(a, "__ds_action__", None)
                if d is None:
                    raise ServeError(ServeError.MANIFEST,
                                     "action entry must be a dict or a @action function")
                norm_actions.append(dict(d))
    for aid, entry in _REGISTRY.items():
        if aid not in {a.get("id") for a in norm_actions if isinstance(a, dict)}:
            norm_actions.append(entry.def_dict())
    data["actions"] = norm_actions
    return Manifest(data).to_dict()


def _serve_plugin_exit(plugin: Plugin) -> None:
    """Run a built plugin on stdio and exit with the documented code."""
    try:
        plugin.serve_stdio()
    except ServeError as e:
        sys.stderr.write("ds-plugin-py: %s\n" % e.message)
        sys.stderr.flush()
        sys.exit(e.exit_code())
    sys.exit(0)


def serve(manifest: Any, *, stdin=None, stdout=None) -> None:
    """Serve a manifest over stdin/stdout (the real transport).

    ``manifest`` is a dict per ``schemas/plugin-manifest.json``; its ``actions`` list may
    contain @action-decorated functions (as in the §7 example) or plain
    ``{"id":…, "params":…}`` dicts (matched to registered handlers by id). Actions
    registered with @action but not listed are appended. Exits 0 on clean shutdown
    (EOF or ``shutdown``/``stop`` op); fatal protocol errors go to stderr with the
    documented exit code.
    """
    global _ACTIVE
    m = manifest if isinstance(manifest, Manifest) else Manifest(dict(manifest))
    m.validate()
    plugin = Plugin(m)
    for entry in _REGISTRY.values():
        plugin.actions[entry.id] = entry
    # manifest action dicts without a registered handler are still declared; give them
    # a handler that answers E_SCHEMA (the host validates args before invoke anyway)
    for a in m.actions:
        if isinstance(a, dict) and a.get("id") not in plugin.actions:
            entry = ActionEntry(a.get("params"), lambda args: [], name=a.get("name"),
                                description=a.get("description"))
            entry.id = a["id"]
            plugin.actions[entry.id] = entry
    if _EVENT_HANDLER is not None:
        plugin.event_handler = _EVENT_HANDLER
    if stdin is None and stdout is None:
        _serve_plugin_exit(plugin)
    else:
        import io as _io

        inp = stdin if stdin is not None else sys.stdin
        out = stdout if stdout is not None else sys.stdout
        r = inp.buffer if hasattr(inp, "buffer") else inp
        w = out.buffer if hasattr(out, "buffer") else out

        def read_chunk(n: int) -> bytes:
            return r.read(n) or b""

        def write(b: bytes) -> None:
            w.write(b)
            w.flush()

        plugin.serve_io(read_chunk, write)


# ---------------------------------------------------------------------------
# module-level host-mediated helpers: they delegate to the active session, which is
# what makes `sdk.http(...)` inside a handler work (the §7 example style).
# ---------------------------------------------------------------------------

def _active() -> Sdk:
    if _ACTIVE is None:
        raise PluginError.unreachable(
            "no active plugin session (call this from inside an action/event handler)")
    return _ACTIVE


def get_state(path: str) -> Any:
    """Host-mediated state read: ``{"op":"request","kind":"get","path":…}`` -> value."""
    return _active().get_state(path)


def set_state(path: str, value: Any) -> int:
    """Host-mediated state write: ``{"op":"request","kind":"set",…}`` -> new rev."""
    return _active().set_state(path, value)


def http(method: str, url: str) -> HttpResponse:
    """Host-mediated HTTP (S4): the daemon performs the request; the plugin never opens
    a socket. Bounded by ``MAX_NET_URL_CHARS`` etc."""
    return _active().http(method, url)


def http_request(method: str, url: str, body: Any = None,
                 headers: Optional[dict] = None) -> HttpResponse:
    """Like :func:`http` but with an optional JSON body and headers (still bounded)."""
    return _active().http_request(method, url, body=body, headers=headers)
