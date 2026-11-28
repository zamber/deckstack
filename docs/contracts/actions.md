# Contract: actions — input event → behaviour, no hardcoded catalog

R3/R4/R8. Design: ADR-007, D07. Binding schema: `schemas/binding.json`. Fixes S3/S7/S11.

## 1. Event topics

```
deck/control/<control>/<gesture>     control: keys.0 | dial | knobs.0 | faders.2 | rollers.1
                                     gesture: down | up | long | double | rotate | slide | value
deck/device/<serial>/<event>         connected | disconnected | orientation | faders
deck/system/<event>                  start | stop | rev | binding-error | exec | crash-recovered
daemon/<event>                       start | stop | reload        (daemon lifecycle namespace)
app/<sender>/<topic>                 any external program (ds emit app/focus/changed …)
plugin/<id>/<event>                  reserved for the plugin host (phase 5)
```

Events: `{topic, ts_ms, payload, rev}`. `rev` is present **only on state events** (a
committed revision of the state document); device/app/daemon events carry `rev: null`.
State changes are themselves bus events whose topic is the state path
(`/controls/keys/0/image`), delivered to path subscribers in the historical
`{"op":"event","path":…,"rev":…,"value":…,"prev":…}` wire shape — one event per changed
path per committed revision. Frames and `/telemetry/*` never enter the bus (S1).

Device input reports are decoded into `deck/control/*` events by the daemon's device
task (`ds-vdevctl inject` on the virtual device, real HID in phase 2):

| input report | topic | payload |
|---|---|---|
| key state diff | `deck/control/keys.<i>/down\|up` | `{"control","key","pressed"}` |
| dial detent / press | `deck/control/dial/rotate` · `down\|up` | `{"control":"dial","delta"\|"pressed"}` |
| knob detent / press | `deck/control/knobs.<i>/rotate` · `down\|up` | `{"control","index","delta"\|"pressed"}` |
| roller notch | `deck/control/rollers.<i>/rotate` | `{"control","index","delta"}` |
| fader value diff | `deck/control/faders.<i>/slide` | `{"control","index","value"}` |
| base orientation | `deck/device/<serial>/orientation` | `{"orientation"}` |

`long`/`double` need hold/double timing and are synthesized later (DS-042).

On the wire, a **topic subscription** is spelled with a leading `/` in the `subscribe`
op's `path` (`{"op":"subscribe","path":"/deck/control/keys/*"}`); topic events arrive as
`{"op":"event","topic":"deck/control/keys.0/down","path":"/deck/control/keys.0/down","ts_ms":…,"payload":{…}}`.
A `path` that does not start with a topic root (`deck`/`app`/`daemon`/`plugin`) remains a
state-path subscription with the `{"op":"event","path":…,"rev":…}` shape above.

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
- **Wildcard grammar:** in topics, `/` **and** `.` both separate segments for matching
  (`keys.0` is the two segments `keys` + `0`). A pattern matches a topic when the pattern
  is a segment-wise **prefix** of the topic and each non-`*` pattern segment equals the
  corresponding topic segment; `*` matches exactly one segment. So
  `deck/control/keys/*` matches `deck/control/keys.0/down` (`*` eats the `0`), `deck/*`
  matches every `deck/…` topic, and `*` alone matches everything. State-path patterns
  (`/controls/keys/*`) use the same prefix rule with `/`-only segments. In a binding
  `topic`, `*` is allowed only as a **whole segment** (the coarse pattern in
  `schemas/binding.json` is refined by the code-level grammar in
  `ds-state::bindings::validate_binding_topic`, enforced on every `bind`).
- **Creating / deleting bindings (DS-041):** `ds bind <topic> --do '<step>' [--do …]
  [--when '<cond>']` sends `{"op":"bind","binding":{…}}` and appends ONE element to
  `/bindings` in ONE transaction (rev +1, one `/bindings` subscription event). The
  request's `binding.id` is a **placeholder** (the IPC schema requires the field shape;
  `ds` sends `b0000`) — the daemon always allocates the real id and returns it:
  `{"ok":true,"rev":N,"result":{"id":"b0001"}}` (state.md §8). Ids are `b%04d` of a
  monotonic high-water counter that starts at the maximum id in the loaded document and
  is never lowered by an unbind: allocated on add, **never reused** within a daemon
  run, so a duplicate id is impossible. `ds unbind <id>` sends
  `{"op":"unbind","binding":"b0001"}` (unknown id → `E_PATH`, CLI exit 2);
  `ds unbind --topic <topic>` sends `{"op":"unbind","topic":…}` and removes ALL bindings
  with exactly that topic (no match → `E_PATH`). Both are one transaction (rev +1) and
  answer `{"ok":true,"rev":N,"result":{"removed":["b0001"]}}`. `ds get /bindings` lists
  all, `ds get /bindings/<id>` reads one, `ds watch /bindings --ndjson` streams the
  changes (topics stay inside the `topic` field, never path segments — S2).
