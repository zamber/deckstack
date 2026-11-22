# Contract: actions — input event → behaviour, no hardcoded catalog

R3/R4/R8. Design: ADR-007, D07. Binding schema: `schemas/binding.json`.

## 1. Event topics

```
deck/control/<control>/<gesture>     control: keys.0 | dial | knobs.0 | faders.2 | rollers.1
                                     gesture: down | up | long | double | rotate | slide
deck/device/<serial>/<event>         connected | disconnected | faders | orientation
deck/system/<event>                  start | stop | rev | binding-error | crash-recovered
app/<sender>/<topic>                 any external program (ds emit app/focus/changed …)
```

Events carry `{topic, ts_ms, payload, rev}`. Example payloads: key down
`{"control":"keys.0","gesture":"down"}`; focus `{"app":"code","title":"PLAN.md","pid":123}`.

## 2. Binding / routing model

Bindings are **data in state** at `/bindings/<topic-with-*>`:

```json
{"topic": "deck/control/keys.0/down",
 "when": {"path": "/focus/app", "op": "exists"},
 "steps": [{"do": "plugin", "id": "demo", "action": "bump", "args": {"by": 1}},
           {"do": "set", "path": "/controls/keys/0/label", "value": "2"},
           {"do": "emit", "topic": "app/demo/bumped", "payload": {}}]}
```

- Lookup order: exact topic → `…/<control>/*` → `deck/control/*` → `deck/#`-style global.
  Multiple bindings on one topic all run, in list order.
- `when`: `{path, op: exists|eq|neq|gt|lt, value?}` evaluated against state at event time;
  missing path ⇒ `exists:false`, others false.
- Composition: steps run sequentially; a step may `emit`, which can trigger other
  bindings (recursion depth cap 10 — precedent `hubdeck/.../engine.py:1088-1112`).

## 3. Core vocabulary (closed — this list is the whole engine-side action set)

| Step `do` | Args | Capability needed |
|---|---|---|
| `set` | path, value (schema-validated) | mutate |
| `emit` | topic, payload | mutate |
| `plugin` | id, action, args | mutate (plugin must declare the action) |
| `exec` | argv (array, **no shell**), cwd, env, timeout_ms | **admin scope + binding flag `allow_exec: true`** |
| `sleep` | ms | — |

`exec` runs the binary with argv directly (no `/bin/sh` — the `shell=True` at
`hubdeck/.../platforms/posix.py:70` is the rejected injection vector), with rlimits
(cpu 10 s, nproc 64, nofile 64, as 1 GiB), cwd confined to the media root or `/tmp`,
env scrubbed (no token in env). Plugin invocation: daemon sends `invoke` to the plugin
process, awaits effects ≤ 2 s (`ASK_SECONDS` precedent, `hubdeck/.../engine.py:336-344`),
applies each effect as a state write (`setImage` → `/controls/<c>/image`, `setTitle` →
`label`, `setState`, `setLed` → `/controls/<c>/led`; upstream event names
`upstream/.../plugins.rs:14-20`).

Why no catalog: both current engines hardcode theirs (`upstream/.../engine/actions.rs:105`,
`hubdeck/.../plugins/__init__.py:22-28` + `ALIASES` `config/schema.py:55-67`). Here any
program can `ds watch deck/control --ndjson` and react — the vocabulary is the ecosystem.

## 4. Exec policy (R8)

Default-deny. An `exec` step requires ALL of: token scope `admin`, binding field
`"allow_exec": true`, argv[0] an absolute path or resolved via a declared
`/settings/exec_path` (default `["/usr/bin","/bin","/usr/local/bin"]`, no `.`), and the
exec capability in `/policy`. Audit: every exec is logged to `daemon.log` and emitted as
`deck/system/exec {argv, pid, at_rev}`. Config can never contain code (no eval — the
`{python: …}` exec at `hubdeck/.../plugins/python.py:58` is rejected by R8 and by
`schemas/binding.json`, which has no code field).

## 5. Latency budget (R11)

Event → router dispatch: p99 ≤ 5 ms. `set`/`emit` step: p99 ≤ 10 ms. Plugin round-trip:
≤ 2 s timeout, typical ≤ 50 ms. Frame push under load must not move the query p99
budget (state.md §5). Measured by `cargo bench -p ds-bench`.

## 6. Worked chains

**Chain A — key press → plugin → visual update:**

```sh
ds bind /controls/keys/0/down --do 'plugin demo.bump {}' --json
# plugin demo (Python, via ds-plugin-py) on invoke demo.bump returns effects:
#   [{"effect":"set","path":"/controls/keys/0/label","value":"3"},
#    {"effect":"set","path":"/controls/keys/0/image","value":{"kind":"color","value":"#ff8800"}}]
# daemon applies effects → rev++ → ds watch sees:
#   {"op":"event","path":"/controls/keys/0/label","rev":44,"value":"3"}
# conformance: inject key-down on ds-vdev, assert label+colour changed within 2 s.
```

**Chain B — window focus → CLI → recolor (R4):**

```sh
ds-focus &                                   # emits app/focus/changed {app,title,pid}
ds bind app/focus/changed --when '{"path":"/focus/app","op":"eq","value":"code"}' \
     --do 'set /controls/keys/3/image {"kind":"color","value":"#00ff00"}' \
     --do 'emit app/focus/applied {"key":3}'
# conformance: ds-focus --inject focus-code.ndjson → assert /controls/keys/3/image changed.
```
