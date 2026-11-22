# Component: ds-daemon

The single long-lived process. Owns the state document, all hardware I/O, the plugin
host, the event router, the frame plane. Details: `docs/10-architecture.md`,
`docs/contracts/state.md`, `docs/contracts/ipc.md`.

- **Language:** Rust (edition 2024). Why + rejected alternatives: ADR-001. Deps (boring,
  well-supported): `tokio` (or std threads — decide in phase 1 prototype, default tokio for
  the socket server), `serde`/`serde_json`, `hidapi` via `mh-device` (ADR-010), `jsonschema`
  for validating messages against `schemas/`, `tracing` for logs.
- **Binary layout:** one binary `ds-daemon` with subcommands `run` (default), `--guard`,
  `--version`. Modules in-crate: `state`, `ipc`, `router`, `frames`, `plugins`, `device`,
  `platform` (shim), `persist`.
- **Public surface:** the IPC contract (`docs/contracts/ipc.md`): NDJSON control endpoint +
  binary stream endpoint + optional WS/HTTP adapter; plugin host endpoints (internal);
  state schema (`schemas/state.json`).
- **Invariants:**
  1. One writer to the state document (the daemon's state task); all mutations bump `rev`.
  2. The state lock is never held across HID I/O or process spawns.
  3. No code path can `eval` config or spawn a shell; `exec` steps are argv-only and
     require the `admin` scope (ADR-007/009).
  4. No raw HID report leaves without passing `mh_protocol::safety::is_forbidden`
     (`upstream/crates/mh-protocol/src/safety.rs:34-36`) and the `/policy` check.
  5. Never send backlight 0 to the dial — screens-off = black JPEG
     (`upstream/PROTOCOL.md:61`).
  6. Frame queues are bounded (1 in flight + 1 waiting per module); overflow drops, never blocks.
  7. Plugin effects are schema-validated before touching state.
- **Failure modes:** socket missing → CLI exits 6 (`E_UNREACHABLE`); state file corrupt →
  start with empty state + `last-known-good` kept as `state.json.corrupt-<ts>`, log loudly;
  device busy (vendor app running) → status says so; plugin crash → respawn ≤3/10 min;
  OOM guard → frame plane drops first, then refuses new streams, never kills control plane.
- **Dependencies:** `ds-protocol` (re-exported mh-protocol), `ds-device` (mh-device),
  `ds-state`, `ds-ipc`, `ds-platform`.
- **Test command:** `cargo test -p ds-daemon` (unit, with ds-vdev transport injected);
  `python tests/conformance/run.py --virtual` (black-box via the real socket).
- **Minimum working example (phase 1):**
  ```sh
  ds-vdev --socket /tmp/vdev.sock &                 # virtual MasterHub
  ds daemon start --device virtual:/tmp/vdev.sock    # or: systemctl --user start ds-daemon
  ds set /controls/keys/0/color '#3cc86e'
  ds get /controls/keys/0 --json
  ds daemon status --json
  ```
  `daemon start` uses `systemctl --user` when the unit is installed, else spawns a
  detached child with a pidfile (ADR-011/D11); `daemon stop` is deterministic either way.
