# Contract: plugins

R6/R8. Design: ADR-006, D08/D09. Manifest schema: `schemas/plugin-manifest.json`. Fixes
S4/M5.

## 1. Process model

One OS process per plugin from `~/.local/share/deckstack/plugins/<id>/`, `command` =
argv array (never a shell). Transport: newline-JSON over stdin/stdout (stderr → daemon
log). Handshake:

```json
host→plugin  {"op":"hello","protocol":1,"plugin_token":"…","caps":["state.read","device.write","net:lights.lan"],"roots":{"fs":["/home/u/media"]},"limits":{"cpu_s":10,"nproc":64,"nofile":64,"as_mb":1024}}
plugin→host  {"op":"register","id":"demo","manifest":{…}}
host→plugin  {"op":"event","event":"keyDown","control":"keys.0","state":{…}}
host→plugin  {"op":"invoke","id":"demo.bump","args":{"by":1},"request_id":7}
plugin→host  {"op":"effects","request_id":7,"effects":[{"effect":"set","path":"/controls/keys.0/label","value":"1"}]}
plugin→host  {"op":"request","request_id":8,"kind":"get","path":"/controls/keys.0"}
plugin→host  {"op":"request","request_id":9,"kind":"net","method":"GET","url":"http://lights.lan/api/…"}   ← S4
host→plugin  {"op":"reply","request_id":9,"ok":true,"status":200,"body":"…"}
```

## 2. Network capability — host-mediated (S4/M5)

Landlock network rules (ABI ≥ 4) can restrict **ports**, not hostnames, and a plugin can
open any socket directly — so `net:<glob>` is **not** enforced by sandboxing the plugin's
sockets. Instead:

- Plugins **never open sockets**. The SDK exposes `sdk.http(method, url, …)` (Python:
  `ds_plugin_py.http(...)`), which sends `request kind:"net"` to the daemon.
- The daemon parses the URL, extracts the host, matches it against the plugin's granted
  `net:<glob>` **and** `/policy/net/allow_hosts`, then performs the request itself and
  returns status/body. Glob semantics (M5): `*` matches exactly one DNS label; matching
  is anchored against the full host — `lights.lan` matches only `lights.lan`; `*.lan`
  matches `lights.lan` but not `lan`, `a.b.lan`, or `lights.lan.evil.com`. Edge-case tests
  in tests/fixtures.
- Where Landlock ABI ≥ 4 is available (OPEN O6/O11), the daemon additionally denies
  outbound connect from the plugin process entirely (defense in depth); where it is not,
  the host-mediation is the only control — **documented gap** (docs/20-security.md T11).
- DNS is resolved by the daemon at request time and the connection is made to the
  resolved address after re-checking the host string (no TOCTOU via the plugin).

## 3. Manifest (schemas/plugin-manifest.json)

`{id, name, version, command[], actions[{id, name, params: <JSON Schema>}],
capabilities[], events[], resources{}, fs_roots[]}`. `id` ∈ `[A-Za-z0-9._-]{1,80}`
(precedent `upstream/.../plugins.rs:98-100`). Action args validated against `params`
before invoke. Capabilities enforced per request/effect — not displayed only (contrast
`upstream/.../plugins.rs:56-58`).

## 4. Lifecycle

`install → start (spawn, rlimits, Landlock fs + net-deny where available) → register
(manifest must match, else E_SCHEMA + kill) → events/invokes → crash/hang → watchdog kill
+ respawn ≤ 3/10 min → remove`. Daemon exit kills all plugin processes (Windows job
object, `upstream/.../plugins.rs:180-205`; process group elsewhere).

## 5. Failure isolation

- Effects validated against the state schema subset before touching state; invalid →
  rejected, counted in `/plugins/<id>/stats.violations`.
- Invoke timeout 2 s → `degraded` (stops receiving events); second consecutive → restart.
- Resource abuse → rlimits kill; daemon unaffected (conformance: fork-bomb, traversal,
  cap violation, oversized net URL — docs/20-security.md T3/T5/T6/T11).

## 6. Python compatibility wrapper (ds-plugin-py, D09)

