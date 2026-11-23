"""Effect builders (``docs/contracts/plugins.md`` §8: ``set``/``emit``/``log``/``sleep``).

An action/event handler returns a list of effects; the serve loop sends them as the
``effects`` array of an ``{"op":"effects",…}`` message. The host validates every effect
against the state schema and the plugin's capabilities before touching state.

Mirror of ``crates/ds-plugin-sdk/src/effect.rs`` (DS-051). Field order matches the
contract example ``{"effect":"set","path":"/controls/keys.0/label","value":"1"}``.
"""

from __future__ import annotations

from typing import Any

_EFFECT_FIELDS = ("path", "value", "topic", "payload", "level", "message", "ms")


def _effect(kind: str, **fields: Any) -> dict:
    eff: dict = {"effect": kind}
    for name in _EFFECT_FIELDS:
        if name in fields and fields[name] is not None:
            eff[name] = fields[name]
    return eff


def set(path: str, value: Any) -> dict:
    """``{"effect":"set","path":…,"value":…}`` — set a state path (capability ``state.write``)."""
    return _effect("set", path=path, value=value)


def emit(topic: str, payload: Any = None) -> dict:
    """``{"effect":"emit","topic":…,"payload":…}`` — emit a bus event (capability ``emit``;
    the host only accepts topics under ``plugin/<id>/…``)."""
    return _effect("emit", topic=topic, payload=payload)


def log(level: str, message: str) -> dict:
    """``{"effect":"log","level":…,"message":…}`` — a log line in the daemon log ring."""
    return _effect("log", level=level, message=message)


def sleep(ms: int) -> dict:
    """``{"effect":"sleep","ms":…}`` — pause the action chain for ``ms``."""
    return _effect("sleep", ms=ms)
