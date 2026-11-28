# Contract: state model

Normative schema: `schemas/state.json`. Design: ADR-004, D05. R2.

## 0. The two planes (S1 — read first)

The state document has a **durable** part and a **non-durable telemetry** part, and the
frame plane touches neither per frame:

- Pushing a frame onto the frame plane (streaming.md) updates **no durable state and no
  `rev`** — at 24–32 fps a rev bump and full-document serialization per frame would violate
  R2/R5. Frame pushes are acknowledged on the stream socket (`stream-ack`/`stream-nack`)
  and counted in telemetry only.
- `/telemetry/*` is sampled by the daemon at **≤ 1 Hz**, is **never persisted**, never
  bumps `rev`, and `get /telemetry/...` returns the current sample with no rev in the
  response. Subscription events on `/telemetry/*` carry `"telemetry": true` and no `rev`.
- An `image: {kind:"frame", target, seq, sha256}` descriptor changes **only when the source
  changes** (a `set` op), never per pushed frame. `seq`/`sha256` there describe the frame
  that is the current source, not a per-frame counter.

## 1. Addressing

One JSON document. Paths are `/`-separated; `*` in queries matches one segment. Real
top-level paths:

```
/meta            {version: 1, rev, daemon_pid, started_at, protocol: 1}
/devices/<serial>            {kind, model, pid, slot, position, orientation, connection, last_seen_rev}
/controls/keys/<0-14>        {image, label, brightness}
/controls/dial               {image, label, brightness}
/controls/panel              {image, label, brightness}  (image also accepts the panel-only `colors` visual, DS-111)
/controls/knobs/<0-2>        {led: {mode, color, brightness}, value}
/controls/faders/<0-4>       {led, value}
/controls/rollers/<0-1>      {led, value}
/bindings                    ARRAY of binding objects {id: "b0001", topic, when, allow_exec, steps}
/bindings/<id>               one binding (S2: topics contain '/', so they are fields, not path segments)
/plugins/<id>                {manifest, status, state, stats}
/streams/<target>            durable per-target CONFIG only: {queue_depth: 1|2}
/telemetry/streams/<target> {sent, dropped, fps, bytes, queue, in_flight, last_sha256, last_seq, ...}  (non-durable, ≤1 Hz)
/settings/...                free-form by design (DS-119): {sender backends, log level, exec_path removed — see /policy/exec, ...} are the documented subset only, not an exhaustive schema; arbitrary additional keys are permitted and ignored by the daemon
/policy/...                  net.allow_hosts, fs.roots, exec.allow, exec.deny, led_rate_per_s, preview (DS-113 debug surface; default false)  (S13 — no policy exists only in prose)
/focus                       {app, title, pid?, at_rev}  (mirrored atomically from app/focus/changed, S7; pid only when the backend knows it — DS-114)

```

Targets everywhere are the unified vocabulary `keys | key:<0-14> | dial` (S6) — no
`panel`/`screen` synonyms. The one exception is the ds-vdevctl test tooling, where
`panel` is a vdev-only alias of `keys` (see `docs/contracts/streaming.md` §1 and
`docs/components/virtual-device.md`).

**Index ranges (S8, enforced at object level).** The deck layout fixes the control
indices, and `schemas/state.json` closes every indexed map with
`additionalProperties: false`, so an out-of-range index is rejected by the normative
schema itself, not only by the write path: `/controls/keys` accepts exactly `"0"`–`"14"`
(15 keys — `"15"` and `"99"` do not match), `/controls/knobs` exactly `"0"`–`"2"`,
`/controls/faders` exactly `"0"`–`"4"`, `/controls/rollers` exactly `"0"`–`"1"`, and
`/controls/dial` is a single control, not a map. `/streams` and `/telemetry/streams`
accept only the target vocabulary `keys | dial | key:<0-14>`.

**Identity across hot-plug (DS-112):** `/devices/<serial>` — serial read at connect
(`upstream/crates/mh-device/src/hub.rs:241-245`), but the module **identity key is
`(slot, orientation)` in addition to the serial**: `slot` is the USB port path relative to
the base's top hub (abs `3-2-1-4-3` → `1-4-3`; base MCU `1-4-4`; SALON-001), `position` is
the connector index derived from that port path within the connector grid, and
`orientation` mirrors the `deck/device/<serial>/orientation` payload
(`horizontal` = flat `0x01`, `vertical` = upright `0x00`). Every physical connector
exposes a different slot per module rotation, so a reseated or rotated module remaps to
the same logical position instead of appearing as a new device. Control paths hang off
the *kind slot* (`keys`, `dial`, `knobs.0`), not the serial, so any module in any of the
26 connectors stays logically addressed (`/controls/knobs/<i>` etc.); when a module with
a known `(slot, orientation)` key hot-plugs, the daemon re-handshakes (≥ 600 ms settle,
double command) and re-pushes the cached frames/LEDs keyed by position, idempotently.
Two same-kind modules: `keys` mirrors; a second knob/fader/roller set addresses by
serial suffix (`upstream/PROTOCOL.md:130-139`).

## 2. Bindings representation (S2)

