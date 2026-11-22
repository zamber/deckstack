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
  1. One writer to the durable state document (the daemon's state task); all durable
     mutations bump `rev`. Frame pushes and `/telemetry` sampling never do (S1).
  2. The state lock is never held across HID I/O or process spawns.
  3. No code path can `eval` config or spawn a shell; `exec` steps are argv-only,
     argv[0] absolute + in `/policy/exec/allow` − `/policy/exec/deny` (shells pre-denied),
     admin scope, fire-and-forget with rlimits (S3/S11, ADR-007/009).
  4. No raw HID report leaves without passing `mh_protocol::safety::is_forbidden`
     (`upstream/crates/mh-protocol/src/safety.rs:34-36`) and the `/policy` check.
  5. Never send backlight 0 to the dial — screens-off = black JPEG
     (`upstream/PROTOCOL.md:61`).
  6. Frame queues are bounded (1 in flight + 1 waiting per module); overflow drops, never
     blocks; the in-flight frame is never dropped mid-send (S16/M8). Telemetry is sampled
     ≤ 1 Hz into non-durable `/telemetry/*`.
  7. Token revocation is instant for new connections; ≤ 5 s grace only for in-flight
     requests on an established connection (connection-generation counter, S12).
  7. Plugin effects are schema-validated before touching state.
- **Failure modes:** socket missing → CLI exits 6 (`E_UNREACHABLE`); state file corrupt →
  start with empty state + `last-known-good` kept as `state.json.corrupt-<ts>`, log loudly;
  device busy (vendor app running) → status says so; plugin crash → respawn ≤3/10 min;
  OOM guard → frame plane drops first, then refuses new streams, never kills control plane.
- **Dependencies:** `ds-protocol` (re-exported mh-protocol), `ds-device` (mh-device),
  `ds-state`, `ds-ipc`, `ds-platform`.
- **Test command:** `cargo test -p ds-daemon` (unit, with ds-vdev transport injected);
  `python tests/conformance/run.py --virtual` (black-box via the real socket).
- **Device task — image visual pushes (DS-095, phase 1):**

  | State path | Target | HID image cmd |
  |---|---|---|
  | `/controls/keys/image` | keys panel (480×272) | output `02 35`, index 0 |
  | `/controls/keys/<0-14>/image` | key N (72×72) | output `02 36`, index N |
  | `/controls/dial/image` | dial screen (240×240) | output `02 45` |

  A committed change of one of those paths with a displayable visual pushes **image bytes**
  to the device as chunked image reports (`02 <cmd> <chunk u16 LE> <len u16 LE> <index>
  <last> <≤312 B>` inside 320 B output reports — `docs/contracts/streaming.md` §2), built
  with the existing ds-protocol chunkers (`panel_image`/`key_image`/`dial_image`); there is
  no second chunker. Writes are paced by the per-device rate limit (LED writes at
  `/policy/led_rate_per_s`, image chunks at the device's ~2.1 MB/s cap).

  - `{"kind":"file","path":…}` — the file's bytes are read on the device task and pushed
    **unchanged** (phase-1 liberty, see caveat below). The read is confined to
    `/policy/fs/roots` (read from the persisted state document; the state task forwards
    only `/controls/*` pushes, and ds-state persists synchronously on every commit) plus
    the system temp dir; paths are canonicalized and a file outside the roots is refused
    with a logged error — never a panic. Size cap: 16 MiB (`MAX_IMAGE_FILE_BYTES`, the
    documented wire cap for image payloads, streaming.md §2/§5); larger files are refused
    the same way.
  - `{"kind":"color","value":"#rrggbb"}` — a minimal solid-colour PNG (8×8 RGB, std-only
    encoder in `crates/ds-daemon/src/device.rs`) is pushed. Phase-1 placeholder: the real
    per-target wire format is baseline JPEG 4:4:4 (DS-030).

  **Phase-1 caveat:** file bytes are pushed unchanged and colours as the placeholder PNG —
  the device wire format is baseline JPEG only (streaming.md §2). JPEG baseline 4:4:4
  re-encode is DS-030 (phase 3) and animation is DS-034; from phase 3 on, the golden sha
  observed by `ds-vdevctl frames <target>` is the sha of the *encoded* frame, not of the
  source file.

  **Non-blocking:** pushes travel over a bounded channel (overflow drops and is counted);
  the file read and all chunk writes happen on the device task's thread — the state lock is
  never held across them, and a failed push is logged and dropped, so it never stalls the
  state task or IPC. LED colours on knobs/faders/rollers stay feature reports (`led_color`).
  Icons, text and `kind:frame` references remain out of scope (logged only).
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
