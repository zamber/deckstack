"""Wire messages of the plugin protocol (``docs/contracts/plugins.md`` §1).

Transport: one JSON object per line (newline-JSON) over stdin/stdout. Plugin->host
messages are built as ordered dicts serialized compact (``separators=(",", ":")``) in
the documented field order; host->plugin messages are parsed leniently so a malformed
or unknown message can be logged to stderr and skipped — a protocol violation never
panics the plugin (or the host).

Mirror of ``crates/ds-plugin-sdk/src/wire.rs`` (DS-051).
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any, Optional

from .errors import PluginError

#: Wire protocol version spoken by this SDK (``protocol: 1`` in ``hello``).
PROTOCOL_VERSION = 1

#: Max bytes of one inbound line (mirrors ``ds-ipc`` ``MAX_LINE_BYTES`` = 1 MiB).
MAX_LINE_BYTES = 1024 * 1024


def dumps(obj: Any) -> str:
    """Serialize one wire message compactly (no spaces), like serde_json."""
    return json.dumps(obj, separators=(",", ":"), ensure_ascii=False)


def send_line(write, obj: Any) -> None:
    write((dumps(obj) + "\n").encode("utf-8"))


# ---------------------------------------------------------------------------
# plugin -> host
# ---------------------------------------------------------------------------

def register_msg(plugin_id: str, manifest: dict) -> dict:
    """``{"op":"register","id":"demo","manifest":{…}}``"""
    return {"op": "register", "id": plugin_id, "manifest": manifest}


def effects_msg(effects: list, request_id: Optional[int] = None) -> dict:
    """``{"op":"effects","request_id":7,"effects":[…]}`` — ``request_id`` only when
    replying to an ``invoke``/correlated ``event``."""
    msg: dict = {"op": "effects"}
    if request_id is not None:
        msg["request_id"] = request_id
    msg["effects"] = effects
    return msg


def error_msg(request_id: int, error: PluginError) -> dict:
    """``{"op":"error","request_id":7,"error":{"code":"E_…","message":"…"}}``"""
    return {"op": "error", "request_id": request_id, "error": error.to_dict()}


_REQUEST_FIELDS = ("path", "value", "method", "url", "body", "headers")


def request_msg(request_id: int, kind: str, **fields: Any) -> dict:
    """``{"op":"request","request_id":N,"kind":"get","path":…}`` /
    ``{"kind":"set","path":…,"value":…}`` /
    ``{"kind":"net","method":…,"url":…,"body"?:…,"headers"?:…}``.

    ``kind`` is followed by the kind-specific fields in a fixed order, so the ``net``
    request serializes exactly like the contract example
    ``{"op":"request","request_id":9,"kind":"net","method":"GET","url":"http://…"}``.
    """
    msg: dict = {"op": "request", "request_id": request_id, "kind": kind}
    for name in _REQUEST_FIELDS:
        if name in fields and fields[name] is not None:
            msg[name] = fields[name]
    return msg


# ---------------------------------------------------------------------------
# host -> plugin (parsed leniently)
# ---------------------------------------------------------------------------

@dataclass
class Hello:
    protocol: int
    caps: list = field(default_factory=list)
    plugin_token: Optional[str] = None
    roots: Any = None
    limits: Any = None


@dataclass
class Event:
    event: str
    control: Optional[str] = None
    topic: Optional[str] = None
    state: Any = None
    payload: Any = None
    request_id: Optional[int] = None

    @property
    def control_or_topic(self) -> Optional[str]:
        return self.control if self.control is not None else self.topic

    @property
    def state_or_payload(self) -> Any:
        return self.state if self.state is not None else self.payload


@dataclass
class Invoke:
    id: str
    args: Any
    request_id: int


@dataclass
class Reply:
    request_id: int
    ok: bool
    status: Optional[int] = None
    body: Any = None
    value: Any = None
    rev: Optional[int] = None
    error: Optional[PluginError] = None

    def error_or(self, fallback: PluginError) -> PluginError:
        return self.error if self.error is not None else fallback


@dataclass
class Shutdown:
    pass


def _get_str(v: dict, key: str) -> Optional[str]:
    val = v.get(key)
    return val if isinstance(val, str) else None


def _get_u64(v: dict, key: str) -> Optional[int]:
    val = v.get(key)
    return val if isinstance(val, int) and not isinstance(val, bool) and val >= 0 else None


def parse_host_msg(line: str):
    """Parse one inbound line into a host message.

    Returns a dataclass (Hello/Event/Invoke/Reply/Shutdown), or ``None`` when the line
    is not a recognizable message (the caller logs to stderr and skips). Raises
    ``json.JSONDecodeError`` for lines that are not JSON at all — the caller treats
    that as a malformed line and skips it, never a panic.
    """
    v = json.loads(line)
    if not isinstance(v, dict):
        return None
    op = v.get("op")
    if not isinstance(op, str):
        return None
    if op == "hello":
        caps = v.get("caps")
        return Hello(
            protocol=_get_u64(v, "protocol") or 0,
            caps=[c for c in caps if isinstance(c, str)] if isinstance(caps, list) else [],
            plugin_token=_get_str(v, "plugin_token"),
            roots=v.get("roots"),
            limits=v.get("limits"),
        )
    if op == "event":
        return Event(
            event=_get_str(v, "event") or "",
            control=_get_str(v, "control"),
            topic=_get_str(v, "topic"),
            state=v.get("state"),
            payload=v.get("payload"),
            request_id=_get_u64(v, "request_id"),
        )
    if op == "invoke":
        return Invoke(
            id=_get_str(v, "id") or "",
            args=v.get("args"),
            request_id=_get_u64(v, "request_id") or 0,
        )
    if op == "reply":
        ok = v.get("ok")
        err = v.get("error")
        if err is None and isinstance(v.get("code"), str):
            # the host also answers some failures with a top-level code/message pair
            err = {"code": v.get("code"), "message": v.get("message")}
        return Reply(
            request_id=_get_u64(v, "request_id") or 0,
            ok=ok if isinstance(ok, bool) else False,
            status=_get_u64(v, "status"),
            body=v.get("body"),
            value=v.get("value"),
            rev=_get_u64(v, "rev"),
            error=PluginError.from_dict(err),
        )
    if op in ("shutdown", "stop"):
        return Shutdown()
    return None
