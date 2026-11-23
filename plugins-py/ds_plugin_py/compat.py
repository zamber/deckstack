"""hubdeck compatibility wrapper (D09, ``docs/contracts/plugins.md`` §6).

Runs an existing hubdeck plugin class — the ``hubdeck/.../plugins/base.py:112-272``
interface: ``actions`` / ``states`` / ``values`` dicts and the ``invoke`` / ``adjust`` /
``set`` / ``watch`` / ``watch_states`` methods — in this dedicated child process and
translates it to the ds wire protocol. **The daemon never imports old code**: the old
class is instantiated only here, in the plugin's own process, and talks to the host
exclusively through the SDK.

Translation:

- manifest generated from the class: one action per ``actions`` entry (``params`` is the
  entry's JSON Schema, or the pydantic arg model's ``model_json_schema()`` when pydantic
  is installed), plus one ``set.<name>`` action per ``states``/``values`` entry that
  forwards to the old ``set(name, value)``;
- ``invoke(name, arg)`` / ``adjust(name, arg)`` -> the action handler; effects produced
  through the ctx (below) or returned as a list are sent as the ``effects`` reply;
- ``set(name, value)`` -> a ``set`` effect on ``/plugins/<id>/state/<name>``;
- ``watch`` / ``watch_states`` -> the host events listed in the manifest are forwarded
  to them (the wrapper subscribes to every event kind by default);
- network through ``ctx.http(...)`` -> the host-mediated ``sdk.http`` (S4).

The ctx handed to the old plugin captures the old engine calls:

- ``ctx.set_state(name_or_path, value)`` -> ``set`` effect (``name`` is prefixed to
  ``/plugins/<id>/state/``; absolute paths pass through and the host enforces the
  writable subset ``/controls/…`` + ``/plugins/<id>/…``);
- ``ctx.emit(topic, payload)`` -> ``emit`` effect (topic prefixed to ``plugin/<id>/``);
- ``ctx.log(level, message)`` -> ``log`` effect;
- ``ctx.get_state(path)`` / ``ctx.http(method, url)`` -> host-mediated requests.

**Guarantees** (contract §6): process isolation, lifecycle, wire contract, resource
limits, host-mediated network. **Intentionally NOT supported**: engine internals
(``ctx.deck`` depth — OPEN C1), in-process state sharing between plugins, timing
semantics of the old engine, pydantic arg models when pydantic is not installed (falls
back to a permissive ``{"type":"object"}`` schema), and the engine-executed plugin kinds
(``page``/``preset``/``flow``/``style``). Old-app plugins are ported manually,
ticket-driven, post-alpha (``docs/70-porting-notes.md``) — nothing is auto-imported.

Typical plugin dir (``~/.local/share/deckstack/plugins/<id>/``)::

    manifest.json   # written from ds_plugin_py.compat.manifest_for(MyPlugin) + command
    plugin.py       # shim: from old_plugin import MyPlugin; compat.serve_plugin(MyPlugin)
    old_plugin.py   # the unmodified hubdeck-style class
"""

from __future__ import annotations

import inspect
import re
from typing import Any, Optional

from . import on_event as _on_event_decorator
from . import serve as _serve
from .effects import emit as _emit_effect
from .effects import log as _log_effect
from .effects import set as _set_effect
from .manifest import EVENT_KINDS
from .session import ActionEntry

__all__ = ["CompatCtx", "manifest_for", "serve_plugin", "action_params"]


def _slug(text: str) -> str:
    s = re.sub(r"[^A-Za-z0-9._-]+", "-", text).strip("-.")
    return s[:80] or "plugin"


def action_params(spec: Any) -> dict:
    """JSON Schema for one old-style action spec: a pydantic model (``model_json_schema``
    when pydantic is installed), a dict schema as-is, a callable/None -> permissive."""
    if spec is None:
        return {"type": "object"}
    if isinstance(spec, dict):
        return spec
    model_json_schema = getattr(spec, "model_json_schema", None)
    if callable(model_json_schema):
        try:
            schema = model_json_schema()
            if isinstance(schema, dict):
                return schema
        except Exception:
            pass
    return {"type": "object"}


class CompatCtx:
    """The ctx object handed to an old hubdeck plugin; captures engine calls as effects."""

    def __init__(self, sdk, plugin_id: str):
        self._sdk = sdk
        self.plugin_id = plugin_id
        self.effects: list = []

    # -- state ---------------------------------------------------------------
    def state_path(self, name_or_path: str) -> str:
        if name_or_path.startswith("/"):
            return name_or_path
        return "/plugins/%s/state/%s" % (self.plugin_id, name_or_path)

    def set_state(self, name_or_path: str, value: Any) -> None:
        self.effects.append(_set_effect(self.state_path(name_or_path), value))

    def get_state(self, path: str) -> Any:
        return self._sdk.get_state(path)

    # -- bus -------------------------------------------------------------------
    def emit_topic(self, topic: str) -> str:
        prefix = "plugin/%s/" % self.plugin_id
        if topic.startswith(prefix):
            return topic
        if topic.startswith("/"):
            topic = topic.lstrip("/")
        return prefix + topic

    def emit(self, topic: str, payload: Any = None) -> None:
        self.effects.append(_emit_effect(self.emit_topic(topic), payload))

    # -- log / net ---------------------------------------------------------------
    def log(self, level: str, message: str) -> None:
        self.effects.append(_log_effect(level, message))

    def http(self, method: str, url: str):
        return self._sdk.http(method, url)


def _class_dicts(plugin_cls: type) -> tuple:
    actions = getattr(plugin_cls, "actions", None) or {}
    states = getattr(plugin_cls, "states", None) or {}
    values = getattr(plugin_cls, "values", None) or {}
    return actions, states, values