Runs an existing hubdeck plugin (`hubdeck/.../plugins/base.py:112-272` interface) in a
child process, translates to the ds wire protocol: manifest generated from
`actions/states/values` + pydantic arg models (existing `plugins/host/manifest.py` emits
JSON Schema), `invoke/adjust/set` → effects, `watch/watch_states` → subscriptions, plus
`sdk.http` for network. **Guarantees:** process isolation, lifecycle, wire contract,
resource limits, host-mediated network. **Does not guarantee:** engine internals
(`ctx.deck` depth — OPEN C1), in-process state sharing, timing. Old-app plugins are ported
manually, ticket-driven, post-alpha (`docs/70-porting-notes.md`) — nothing is imported.

## 7. Example plugin (Python, ~40 lines)

```python
import ds_plugin_py as sdk
count = {"n": 0}
@sdk.action("bump", params={"type":"object","properties":{"by":{"type":"integer","default":1}}})
def bump(args):
    count["n"] += args.get("by", 1)
    return [sdk.set("/controls/keys.0/label", str(count["n"])),
            sdk.set("/controls/keys.0/image", {"kind":"color","value":"#ff8800"})]
@sdk.action("get_lamp")
def get_lamp(args):
    r = sdk.http("GET", "http://lights.lan/api/lamp")     # host-mediated, glob-checked (S4)
    return [sdk.set("/plugins/demo/state/last", r["body"])]
sdk.serve({"id":"demo","name":"Demo counter","version":"0.1.0",
           "capabilities":["state.write","net:lights.lan"],"actions":[bump, get_lamp]})
```

## 8. SDK shape

`ds-plugin-sdk` (Rust) and `ds-plugin-py` (Python): `serve(manifest)`,
`@action(id, params)`, effect builders `set/emit/log`, `get/set_state`, `http()` (S4),
event iteration. Same wire protocol both sides. Test: `cargo test -p ds-plugin-sdk` +
`tests/conformance/plugins/`.

## 9. Host error mapping, lifecycle state and logs (DS-050 host, normative)

The host (`crates/ds-daemon/src/plugins/`, component `docs/components/plugin-host.md`)
maps every plugin failure to the stable control-plane codes (`docs/contracts/ipc.md` §4):

| situation | code |
|---|---|
| unknown plugin id (not installed) / unknown action on a running plugin | `E_PATH` |
| plugin not running (`stopped`/`crashed`/disabled) or crashed **mid-call** | `E_DEVICE` |
| capability violation (effect/request without the grant, path outside the writable subset, net host not granted) | `E_CAP` |
| invoke `args` violate the action's `params` schema; manifest invalid/mismatch at register; malformed wire line | `E_SCHEMA` |
| invoke timeout (2 s) — plugin `degraded` on the first, restarted on the second consecutive | `E_LIMIT` |
| spawn/host internal failure | `E_INTERNAL` |

Lifecycle state is exposed at `/plugins/<id>` (`{manifest, status, state, stats}`,
`schemas/state.json`) — readable with the ordinary `get /plugins` op and reflected in
`daemon status` through `rev`. `status` ∈ `installing | running | degraded | stopped |
crashed`; a plugin the watchdog gave up on is `stopped` with
`state.disabled_reason = "watchdog: 3 respawns within 600 s; plugin disabled"` and a
`plugin/<id>/disabled` bus event. `stats` carries `restarts`, `violations`, `invokes`.
A crash publishes `plugin/<id>/crash` on the bus and a `warn` daemon log line.

Plugin output never reaches the control plane: stdout is the protocol channel
(malformed lines are counted as violations + logged), **stderr goes to the daemon log
and a per-plugin ring buffer of the last 200 lines** (`Host::logs(id)` in-crate; the
`ds plugin logs <id>` verb lands with the CLI ticket). The plugin's `log` op/request
kind writes the same ring.

Writable-state subset for plugin effects/requests: `set` may only touch `/controls/…`
or the plugin's own `/plugins/<id>/…` (capability `state.write`); `emit` only topics
under `plugin/<id>/…` (capability `emit`); every `set` is schema-validated by the state
task on commit — an invalid effect is rejected, counted in `stats.violations`, and
never touches state. At most 100 effects per invoke (extras rejected + counted).

Handshake deadline: `register` must arrive within 5 s of `hello` and its manifest must
be schema-valid **and** equal (semantically) to the on-disk `manifest.json`, else
`E_SCHEMA` + the child is killed.
