# Component: ds-vdev (virtual device)

A software MasterHub at the HID-report level for CI and development (R10, D16). Port of
`hubdeck/tests/fakes/virtual_hub.py:17-130,196-246` to Rust, exposed as a socket server
so the real `ds-device` code path (chunking, framing, safety) runs unchanged.

- **Language:** Rust. Why: the acceptance suite must drive the real drivers, not a
  parallel model (`hubdeck/CLAUDE.md:101-103` — if real and virtual disagree, fix the
  model).
- **Implements:** VID `0x2516` modules — keys M4315 (`0x01FE`, 320 B out, tag `0x34`),
  dial M131E (`0x0206`, tag `0x44`), and the **full M0 family** (`0x01F6` base,
  `0x0200` faders, `0x0202` knobs, `0x0204` rollers — rollers included explicitly, m7:
  64 B out, tag `0x2A`, input `01 2A … 01 02 r1 r2`) — `docs/discovery/40` §1. Software-mode features `03 3A/49`, backlight
  `03 34/44`, solid `03 36/37/46`, chunked images `02 36/35/45` (312 B chunks, reassembled
  into `frames[target]`), LEDs `02 2C/2D/2E`, input injection (`01 34 … 02 0F bitmap`,
  dial `01 44 …`, M0 `01 2A …`), feature reads incl. serial (`0x30/0x3A/0x4A`),
  fader values `0x33`, orientation `0x35`.
- **Enforces the safety rule too:** refuses forbidden reports with the same
  `is_forbidden` list (`upstream/crates/mh-protocol/src/safety.rs:8-32`) so tests can
  assert the daemon never emits them.
- **Public surface:** `ds-vdev --socket /tmp/vdev.sock [--modules keys,dial,m0]
  [--inject events.ndjson] [--throttle-kbps N] [--drop-after N]`. Inspection is the
  **`ds-vdevctl` subcommands of the same binary** (m10/S10 — there is no test-only verb in
  the production `ds` CLI): `ds-vdevctl --socket S frames <target>` → sha256 + bytes of
  each reassembled frame (the golden-frame oracle); `ds-vdevctl … writes` → full report
  log (bytes, chunk index, timestamps, per-write return counts); `ds-vdevctl … inject
  events.ndjson` → scripted input with timestamps.
- **Failure modes / knobs:** `--throttle-kbps` simulates the ~2.1 MB/s device cap
  (`upstream/PROTOCOL.md:265`) to prove backpressure; `--drop-after` simulates
  hot-unplug; `--inject` replays input events with timestamps.
- **Dependencies:** ds-protocol (mh-protocol) for report building/parsing.
- **Test command:** `cargo test -p ds-vdev`; the conformance suite
  (`python tests/conformance/run.py`) starts ds-vdev itself.
- **Minimum working example:**
  ```sh
  ds-vdev --socket /tmp/vdev.sock &
  ds daemon start --device virtual:/tmp/vdev.sock
  ds set /controls/keys/0/image --file examples/key.png
  ds-vdevctl --socket /tmp/vdev.sock frames key:0    # sha256 of the reassembled frame
  ds-vdevctl --socket /tmp/vdev.sock writes | tail -3
  ```
