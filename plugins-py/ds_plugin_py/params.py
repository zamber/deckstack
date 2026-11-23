"""JSON-Schema subset validation of action arguments against the declared ``params``.

The host validates ``args`` against the full draft 2020-12 schema before every invoke;
this module is the SDK-side early check so a bad payload fails inside the handler
boundary with ``E_SCHEMA`` instead of a crash. Supported keywords: ``type``,
``properties``, ``required``, ``additionalProperties`` (bool), ``enum``, ``const``,
``items``, ``minItems``/``maxItems``, ``minLength``/``maxLength``,
``minimum``/``maximum``. ``pattern`` is NOT evaluated (stdlib has ``re`` but the host
remains the enforcing validator, which is documented in the README).

Mirror of ``crates/ds-plugin-sdk/src/params.rs`` (DS-051).
"""

from __future__ import annotations

from typing import Any

from .errors import PluginError


def _type_matches(schema_type: str, v: Any) -> bool:
    if schema_type == "object":
        return isinstance(v, dict)
    if schema_type == "array":
        return isinstance(v, list)
    if schema_type == "string":
        return isinstance(v, str)
    if schema_type == "boolean":
        return isinstance(v, bool)
    if schema_type == "null":
        return v is None
    if schema_type == "number":
        return isinstance(v, (int, float)) and not isinstance(v, bool)
    if schema_type == "integer":
        return isinstance(v, int) and not isinstance(v, bool)
    return True  # unknown type keyword: defer to the host


def _fail(path: str, msg: str) -> PluginError:
    return PluginError.schema("args at %s: %s" % (path, msg))


def _validate(schema: Any, v: Any, path: str) -> None:
    if not isinstance(schema, dict):
        return
    t = schema.get("type")
    if t is not None:
        types = [t] if isinstance(t, str) else t if isinstance(t, list) else []
        if types and not any(
            isinstance(x, str) and _type_matches(x, v) for x in types
        ):
            raise _fail(path, "expected type %s, got %s" % (t, _short(v)))
    if "enum" in schema and isinstance(schema["enum"], list):
        if v not in schema["enum"]:
            raise _fail(path, "%s is not one of %s" % (_short(v), schema["enum"]))
    if "const" in schema and schema["const"] != v:
        raise _fail(path, "%s != const %s" % (_short(v), _short(schema["const"])))
    if isinstance(schema.get("minimum"), (int, float)) and not isinstance(v, bool):
        if not isinstance(v, (int, float)) or v < schema["minimum"]:
            raise _fail(path, "%s < minimum %s" % (_short(v), schema["minimum"]))
    if isinstance(schema.get("maximum"), (int, float)) and not isinstance(v, bool):
        if not isinstance(v, (int, float)) or v > schema["maximum"]:
            raise _fail(path, "%s > maximum %s" % (_short(v), schema["maximum"]))
    if isinstance(schema.get("minLength"), int) and isinstance(v, str):
        if len(v) < schema["minLength"]:
            raise _fail(path, "string shorter than minLength %d" % schema["minLength"])
    if isinstance(schema.get("maxLength"), int) and isinstance(v, str):
        if len(v) > schema["maxLength"]:
            raise _fail(path, "string longer than maxLength %d" % schema["maxLength"])
    if isinstance(v, list):
        if isinstance(schema.get("minItems"), int) and len(v) < schema["minItems"]:
            raise _fail(path, "array shorter than minItems %d" % schema["minItems"])
        if isinstance(schema.get("maxItems"), int) and len(v) > schema["maxItems"]:
            raise _fail(path, "array longer than maxItems %d" % schema["maxItems"])
        if "items" in schema:
            for i, item in enumerate(v):
                _validate(schema["items"], item, "%s[%d]" % (path, i))
    if isinstance(v, dict):
        props = schema.get("properties")
        if isinstance(props, dict):
            for key, sub in props.items():
                if key in v:
                    _validate(sub, v[key], "%s.%s" % (path, key))
        req = schema.get("required")
        if isinstance(req, list):
            for r in req:
                if isinstance(r, str) and r not in v:
                    raise _fail(path, "missing required property %r" % r)
        if schema.get("additionalProperties") is False and isinstance(props, dict):
            for key in v:
                if key not in props:
                    raise _fail(path, "additional property %r not allowed" % key)


def _short(v: Any) -> str:
    s = repr(v)
    return s if len(s) <= 120 else s[:117] + "..."


def validate_params(schema: Any, args: Any) -> None:
    """Validate ``args`` against ``schema``; raises ``E_SCHEMA`` on the first violation."""
    _validate(schema, args, "$")
