# Contract: actions — input event → behaviour, no hardcoded catalog

R3/R4/R8. Design: ADR-007, D07. Binding schema: `schemas/binding.json`. Fixes S3/S7/S11.

## 1. Event topics

```
deck/control/<control>/<gesture>     control: keys.0 | dial | knobs.0 | faders.2 | rollers.1
                                     gesture: down | up | long | double | rotate | slide
deck/device/<serial>/<event>         connected | disconnected | faders | orientation
deck/system/<event>                  start | stop | rev | binding-error | exec | crash-recovered
app/<sender>/<topic>                 any external program (ds emit app/focus/changed …)
```

Events: `{topic, ts_ms, payload, rev}`.

## 2. Binding / routing model (S2)

Bindings are elements of the `/bindings` **array** (`schemas/binding.json`), addressed by
`id` (`b0001`). Example:

```json
{"id": "b0002", "topic": "app/focus/changed",
 "when": {"payload": "app", "op": "eq", "value": "code"},
 "allow_exec": false,
 "steps": [{"do": "set", "path": "/controls/keys/3/image", "value": {"kind":"color","value":"#00ff00"}}]}
```

- Lookup: exact topic → `<prefix>/*` wildcard bindings. All matching bindings run in
  array order.
- `when` evaluates against **state** (`path`) **or the event payload** (`payload` field
  name) — S7. For `app/focus/changed` the router first mirrors the payload into `/focus`
  **in the same transaction** that dispatches bindings, so `/focus`-based conditions are
  never stale (atomic ordering; conformance test in docs/30-testing.md).
- Steps run sequentially; `emit` steps can trigger other bindings (recursion cap 10 —
  precedent `hubdeck/.../engine.py:1088-1112`).

## 3. Core vocabulary (closed)

| Step `do` | Args | Capability |
|---|---|---|
| `set` | path, value (schema-validated) | mutate |
| `emit` | topic, payload | mutate |
| `plugin` | id, action, args (validated against manifest params) | mutate |
| `exec` | argv, cwd — **fire-and-forget** (S11) | admin scope + `allow_exec:true` + `/policy/exec/allow` ∋ argv[0] + `/policy/exec/deny` ∌ argv[0] |
| `sleep` | ms | — |

## 4. Exec policy — enforced, not asserted (S3, reconciles review M1/M2)

- Schema level (M1): `argv[0]` must match `^/` (absolute) — `prefixItems` pattern in
  `binding.json`; relative paths fail validation. `cwd` may be any absolute path (M2);
  runtime confines it to `/policy/fs/roots` + `/tmp`.
- Runtime level (S3): the daemon checks `argv[0]` against **`/policy/exec/allow`** (an
  admin-managed allowlist of absolute binary paths) and **`/policy/exec/deny`**, which by
  default contains the shells/interpreters/wrappers — `/bin/sh`, `/bin/bash`, `/bin/dash`,
  `/bin/zsh`, `/usr/bin/env`, `/usr/bin/python3`, … — so `["/bin/sh","-c","…"]` is refused
  even though `/bin/sh` is absolute. "No shell" is a deny list, not an assertion.
  `/settings/exec_path` is gone; PATH lookup is gone (absolute paths only).
- Semantics (S11/m5): exec steps are **fire-and-forget** — the daemon spawns via execve
  (never a shell), applies rlimits (cpu 10 s → SIGKILL, nproc 64, nofile 64, as 1 GiB),
  scrubs env (no token), tracks the pid, and emits `deck/system/exec {argv, pid, rev}`.
  There is **no `timeout_ms`** in the schema; runaway processes die by RLIMIT_CPU.
- Audit: every exec logged + emitted. Config can never contain code: `binding.json` has
  no code field; `eval`-like steps fail schema validation (fixture negative test).

## 5. Latency budget (R11)

Event → dispatch p99 ≤ 5 ms; `set`/`emit` p99 ≤ 10 ms; plugin invoke ≤ 2 s timeout
(`ASK_SECONDS` precedent `hubdeck/.../engine.py:336-344`), typical ≤ 50 ms; exec spawn
≤ 10 ms (fire-and-forget). Frame pushes never move these budgets (S1).

## 6. Worked chains

**Chain A — key press → plugin → visual update:**

```sh
ds bind deck/control/keys.0/down --do 'plugin demo.bump {"by":1}' --json   # → {"id":"b0001"}
# plugin demo returns effects on invoke:
#   [{"effect":"set","path":"/controls/keys.0/label","value":"3"},
#    {"effect":"set","path":"/controls/keys.0/image","value":{"kind":"color","value":"#ff8800"}}]
# conformance: inject key-down on ds-vdev → assert label+colour changed within 2 s, rev bumped twice.
```

**Chain B — window focus → CLI → recolor (R4, S7):**

```sh
ds-focus &                                        # emits app/focus/changed {app,title,pid}
ds bind app/focus/changed --when '{"payload":"app","op":"eq","value":"code"}' \
     --do 'set /controls/keys/3/image {"kind":"color","value":"#00ff00"}'
# conformance: ds-focus --inject focus-code.ndjson → assert /controls/keys/3/image changed
# AND that a when-condition on /focus/app (not payload) saw the new value in the same dispatch.
```
