# Component: event router / rule engine (in-daemon module)

Turns input events and external emits into binding steps. Contract:
`docs/contracts/actions.md`. Design: ADR-007, D07.

- **Language:** Rust, inside ds-daemon.
- **Topics:** `deck/control/<control>/<gesture>` (gestures: `down|up|long|double|rotate|slide|value`),
  `deck/device/<serial>/<event>` (`connected|disconnected|orientation|faders`),
  `deck/system/<event>` (`start|stop|rev|binding-error|exec|crash-recovered`),
  `app/<sender>/<topic>` (any external program via `ds emit app/focus …`).
- **Routing model (S2):** bindings are elements of the `/bindings` **array**
  (`schemas/binding.json`, each with `id` `bNNNN`). On an event the router looks up
  bindings by topic (exact, then `*` wildcards), evaluates each binding's optional `when`
  against **state** (`path`) **or the event payload** (`payload` field, S7), and executes
  steps sequentially.
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
  `exec` fire-and-forget with pid tracked. Budgets asserted by
  `cargo bench -p ds-bench -- event-latency`.
- **Failure modes:** binding step fails → error event on `deck/system/binding-error`,
  remaining steps still run; `when` path missing → treated as `exists: false`;
  runaway recursion (`emit` → binding → `emit` same topic) capped at depth 10 (precedent:
  flow depth cap 10, `hubdeck/src/hubdeck/engine.py:1088-1112`).
- **Dependencies:** ds-state, ds-ipc, plugin host (for `plugin` steps).
- **Test command:** `cargo test -p ds-daemon::router`; conformance
  `tests/conformance/bindings/` (the two chains in docs/contracts/actions.md §6).
- **Minimum working example:**
  ```sh
  ds bind deck/control/keys.0/down --do 'plugin demo.bump {"by":1}' --json     # → {"id":"b0001"}
  ds bind deck/control/keys.1/down --do 'set /controls/keys/1/image {"kind":"color","value":"#ff0000"}' --do 'emit app/demo/red {}'
  ds bind app/focus/changed --when '{"payload":"app","op":"eq","value":"code"}' --do 'set /controls/keys/3/image {"kind":"color","value":"#00ff00"}'
  ds emit app/demo/hello '{"n":1}' --json
  ds watch deck/control --ndjson        # --json here is an error (exit 1, S9)
  ```
