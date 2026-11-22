# Component: event router / rule engine (in-daemon module)

Turns input events and external emits into binding steps. Contract:
`docs/contracts/actions.md`. Design: ADR-007, D07.

- **Language:** Rust, inside ds-daemon.
- **Topics:** `deck/control/<control>/<gesture>` (gestures: `down|up|long|double|rotate|slide|value`),
  `deck/device/<serial>/<event>` (`connected|disconnected|orientation|faders`),
  `deck/system/<event>` (`start|stop|rev-changed|crash-recovered`),
  `app/<sender>/<topic>` (any external program via `ds emit app/focus …`).
- **Routing model:** bindings live in state at `/bindings/<control>/<gesture>` = list of
  steps (see `schemas/binding.json`). On an event the router looks up bindings for the
  topic (exact, then wildcard `deck/control/*`), evaluates each step's optional `when`
  (`{path, op: eq|neq|exists|gt|lt, value}`), and executes steps sequentially.
- **Core vocabulary (closed, small, boring):** `set`, `emit`, `plugin`, `exec`, `sleep`,
  plus `when` composition. `exec` is argv-only and requires the `admin` token scope AND
  `capabilities: ["exec"]` on the binding — no shell anywhere (the old `shell=True` at
  `hubdeck/src/hubdeck/platforms/posix.py:70` and `{python: …}` exec at
  `hubdeck/src/hubdeck/plugins/python.py:58` are the rejected patterns, R8).
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
  ds bind /controls/keys/0/down --do 'plugin demo.bump {}' --json
  ds bind /controls/keys/1/down --do 'set /controls/keys/1/color #ff0000' --do 'emit demo.red'
  ds emit app/demo/hello '{"n":1}' --json
  ds watch deck/control --ndjson
  ```
