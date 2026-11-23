"""The plugin manifest, mirroring ``schemas/plugin-manifest.json`` (normative).

The SDK serializes the manifest into the ``register`` message and validates it against
the schema's rules (patterns, lengths, bounds, no unknown keys) *before* the handshake,
so a bad manifest fails fast in the plugin process instead of being killed by the host
with ``E_SCHEMA``.

Mirror of ``crates/ds-plugin-sdk/src/manifest.rs`` (DS-051).
"""

from __future__ import annotations

import re
import sys
from typing import Any, Optional

from .errors import ServeError

_ID_RE = re.compile(r"^[A-Za-z0-9._-]{1,80}$")
_VERSION_RE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+$")
_NET_GLOB_RE = re.compile(r"^[A-Za-z0-9.*-]{1,200}$")
_FS_ROOT_RE = re.compile(r"^/[A-Za-z0-9_./-]{0,200}$")

#: Events the plugin may subscribe to (schema ``events`` enum, upstream event names).
EVENT_KINDS = (
    "keyDown",
    "keyUp",
    "dialRotate",
    "slide",
    "state",
    "willAppear",
    "willDisappear",
    "focus",
)

#: Capability prefixes the schema allows.
_FIXED_CAPS = ("state.read", "state.write", "device.write", "exec", "emit")

_RESOURCE_RANGES = {
    "cpu_s": (1, 3600),
    "nproc": (1, 4096),
    "nofile": (8, 4096),
    "as_mb": (64, 65536),
}


def valid_id(id: str) -> bool:
    return bool(_ID_RE.match(id))


def valid_version(v: str) -> bool:
    return bool(_VERSION_RE.match(v))


def valid_net_glob(glob: str) -> bool:
    return bool(_NET_GLOB_RE.match(glob))


def valid_capability(cap: str) -> bool:
    if cap in _FIXED_CAPS:
        return True
    if cap.startswith("net:"):
        return valid_net_glob(cap[4:])
    if cap.startswith("fs:"):
        return bool(_FS_ROOT_RE.match(cap[3:]))
    return False


def default_command() -> list:
    """The argv the host used to start this process (mirrors ``current_exe`` in Rust)."""
    return [sys.executable or "python3"]