- **Dispatch order:** on every published event, matching subscribers/bindings are served
  **exact patterns first, then wildcard patterns**, each group in subscription (array)
  order; each subscriber receives its events strictly FIFO in publish order.
- **Backpressure:** every subscriber has a bounded queue (cap **1000** queued events,
  ipc.md §5). A slow or blocked subscriber never blocks the publisher (state task,
  device reader, IPC thread): on overflow the **oldest** queued event is dropped and
  counted, the daemon logs a clear `warn` line (`subscriber #N (…) queue full (1000
  queued): dropped oldest event … — total dropped M`), and the client is told with
  `{"op":"dropped","path":"<pattern>","count":n}` (state.md §5). A dead connection is
  detected on the next send and its subscription removed.
- `when` evaluates against **state** (`path`) **or the event payload** (`payload` field
  name) — S7. For `app/focus/changed` the router first mirrors the payload into `/focus`
  **in the same transaction** that dispatches bindings, so `/focus`-based conditions are
  never stale (atomic ordering; conformance test in docs/30-testing.md). The payload is
  `{app, title, pid?}`: `pid` is included when the backend knows it (DS-114) — absent or
  null is accepted and omitted from the `/focus` mirror, a present non-integer `pid`
  is rejected with `E_SCHEMA` before any state change.
- **`when` shape (normative in `schemas/binding.json`):** exactly one of `path` /
  `payload`; `op` ∈ `exists | eq | neq | gt | lt`; `value` required for every op except
  `exists` (and forbidden for `exists`). DS-041 stores and validates the shape verbatim;
  **evaluation is DS-042** — a stored `when` that does not match simply does not fire.
- Steps run sequentially; `emit` steps can trigger other bindings (recursion cap 10 —
  precedent `hubdeck/.../engine.py:1088-1112). **Step execution itself is DS-042**: DS-041
  accepts and schema-validates the step shapes (`set`/`emit`/`plugin`/`exec`/`sleep`,
  non-empty `steps`, no code fields — `{"do":"eval",…}` fails validation) but runs
  nothing yet.

## 3. Core vocabulary (closed)

| Step `do` | Args | Capability |
|---|---|---|
| `set` | path, value (schema-validated) | mutate |
| `emit` | topic, payload | mutate |
| `plugin` | id, action, args (validated against manifest params) | mutate |
| `exec` | argv, cwd — **fire-and-forget** (S11) | admin scope + `allow_exec:true` + `/policy/exec/allow` ∋ argv[0] + `/policy/exec/deny` ∌ argv[0] |
| `sleep` | ms | — |

DS-113 serves this closed vocabulary — plus every registered plugin's manifest actions
(`id`/`name`/`params` schema/capabilities, `docs/contracts/plugins.md` §3/§9) — as one
read-only aggregate document (`schemas/action-metadata.json/v1`) at
`GET /preview/api/action-metadata` on the opt-in loopback debug surface
(`docs/contracts/preview.md`).

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
The in-daemon bus (DS-040) keeps the publish path inside this budget by design:
`publish` takes one short lock, matches patterns and try-enqueues into bounded
per-subscriber queues (never a socket write), so event → queue is O(subscribers) with no
I/O; delivery happens on per-subscriber drainer threads. Measured by the DS-045 bench
(`cargo bench -p ds-bench -- event-latency`), asserted there, not here.

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
ds-focus &                                        # emits app/focus/changed {app,title,pid?}
ds bind app/focus/changed --when '{"payload":"app","op":"eq","value":"code"}' \
     --do 'set /controls/keys/3/image {"kind":"color","value":"#00ff00"}'
# conformance: ds-focus --inject focus-code.ndjson → assert /controls/keys/3/image changed
# AND that a when-condition on /focus/app (not payload) saw the new value in the same dispatch.
```
