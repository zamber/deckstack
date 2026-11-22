# Component: event router / rule engine (in-daemon module)

Turns input events and external emits into binding steps. Contract:
`docs/contracts/actions.md`. Design: ADR-007, D07.

- **Language:** Rust, inside ds-daemon.
- **Topics:** `deck/control/<control>/<gesture>` (gestures: `down|up|long|double|rotate|slide|value`),
  `deck/device/<serial>/<event>` (`connected|disconnected|orientation|faders`),
  `deck/system/<event>` (`start|stop|rev|binding-error|exec|crash-recovered`),
  `daemon/<event>` (`start|stop|reload` — the daemon lifecycle namespace),
  `app/<sender>/<topic>` (any external program via `ds emit app/focus …`),
  `plugin/<id>/<event>` (reserved, phase 5).
- **The bus (DS-040, implemented):** `crates/ds-daemon/src/router/` —
  `topics.rs` (topic grammar, validation, wildcard matching), `bus.rs` (the event bus:
  bounded per-subscriber queues, drainer threads, drop-oldest backpressure), `mod.rs`
  (`Pattern` — state-path vs topic patterns — and the device-input decoder).
  Producers: the state task publishes one state event per changed path per committed
  revision (`{"op":"event","path":…,"rev":…,"value":…,"prev":…}` on the wire, exactly the
  historical shape); the device task publishes decoded input reports as
  `deck/control/<control>/<gesture>` plus `deck/device/<serial>/connected|disconnected`;
  the daemon loop publishes `daemon/start`/`daemon/stop`; the IPC `emit` op publishes
  `app/<sender>/<topic>` (schema restricts emit topics to `^(deck|app)/`, the router
  additionally validates the full grammar). Frames and `/telemetry/*` never enter the
  bus (S1); `/telemetry/*` subscriptions are served directly by the state store.
- **Subscriptions on the wire:** `subscribe`/`unsubscribe` take a `path`. If its first
  segment is a topic root (`deck`/`app`/`daemon`/`plugin`) it is a **topic pattern**
  (`/deck/control/keys/*` → `deck/control/keys/*`), and topic events arrive as
  `{"op":"event","topic":…,"path":"/<topic>","ts_ms":…,"payload":…}`; otherwise it is a
  **state-path pattern** with the unchanged ds-state matching semantics and wire shape.
  `unsubscribe` removes the calling connection's subscription for that pattern.
- **Wildcard rules:** in topics `/` and `.` both separate segments; a pattern matches
  when it is a segment-wise prefix of the topic and `*` matches exactly one segment —
  `deck/control/keys/*` matches `deck/control/keys.0/down`, `deck/*` matches all `deck/…`,
  `*` matches everything. State-path patterns use the same prefix rule, `/`-only.
- **Dispatch order:** exact patterns first, then wildcard patterns, subscription order
  within each group; FIFO per subscriber (one event per committed revision for state
  events).
- **Backpressure:** each subscriber has a bounded queue (cap 1000, ipc.md §5) drained by
  its own thread. `publish` never blocks and never writes sockets: on overflow the oldest
  event is dropped and counted, a `warn` log line is emitted (`…queue full (1000 queued):
  dropped oldest event … — total dropped N`), and the client receives
  `{"op":"dropped","path":"<pattern>","count":n}`. A slow subscriber therefore can never
  stall the state task or the device reader; a dead connection's subscription removes
  itself on the next failed send. Proven by `tests/conformance/events/` (case 070).
- **Routing model (S2):** bindings are elements of the `/bindings` **array**
  (`schemas/binding.json`, each with `id` `bNNNN`). On an event the router looks up
  bindings by topic (exact, then `*` wildcards), evaluates each binding's optional `when`
  against **state** (`path`) **or the event payload** (`payload` field, S7), and executes
  steps sequentially. (Binding storage/execution lands with DS-041/DS-042; the lookup,
  ordering and bus are DS-040.)
- **Atomic event ingest (S7):** receiving `app/focus/changed` mirrors the payload into
  `/focus` and dispatches bindings in ONE transaction — `/focus`-based `when` conditions
  always see the value that triggered them.
- **Core vocabulary (closed, small, boring):** `set`, `emit`, `plugin`, `exec`, `sleep`,
  plus `when` composition. `exec` is fire-and-forget (S11), argv-only, argv[0] absolute
  (schema) AND in `/policy/exec/allow` AND not in `/policy/exec/deny` (pre-seeded with
  shells/interpreters — S3: no-shell is enforced, not asserted), plus admin scope +
  `allow_exec: true`. The old `shell=True` (`hubdeck/.../platforms/posix.py:70`) and
  `{python: …}` exec (`hubdeck/.../plugins/python.py:58`) remain the rejected patterns (R8).
- **Why the core stays a router:** both current implementations hardcode the vocabulary
  (`upstream/.../engine/actions.rs:105` catalog; `hubdeck/.../plugins/__init__.py:22-28`
  + `ALIASES` at `config/schema.py:55-67`). deckstack instead lets ANY program subscribe
  to `deck/control/…` or `app/…` and act via the CLI — the catalog is the ecosystem, not
  the core (R3/R4).
- **Latency budget:** event → router dispatch p99 ≤ 5 ms; step execution `set`/`emit`
  p99 ≤ 10 ms; `plugin` invoke round-trip ≤ 2 s timeout (effects applied after);
  `exec` fire-and-forget with pid tracked. The bus keeps publish O(subscribers) under one
  short lock with zero I/O, so the budget holds under subscriber load; budgets asserted
  by `cargo bench -p ds-bench -- event-latency` (DS-045).
- **Failure modes:** binding step fails → error event on `deck/system/binding-error`,
  remaining steps still run; `when` path missing → treated as `exists: false`;
  runaway recursion (`emit` → binding → `emit` same topic) capped at depth 10 (precedent:
  flow depth cap 10, `hubdeck/src/hubdeck/engine.py:1088-1112`).
- **Dependencies:** ds-state, ds-ipc, ds-device/ds-protocol (input decoding), plugin host
  (for `plugin` steps).
- **Test command:** `cargo test -p ds-daemon` (router unit + in-process socket tests),
  black-box `python3 tests/conformance/events/run.py` (the 7 router cases on real
  binaries incl. `ds-vdevctl inject`), conformance `tests/conformance/run.py`.
- **Minimum working example:**
  ```sh
  ds bind deck/control/keys.0/down --do 'plugin demo.bump {"by":1}' --json     # → {"id":"b0001"}
  ds bind deck/control/keys.1/down --do 'set /controls/keys/1/image {"kind":"color","value":"#ff0000"}' --do 'emit app/demo/red {}'
  ds bind app/focus/changed --when '{"payload":"app","op":"eq","value":"code"}' --do 'set /controls/keys/3/image {"kind":"color","value":"#00ff00"}'
  ds emit app/demo/hello '{"n":1}' --json
  ds watch deck/control --ndjson        # --json here is an error (exit 1, S9)
  ```