class Manifest:
    """The manifest of ``schemas/plugin-manifest.json``.

    Wraps a plain dict; :meth:`to_dict` returns the wire-ready ordered dict (only the
    schema's keys, optional empty fields omitted — matching the Rust SDK's
    ``skip_serializing_if``).
    """

    def __init__(self, data: dict):
        if not isinstance(data, dict):
            raise ServeError(ServeError.MANIFEST, "manifest must be a JSON object")
        self.data = dict(data)

    # -- accessors -----------------------------------------------------------
    @property
    def id(self) -> str:
        return self.data.get("id", "")

    @property
    def actions(self) -> list:
        return self.data.get("actions") or []

    @property
    def capabilities(self) -> list:
        return self.data.get("capabilities") or []

    def has_cap(self, cap: str) -> bool:
        return cap in self.capabilities

    # -- wire ----------------------------------------------------------------
    def to_dict(self) -> dict:
        d = self.data
        out: dict = {"id": d.get("id", ""), "name": d.get("name", ""),
                     "version": d.get("version", "")}
        if d.get("description") is not None:
            out["description"] = d["description"]
        command = d.get("command") or default_command()
        out["command"] = list(command)
        actions = d.get("actions") or []
        if actions:
            out["actions"] = [self._action_def(a) for a in actions]
        caps = d.get("capabilities") or []
        out["capabilities"] = list(caps)
        events = d.get("events") or []
        if events:
            out["events"] = list(events)
        if d.get("resources") is not None:
            out["resources"] = dict(d["resources"])
        fs_roots = d.get("fs_roots") or []
        if fs_roots:
            out["fs_roots"] = list(fs_roots)
        return out

    @staticmethod
    def _action_def(a: Any) -> dict:
        if isinstance(a, dict):
            out = {"id": a.get("id", "")}
            if a.get("name") is not None:
                out["name"] = a["name"]
            if a.get("description") is not None:
                out["description"] = a["description"]
            out["params"] = a.get("params") if a.get("params") is not None else {"type": "object"}
            return out
        # a callable registered via @action — the decorator attaches its def
        def_ = getattr(a, "__ds_action__", None)
        if def_ is not None:
            return dict(def_)
        raise ServeError(ServeError.MANIFEST, "action entry must be a dict or a @action function")

    # -- validation ------------------------------------------------------------
    def validate(self) -> None:
        def bad(m: str) -> ServeError:
            return ServeError(ServeError.MANIFEST, m)

        d = self.data
        unknown = set(d) - {
            "id", "name", "version", "description", "command", "actions",
            "capabilities", "events", "resources", "fs_roots",
        }
        if unknown:
            raise bad("unknown manifest keys: %s (additionalProperties: false)" % sorted(unknown))
        if not isinstance(d.get("id"), str) or not valid_id(d["id"]):
            raise bad("id %r must match ^[A-Za-z0-9._-]{1,80}$" % (d.get("id"),))
        name = d.get("name")
        if not isinstance(name, str) or not 1 <= len(name) <= 120:
            raise bad("name must be 1..=120 chars")
        if not isinstance(d.get("version"), str) or not valid_version(d["version"]):
            raise bad("version %r must match ^[0-9]+\\.[0-9]+\\.[0-9]+$" % (d.get("version"),))
        desc = d.get("description")
        if desc is not None and (not isinstance(desc, str) or len(desc) > 500):
            raise bad("description must be <= 500 chars")
        command = d.get("command") or default_command()
        if not isinstance(command, list) or not 1 <= len(command) <= 16 or not all(
            isinstance(c, str) and c for c in command
        ):
            raise bad("command must be 1..=16 non-empty argv entries")
        actions = d.get("actions") or []
        if not isinstance(actions, list):
            raise bad("actions must be an array")
        seen = set()
        for a in actions:
            if isinstance(a, dict):
                aid = a.get("id")
                params = a.get("params")
                aname = a.get("name")
                adesc = a.get("description")
            else:
                def_ = getattr(a, "__ds_action__", None)
                if def_ is None:
                    raise bad("action entry must be a dict or a @action function")
                aid, params, aname, adesc = def_["id"], def_["params"], def_.get("name"), def_.get("description")
            if not isinstance(aid, str) or not valid_id(aid):
                raise bad("action id %r is invalid" % (aid,))
            if aid in seen:
                raise bad("duplicate action id %r" % (aid,))
            seen.add(aid)
            if aname is not None and len(aname) > 120:
                raise bad("action name must be <= 120 chars")
            if adesc is not None and len(adesc) > 500:
                raise bad("action description must be <= 500 chars")
            if not isinstance(params, dict):
                raise bad("action %r params must be a JSON object" % (aid,))
        caps = d.get("capabilities")
        if not isinstance(caps, list) or not all(isinstance(c, str) for c in caps):
            raise bad("capabilities must be an array of strings")
        for c in caps:
            if not valid_capability(c):
                raise bad("bad capability %r" % (c,))
        events = d.get("events") or []
        if not isinstance(events, list) or any(e not in EVENT_KINDS for e in events):
            raise bad("events must be a subset of %s" % (list(EVENT_KINDS),))
        res = d.get("resources")
        if res is not None:
            if not isinstance(res, dict) or set(res) - set(_RESOURCE_RANGES):
                raise bad("resources must be an object with keys in %s" % sorted(_RESOURCE_RANGES))
            for k, v in res.items():
                lo, hi = _RESOURCE_RANGES[k]
                if not isinstance(v, int) or isinstance(v, bool) or not lo <= v <= hi:
                    raise bad("resources.%s must be %d..=%d" % (k, lo, hi))
        fs_roots = d.get("fs_roots") or []
        if not isinstance(fs_roots, list) or any(
            not isinstance(f, str) or not f.startswith("/") for f in fs_roots
        ):
            raise bad("fs_roots entries must be absolute paths")