def manifest_for(plugin_cls: type, *, id: Optional[str] = None, name: Optional[str] = None,
                 version: str = "0.1.0", capabilities: Optional[list] = None,
                 events: Optional[list] = None, description: Optional[str] = None,
                 command: Optional[list] = None, fs_roots: Optional[list] = None) -> dict:
    """Build the ds manifest for an old hubdeck plugin class (write it as the plugin dir's
    ``manifest.json``, with a ``command`` added)."""
    actions, states, values = _class_dicts(plugin_cls)
    action_defs = []
    for aname, spec in actions.items():
        d = {"id": str(aname), "params": action_params(spec)}
        if isinstance(spec, dict) and isinstance(spec.get("name"), str):
            d["name"] = spec["name"]
        action_defs.append(d)
    for sname in list(states) + list(values):
        action_defs.append({
            "id": "set.%s" % sname,
            "name": "set state %s" % sname,
            "params": {"type": "object", "properties": {"value": {}},
                       "required": ["value"], "additionalProperties": False},
        })
    manifest = {
        "id": id or _slug(plugin_cls.__name__),
        "name": name or plugin_cls.__name__,
        "version": version,
        "capabilities": capabilities if capabilities is not None
        else ["state.read", "state.write", "emit"],
        "actions": action_defs,
        "events": events if events is not None else list(EVENT_KINDS),
    }
    if description is not None:
        manifest["description"] = description
    if command is not None:
        manifest["command"] = list(command)
    if fs_roots is not None:
        manifest["fs_roots"] = list(fs_roots)
    return manifest


def build_plugin(plugin_cls: type, manifest: Optional[dict] = None, **overrides) -> "sdk_pkg.Plugin":
    """Adapt an old hubdeck plugin class into a configured :class:`ds_plugin_py.Plugin`
    (registration only — no I/O). ``serve_plugin`` is ``build_plugin`` + ``serve``."""
    import ds_plugin_py as sdk_pkg

    base = dict(manifest) if manifest is not None else manifest_for(plugin_cls)
    base.update(overrides)
    plugin_id = base["id"]

    # the old class is instantiated here, in this process — never in the daemon
    holder: dict = {}

    def make_instance(sdk) -> tuple:
        if "inst" in holder and holder.get("sdk") is sdk:
            return holder["inst"], holder["ctx"]
        ctx = CompatCtx(sdk, plugin_id)
        try:
            sig = inspect.signature(plugin_cls.__init__)
            takes = [p for p in sig.parameters.values()
                     if p.kind in (p.POSITIONAL_ONLY, p.POSITIONAL_OR_KEYWORD)]
            inst = plugin_cls(ctx) if len(takes) >= 2 else plugin_cls()
        except TypeError:
            inst = plugin_cls()
        try:
            inst.ctx = ctx
        except Exception:
            pass
        holder["sdk"] = sdk
        holder["ctx"] = ctx
        holder["inst"] = inst
        return inst, ctx

    actions, states, values = _class_dicts(plugin_cls)

    plugin = sdk_pkg.Plugin(sdk_pkg.Manifest(base))
    for aname, spec in actions.items():
        aname = str(aname)

        def make_handler(name, spec):
            def handler(sdk, args):
                inst, ctx = make_instance(sdk)
                invoke = getattr(inst, "invoke", None)
                if callable(invoke):
                    result = invoke(name, args)
                elif callable(spec):
                    result = spec(args)
                elif name in (values or {}) or name.startswith("value:"):
                    adjust = getattr(inst, "adjust", None)
                    result = adjust(name, args) if callable(adjust) else None
                else:
                    raise sdk_pkg.PluginError.schema(
                        "old plugin has no invoke for action %r" % name)
                effects = list(ctx.effects)
                if isinstance(result, list):
                    effects.extend(result)
                return effects
            return handler

        entry = ActionEntry(action_params(spec), make_handler(aname, spec))
        entry.id = aname
        plugin.actions[aname] = entry

    for sname in list(states) + list(values):
        sname = str(sname)

        def make_set_handler(name):
            def handler(sdk, args):
                inst, ctx = make_instance(sdk)
                value = args.get("value") if isinstance(args, dict) else None
                setter = getattr(inst, "set", None)
                if callable(setter):
                    setter(name, value)
                ctx.set_state(name, value)
                return list(ctx.effects)
            return handler

        entry = ActionEntry({"type": "object", "properties": {"value": {}},
                             "required": ["value"], "additionalProperties": False},
                            make_set_handler(sname))
        entry.id = "set.%s" % sname
        plugin.actions[entry.id] = entry

    def _compat_event(sdk, event):
        inst, ctx = make_instance(sdk)
        info = {"event": event.event, "topic": event.control_or_topic,
                "payload": event.state_or_payload}
        for meth in ("watch", "watch_states"):
            fn = getattr(inst, meth, None)
            if callable(fn):
                try:
                    result = fn(info)
                except TypeError:
                    result = fn(event)
                if isinstance(result, list):
                    ctx.effects.extend(result)
        return list(ctx.effects)

    plugin.event_handler = _compat_event
    return plugin


def serve_plugin(plugin_cls: type, manifest: Optional[dict] = None, **overrides) -> None:
    """Adapt an old hubdeck plugin class and serve the ds wire protocol in THIS process
    (the plugin child). ``manifest`` defaults to :func:`manifest_for` of the class, with
    ``overrides`` applied (``id=``, ``capabilities=``, ``command=``, …). Exits 0 on clean
    shutdown; fatal protocol errors go to stderr with the documented exit code."""
    import ds_plugin_py as sdk_pkg

    plugin = build_plugin(plugin_cls, manifest, **overrides)
    sdk_pkg._serve_plugin_exit(plugin)
