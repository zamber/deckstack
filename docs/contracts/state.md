# Contract: state model

Normative schema: `schemas/state.json`. Design: ADR-004, D05. R2.

## 1. Addressing

One JSON document. Paths are `/`-separated, segments are literal or `<id>`; `*` in queries
matches one segment. Real top-level paths:

```
/meta            {version: 1, rev: <int>, daemon_pid, started_at, protocol: 1}
/devices/<serial>            {kind: keys|dial|m0-base|faders|knobs|rollers, model, pid,
                              slot: "usb-path", connection: connected|disconnected,
                              capabilities: [...], last_seen_rev}
/controls/keys/<0-14>        {image, label, color, bg, icon, binding-independent visual}
/controls/dial               {image, label, ...}
/controls/knobs/<0-2>        {led: {mode, color, brightness}, value, on: {...}}
/controls/faders/<0-4>       {led, value}
/controls/rollers/<0-1>      {led, on: {...}}
/bindings/<control>/<gesture>  [binding, ...]            (gesture: down|up|long|double|rotate|slide)
/bindings/app/<sender>/<topic> [binding, ...]
/bindings/deck/device/<serial>/<event> [binding, ...]
/plugins/<id>                {manifest, status: running|degraded|stopped, stats, state}
/streams/<target>            {queue_depth, stats: {sent, dropped, fps, bytes, last_sha256}}
/settings/...                {sender backends, log level, led_rate_cap, ...}
/policy/...                  e.g. /policy/allow/net/hosts: ["lights.lan"]   (D18)
/focus                           {app, title, pid, at_rev}   (written by ds-focus via emit+set)
```

**Identity across hot-plug:** `/devices/<serial>` — serial read at connect
(`upstream/crates/mh-device/src/hub.rs:241-245`). Control paths hang off the *kind slot*
(`keys`, `dial`, `knobs.0`), not the serial; when a module with a known serial
reconnects, the daemon re-links its slot and re-pushes the last state, so visuals survive
replug. Two same-kind modules: `keys` mirrors, a second knob/fader/roller set addresses by
serial suffix (`upstream/PROTOCOL.md:130-139`).

## 2. Types

Control visual (`/controls/keys/0` etc.):

```json
{
  "image":  {"kind": "color", "value": "#3cc86e"}
}
```
`image` is one of: `{"kind":"color","value":"#rrggbb"}`, `{"kind":"frame","target":"keys|dial","seq":N,"sha256":"…"}`,
`{"kind":"file","path":"/abs/or/relative-to-media-root.png"}`, `{"kind":"icon","name":"tabler:bolt"}`,
`{"kind":"text","value":"OK","fg":"#fff","bg":"#000"}`. `label`: string. All validated by
`schemas/state.json`.

## 3. Durability, transactions, revisions

- Every mutation is a **transaction**: `{"ops": [{"op":"set","path":…,"value":…}, …],
  "if_rev": N}` — applied atomically; if the current rev ≠ `if_rev`, the whole transaction
  fails with `E_CONFLICT` (exit 3) and returns the current rev. Omitting `if_rev` is
  last-write-wins (allowed for single-key sets).
- Each committed transaction increments `/meta/rev` by 1 and is persisted: the daemon
  serializes the document to `state.json.tmp` and `rename()`s it (atomic;
  `upstream/.../engine/profile.rs:882-909`), debounced 250 ms, fsync on daemon stop.
- **Durable:** everything except `/streams/*/stats` queue internals and `/focus` (ephemeral,
  kept in memory only). Frame bytes are never durable (ADR-005).
- **Crash recovery:** on start, load `state.json`; if unparsable, move to
  `state.json.corrupt-<ts>` and start empty with a loud log line (`deck/system` event).

## 4. Subscriptions

`{"op":"subscribe","path":"/controls/*","from_rev":N}` → stream of
`{"op":"event","path":"/controls/keys/0","rev":123,"value":{…},"prev":…}`. Wildcard `*`
matches one segment; prefix subscription (`/controls`) delivers every change below.
Slow subscribers: daemon drops events and sends `{"op":"dropped","count":n}` (never
blocks the state task — R2/R5).

## 5. Query / filter semantics

`{"op":"get","path":"/controls/keys/0"}` → `{"op":"result","rev":N,"value":{…}}`.
`get /controls` → subtree. `get /controls/keys/*` → object keyed by index.
Filters are client-side: `ds get /controls --json | jq '…'`. Server-side only:
`{"op":"get","path":…,"fields":["image","label"]}` projection. No jq-in-the-daemon.

**Latency budget (D13, R2/R11):** under a 32 fps frame load on the virtual device,
`get` p50 ≤ 2 ms, p99 ≤ 10 ms (measured by `cargo bench -p ds-bench -- query-under-load`).
The state lock is an `RwLock` never held across I/O; frame plane shares no lock with it.

## 6. Conflict rules

- Same-path concurrent transactions: serialized by rev; loser gets `E_CONFLICT`.
- Plugin effect vs user set on the same path within one rev: plugin effect loses unless
  the binding step order says otherwise — effects apply in arrival order, each its own rev.
- Device-driven changes (fader value) write `/controls/faders/N/value` directly and emit
  an event; they never overwrite `image`/`label`.

## 7. Worked examples

Deck query (CLI): `ds get /controls/keys/0 --json` →

```json
{"ok":true,"rev":41,"value":{"image":{"kind":"color","value":"#3cc86e"},"label":"Salon"}}
```

Deck set (transactional, CLI): `ds set /controls/keys/0/image --json '{"kind":"color","value":"#ff8800"}' --if-rev 41`
→ `{"ok":true,"rev":42}`; with a stale `--if-rev 40` → exit 3,
`{"ok":false,"error":{"code":"E_CONFLICT","message":"rev is 42","current_rev":42}}`.

Subscription (API):

```json
→ {"op":"hello","protocol":1}
→ {"op":"auth","token":"…","scope":"mutate"}
→ {"op":"subscribe","path":"/controls/keys/*"}
← {"op":"event","path":"/controls/keys/0/color","rev":43,"value":"#ff8800"}
```

Policy example (D18 — Salon-only lights): `/policy/allow/net/hosts: ["lights.lan"]`; any
plugin `request` with `net` to another host → `E_CAP`. The daemon's device-write choke
point enforces it; core contains zero light-specific code.
