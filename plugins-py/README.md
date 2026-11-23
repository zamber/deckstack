# ds-plugin-py — the deckstack Python plugin SDK

Stdlib-only Python SDK for [deckstack](https://deckstack.dev) plugins, plus a compatibility
wrapper that runs an old **hubdeck** plugin class in a dedicated child process. Published to
PyPI as `ds-plugin-py` (subtree split of `plugins-py/`, see `repos.toml`); the import package
is `ds_plugin_py` (`deckstack_plugin` is an alias re-exporting the same objects).

Same wire protocol as the Rust SDK (`crates/ds-plugin-sdk`, DS-051); contract:
`docs/contracts/plugins.md`, manifest schema `schemas/plugin-manifest.json`.

- One OS process per plugin; **newline-JSON over stdin/stdout** (stderr → the daemon log /
  per-plugin ring buffer). stdout carries only protocol lines — the SDK never prints to it.
- Host handshake: `hello` → `register` (manifest must equal the on-disk `manifest.json`) →
  `event` / `invoke` → `effects` / `error`; plugin→host `request` (`get` / `set` / `net`)
  answered by `reply`.
- **Host-mediated network (S4/M5):** plugins never open sockets. `sdk.http(method, url)`
  sends `{"op":"request","kind":"net",…}` and the daemon checks the URL host against the
  plugin's `net:<glob>` grant *and* `/policy/net/allow_hosts`, then performs the request
  itself. The SDK bounds what it will ask for: URL ≤ 2048 chars, body ≤ 1 MiB, ≤ 16 headers,
  reply body ≤ 4 MiB chars, methods GET/HEAD/POST/PUT/PATCH/DELETE (violations answer
  `E_LIMIT`/`E_SCHEMA` before anything is sent).
- Effects: `sdk.set(path, value)`, `sdk.emit(topic, payload)`, `sdk.log(level, msg)`,
  `sdk.sleep(ms)`. The host validates them (writable subset `/controls/…` +
  `/plugins/<id>/…`, emit topics under `plugin/<id>/…`, ≤ 100 effects per invoke).
- Host-mediated state: `sdk.get_state(path)` → value, `sdk.set_state(path, value)` → rev.
- Errors map to the stable control-plane codes (`E_SCHEMA`, `E_CAP`, `E_LIMIT`,
  `E_UNREACHABLE`, `E_INTERNAL`, … — `schemas/ipc-message.json` `$defs.error`); a handler
  raises `sdk.PluginError(sdk.ErrorCode.E_CAP, "…")` (or any exception → `E_INTERNAL`).

## The ~40-line example (contract §7 — this exact shape runs)

```python
import ds_plugin_py as sdk
count = {"n": 0}

@sdk.action("bump", params={"type": "object", "properties": {"by": {"type": "integer", "default": 1}}})
def bump(args):
    count["n"] += args.get("by", 1)
    return [sdk.set("/controls/keys.0/label", str(count["n"])),
            sdk.set("/controls/keys.0/image", {"kind": "color", "value": "#ff8800"})]

@sdk.action("get_lamp")
def get_lamp(args):
    r = sdk.http("GET", "http://lights.lan/api/lamp")     # host-mediated, glob-checked (S4)
    return [sdk.set("/plugins/demo/state/last", r.body)]

sdk.serve({"id": "demo", "name": "Demo counter", "version": "0.1.0",
           "capabilities": ["state.write", "net:lights.lan"], "actions": [bump, get_lamp]})
```

A runnable version lives in `examples/demo_plugin.py`; `python plugins-py/smoke.py` (from the
repo root) installs it against a real `ds-vdev` + `ds-daemon` and asserts the effects land.

## API

| function | wire |
|---|---|
| `sdk.serve(manifest)` | handshake + dispatch loop on stdin/stdout; exits 0 on EOF/`shutdown` |
| `@sdk.action(id, params=…, name=…, description=…, typed=…)` | declares the action in the manifest and registers the handler (`fn(args)` or `fn(sdk, args)`; `typed=` converts args first — pydantic model / dataclass / callable — mismatch → `E_SCHEMA`) |
| `@sdk.on_event(fn)` | `fn(sdk, event)` → effects; `event.event/control/topic/state/payload/request_id` |
| `sdk.Plugin(manifest).action(...).on_event(...).serve_stdio() / .serve_io(read_chunk, write)` | explicit builder (tests, embedding) |
| `sdk.set / sdk.emit / sdk.log / sdk.sleep` | effect builders |
| `sdk.get_state(path)` / `sdk.set_state(path, value)` | `request kind:"get"` / `kind:"set"` |
| `sdk.http(method, url)` / `sdk.http_request(method, url, body=, headers=)` | `request kind:"net"` → `HttpResponse(status, body)` with `.body_json()` |
| `sdk.build_manifest(manifest)` | normalize a manifest (action callables → defs) — write this as the on-disk `manifest.json` so `register` compares equal |
| `sdk.Manifest(dict).validate()` | schema validation before the handshake (fails fast, exit 1) |
| `sdk.PluginError`, `sdk.ErrorCode`, `sdk.ServeError` | error mapping; fatal serve errors exit with the documented CLI code |

Manifest keys follow `schemas/plugin-manifest.json` exactly (`id`, `name`, `version`,
`description?`, `command`, `actions[]`, `capabilities[]`, `events[]`, `resources?`,
`fs_roots?`). `command` defaults to `[sys.executable]`. Invoke ids arrive as
`<plugin id>.<action>`; the bare action id is accepted too. Malformed/oversized (> 1 MiB)
host lines are counted (`sdk.violations`-side stderr warning) and skipped, never fatal.

## hubdeck compatibility wrapper (`ds_plugin_py.compat`, D09)

Runs an existing hubdeck plugin class — the `hubdeck/.../plugins/base.py:112-272` interface
(`actions` / `states` / `values` dicts, `invoke` / `adjust` / `set` / `watch` /
`watch_states`) — **in the plugin's own child process**; the daemon never imports old code.

```python
# plugin dir: manifest.json + plugin.py + old_plugin.py
from old_plugin import MyPlugin
from ds_plugin_py import compat
compat.serve_plugin(MyPlugin)          # manifest defaults to compat.manifest_for(MyPlugin)
```

Translation: one manifest action per `actions` entry (`params` = the entry's JSON Schema, or
the pydantic arg model's `model_json_schema()` when pydantic is installed), one `set.<name>`
action per `states`/`values` entry (forwards to the old `set(name, value)` → `set` effect on
`/plugins/<id>/state/<name>`); `invoke`/`adjust` become the action handlers; host events are
forwarded to `watch`/`watch_states`; the ctx handed to the old plugin captures
`ctx.set_state(name_or_path, value)` (prefixed to `/plugins/<id>/state/`), `ctx.emit(topic,
payload)` (prefixed to `plugin/<id>/`), `ctx.log(level, msg)`, `ctx.get_state(path)` and
`ctx.http(method, url)` (host-mediated).

**Guarantees** (contract §6): process isolation, lifecycle, wire contract, resource limits,
host-mediated network. **Intentionally NOT supported**: engine internals (`ctx.deck` depth —
OPEN C1), in-process state sharing between plugins, old-engine timing semantics, pydantic arg
models when pydantic is not installed (permissive `{"type":"object"}` fallback), and the
engine-executed kinds (`page`/`preset`/`flow`/`style`). Old-app plugins are ported manually,
ticket-driven, post-alpha (`docs/70-porting-notes.md`) — nothing is auto-imported.

## Layout / tests

```
plugins-py/
  ds_plugin_py/        the SDK (wire, session, manifest, params, effects, http, errors, compat)
  deckstack_plugin/    alias package re-exporting ds_plugin_py
  examples/demo_plugin.py   the §7-style example plugin
  tests/               unittest suite: mock-host protocol session + compat fixture class
  smoke.py             end-to-end: ds-vdev + daemon + example plugin, effects asserted
```

```sh
python -m unittest discover -s plugins-py/tests -p 'test_*.py'   # protocol + compat tests
python plugins-py/smoke.py                                       # end-to-end vs real daemon
python -m build plugins-py            # wheel builds (or: pip wheel --no-deps plugins-py)
```

Nothing is published from here; publishing is DS-083 (PyPI name `ds-plugin-py`, subtree split
`release/plugin-py` per `repos.toml`).
