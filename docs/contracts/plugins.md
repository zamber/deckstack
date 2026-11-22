# Contract: plugins

R6/R8. Design: ADR-006, D08/D09. Manifest schema: `schemas/plugin-manifest.json`.

## 1. Process model

One OS process per plugin, launched by the daemon from
`~/.local/share/deckstack/plugins/<id>/` using the manifest's `command` (argv array —
never a shell string). Transport: **newline-delimited JSON over the plugin's
stdin/stdout** (stderr → daemon log). Handshake:

```json
host→plugin  {"op":"hello","protocol":1,"plugin_token":"…","caps":["state.read","device.write"],"roots":{"fs":["/home/u/media"]},"limits":{"cpu_s":10,"nproc":64,"nofile":64,"as_mb":1024}}
plugin→host  {"op":"register","id":"demo","manifest":{…}}
host→plugin  {"op":"event","event":"keyDown","control":"keys.0","state":{…}}
host→plugin  {"op":"invoke","id":"demo.bump","args":{"by":1},"request_id":7}
plugin→host  {"op":"effects","request_id":7,"effects":[{"effect":"set","path":"/controls/keys.0/label","value":"1"}]}
plugin→host  {"op":"request","request_id":8,"kind":"get","path":"/controls/keys.0"}   # cap-gated
host→plugin  {"op":"reply","request_id":8,"ok":true,"value":{…}}
```

## 2. Manifest (schemas/plugin-manifest.json)

```json
{
  "id": "demo", "name": "Demo counter", "version": "0.1.0",
  "command": ["python3", "-m", "ds_plugin_py", "plugin.py"],
  "actions": [{"id": "bump", "name": "Bump", "params": {"type":"object",
              "properties": {"by": {"type":"integer","default":1}}}}],
  "capabilities": ["state.read","state.write","device.write"],
  "events": ["keyDown","keyUp","state"],
  "resources": {"cpu_s": 10, "nproc": 64, "nofile": 64, "as_mb": 1024},
  "fs_roots": ["/home/u/.local/share/deckstack/media"]
}
```

`id` restricted to `[A-Za-z0-9._-]{1,80}` (precedent `safe_id`,
`upstream/.../plugins.rs:98-100`). Action `params` are JSON Schema; args are validated
against them before invoke. `capabilities` are enforced, not displayed (contrast
`upstream/.../plugins.rs:56-58`, where permissions are shown only).

## 3. Lifecycle

`install (dir copied + manifest validated) → start (spawn, rlimits set, Landlock applied
on Linux ≥6.7 — OPEN O6 — else fs_roots enforced by the host on every `request`) →
register (manifest must match, else `E_SCHEMA`, process killed) → events/invokes →
crash/hang → watchdog kills, respawn ≤ 3 per 10 min with backoff → `remove` kills and
deletes`. Daemon exit kills all plugin processes (job object on Windows, precedent
`upstream/.../plugins.rs:180-205`; process group elsewhere).

## 4. Capabilities

`state.read, state.write, device.write (frames/leds via effects), exec, net:<host-glob>,
fs:<root>, emit`. Each plugin gets a `plugin_token` (random per start) that authorizes
exactly its granted caps; the daemon checks caps on every `request` and every effect.
Default grant: `state.read` only.

## 5. Failure isolation

- Effect payloads validated against `schemas/state.json` subset; invalid → rejected,
  `E_SCHEMA`, counted in `/plugins/<id>/stats.violations`.
- Invoke timeout 2 s → plugin marked `degraded` (stops receiving events), not killed on
  first offense; second consecutive timeout → restart.
- Resource abuse (fork bomb, memory) → rlimits kill the child; daemon unaffected.
  Test: `tests/conformance/plugins/abuse.py` asserts daemon health after each.
- Plugin state lives under `/plugins/<id>/state` — the daemon owns it; the plugin can
  only read/write it via granted requests.

## 6. Python compatibility wrapper (ds-plugin-py, D09)

Runs an existing hubdeck plugin (`hubdeck/.../plugins/base.py:112-272` interface) in a
child process and translates: manifest generated from the plugin's
`actions/states/values` dicts + pydantic arg models (the existing `manifest`/`wire()`
emits JSON Schema already — `hubdeck/.../plugins/host/manifest.py`), `invoke/adjust/set`
→ effects, `watch/watch_states` → event subscriptions. **Guarantees:** process isolation,
lifecycle, wire contract, resource limits, capability enforcement. **Does not guarantee:**
access to engine internals (`ctx.deck` beyond the proxied surface — see OPEN C1), in-process
state sharing, timing. Plugins needing internals are ported (list produced by
`ds migrate import`, docs/70-migration.md).

## 7. Example plugin (Python, ~40 lines)

```python
# plugins/demo/plugin.py — run via ds-plugin-py
import ds_plugin_py as sdk
count = {"n": 0}
@sdk.action("bump", params={"type":"object","properties":{"by":{"type":"integer","default":1}}})
def bump(args):
    count["n"] += args.get("by", 1)
    return [sdk.set("/controls/keys.0/label", str(count["n"])),
            sdk.set("/controls/keys.0/image", {"kind":"color","value":"#ff8800"})]
sdk.serve({"id":"demo","name":"Demo counter","version":"0.1.0",
           "capabilities":["state.write"],"actions":[bump]})
```

## 8. SDK shape

- `ds-plugin-sdk` (Rust) and `ds-plugin-py` (Python) expose: `serve(manifest)`,
  `action(id, params)(fn)`, effect builders `set/emit/log`, request helpers
  `get/set_state`, event iteration. Same wire protocol both sides.
- Test command: `cargo test -p ds-plugin-sdk` + `tests/conformance/plugins/`.