`/bindings` is an **array**; each element is a `schemas/binding.json` object with a
daemon-assigned unique `id` (`b0001`, …). `ds get /bindings` lists all;
`ds get /bindings/b0001` reads one; `ds bind <topic> --do …` appends and returns the id;
`ds unbind b0001` (or `--topic` to remove all matching) deletes. Topics keep their `/`
inside the `topic` field — they are never split into path segments.

## 3. Types

Control visual (`image`):

```json
{"kind": "color", "value": "#3cc86e"}
```
One of: `color` (`#rrggbb` only), `frame` (`{target, seq, sha256}` — a reference, never
bytes; changes only on `set`), `file` (absolute path; schema pattern rejects any `..`
segment — M3; daemon additionally canonicalizes and confines to `/policy/fs/roots`),
`icon` (`tabler:<name>`), `text` (`{value ≤ 32 chars, fg, bg}`). All validated by
`schemas/state.json`.

Panel-only, on `/controls/panel/image` only (`$defs/panelVisual` in `schemas/state.json`;
keys and dial keep `screenControl`/`visual`, so the schema rejects it there before the
transaction — DS-111):

```json
{"kind": "colors", "value": ["#rrggbb", "… exactly 15 entries"]}
```

Row-major key colours 0-14. The daemon composes the 480x272 panel server-side —
background `#000000`, each 72x72 key tile filled at x=5,104,203,303,402 / y=1,101,199
(`docs/contracts/streaming.md` §1, `upstream/PROTOCOL.md:81`) — and pushes it as ONE
`02 35` panel frame; no per-key `0x36` reports are sent.

## 4. Durability, transactions, revisions

- Every mutation of the **durable** document is a transaction: `{"ops": […], "if_rev": N}`
  applied atomically; on mismatch the whole transaction fails with `E_CONFLICT` (exit 3)
  and returns `current_rev`. Omitting `if_rev` is last-write-wins.
- Each committed transaction increments `/meta/rev` by 1 and is persisted atomically
  (temp+rename, debounced 250 ms, fsync on stop).
- **Durable:** everything except `/telemetry/*` and `/focus` (ephemeral, in-memory).
  Frame bytes are never durable (ADR-005). Frame pushes never touch this document (S1).
- **Crash recovery:** load `state.json` at start; if unparsable, move to
  `state.json.corrupt-<ts>`, start empty, emit a loud `deck/system` event.

## 5. Subscriptions

`{"op":"subscribe","path":"/controls/*","from_rev":N}` → stream of
`{"op":"event","path":…,"rev":123,"value":…,"prev":…}`. `/telemetry/*` subscriptions get
≤ 1 Hz `{"op":"event","telemetry":true,"value":…}` with **no rev**. Slow subscribers:
events dropped, `{"op":"dropped","count":n}` — the state task never blocks (R2/R5).

## 6. Query / filter semantics

`get <path>` → exact / subtree / one-segment `*` / `/telemetry/...` (no rev).
`{"op":"get","path":…,"fields":[… ]}` projection. Filtering is client-side
(`ds get /controls --json | jq …`) — no jq in the daemon.

**Latency budget (D13, R2/R11):** under a 32 fps frame load on the virtual device,
`get` p50 ≤ 2 ms, p99 ≤ 10 ms; **frame pushes at 32 fps cause zero rev increments and
zero `/controls` subscription events** (S1 bench assertion, docs/30-testing.md §5).
The state lock is an `RwLock` never held across I/O; the frame plane shares no lock with it.

## 7. Conflict rules

- Same-path concurrent transactions: serialized by rev; loser gets `E_CONFLICT`.
- Plugin effect vs user `set` on the same path: effects apply in arrival order, each its
  own rev.
- Device-driven changes (fader value) write `/controls/faders/N/value` + emit an event;
  they never overwrite `image`/`label`.
- Event ingest is transactional (S7): when the router receives `app/focus/changed`, it
  mirrors the payload into `/focus` **and** dispatches bindings in one transaction, so
  `when` on `/focus/app` sees the new value; `when` may also read the event payload
  directly (`{"payload":"app","op":"eq","value":"code"}`). The payload is
  `{app, title, pid?}` — `pid` is optional (DS-114): absent or null is accepted and
  omitted from the mirror; a present non-integer `pid` is rejected with `E_SCHEMA`.

## 8. Worked examples

Deck query: `ds get /controls/keys/0 --json` →
`{"ok":true,"rev":41,"value":{"image":{"kind":"color","value":"#3cc86e"},"label":"Salon"}}`

Telemetry query (no rev): `ds get /telemetry/streams/keys --json` →
`{"ok":true,"value":{"sent":960,"dropped":12,"fps":32.0,"in_flight":true,…}}`

Deck set (transactional): `ds set /controls/keys/0/image --json '{"kind":"color","value":"#ff8800"}' --if-rev 41`
→ `{"ok":true,"rev":42}`; stale `--if-rev 40` → exit 3,
`{"ok":false,"error":{"code":"E_CONFLICT","message":"rev is 42","current_rev":42}}`.

Binding add: `ds bind deck/control/keys.0/down --do 'plugin demo.bump {"by":1}' --json` →
`{"ok":true,"rev":43,"result":{"id":"b0001"}}`

Policy (D18/S13 — Salon-only lights, admin scope): `ds set /policy/net/allow_hosts --json '["lights.lan"]'`.
Enforced at the daemon's single request choke point; core contains zero light-specific code.
