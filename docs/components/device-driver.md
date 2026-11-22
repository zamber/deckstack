# Component: ds-device (device driver crate)

HID transport + protocol for the MasterHub family. Thin wrapper over the reused
`mh-protocol` / `mh-device` crates (ADR-010, D03).

- **Language:** Rust. Why: the load-bearing code already exists and is hardware-verified
  (`upstream/PROTOCOL.md` "Verified here" column). Rejected: fresh layer, vendored fork —
  see ADR-010. Licence path: upstream is `MIT OR Apache-2.0` (`upstream/Cargo.toml:8`);
  deckstack is dual-licensed the same way and carries a `NOTICE.md` entry; we depend via
  git and contribute fixes upstream instead of forking.
- **What ds-device adds on top of mh-device:** the virtual transport (ds-vdev backend) so
  the same driver code runs against an in-memory HID fake; the policy choke point
  (`/policy` checks before any write); per-module writer threads with latest-frame-wins
  queues (mirroring `upstream/app/src-tauri/src/engine/mod.rs:1303-1335`); handback on
  exit (`upstream/crates/mh-device/src/module.rs:227-240`).
- **Public surface (Rust API, stable within the daemon crate):**
  `Hub::start(transport) -> Hub`, `hub.events() -> Stream<DeviceEvent>`,
  `module.send_frame(target, jpeg) `, `module.set_led(zone, idx, mode, rgb, bright)` (always
  save=0), `module.backlight(pct)` (refuses 0 on dial), `module.query(Query)` (software
  mode only), `hub.release_all()` (handback).
- **Invariants (from hardware facts, docs/discovery/40):**
  1. Forbidden commands are unrepresentable: builders only for safe commands + raw-path
     `is_forbidden` check (`upstream/crates/mh-protocol/src/safety.rs:8-36`,
     `report.rs:28-30`). Firmware/serial/factory/reset/recovery bytes can never be encoded.
  2. Software mode first: keys `03 3A 01`, dial `03 49 01`, ≥600 ms settle, double command
     before first read on macOS (`upstream/PROTOCOL.md:42,60,197`).
  3. Images: baseline JPEG only, standard Huffman tables, 4:4:4 chroma, chunked
     `02 <cmd> <chunk u16 LE> <len u16 LE> <idx> <last> <≤312 B>` (`upstream/PROTOCOL.md:87-96`,
     `crates/mh-protocol/src/report.rs:132-171`). cmds: keys `0x36`, panel `0x35`, dial `0x45`.
  4. LED writes `02 2C/2D/2E <target> <idx> … save=0` always (`upstream/PROTOCOL.md:34-38`);
     rate cap 30/s pending soak test (OPEN O2, `upstream/PROTOCOL.md:118-121`).
  5. SET_IDLE sent at connect on macOS so rollers speak (`upstream/PROTOCOL.md:40`).
  6. Reads on keys/dial only in software mode (`Error::HardwareMode`,
     `upstream/crates/mh-device/src/module.rs:158-166`); identify modules by USB path/serial,
     never USB string (`upstream/PROTOCOL.md:208-211, 272-273`).
  7. Pipelined writes where the OS allows (8 in flight: `upstream/crates/mh-device/src/overlapped.rs`),
     dedicated writer handle per module; SET_REPORT on the base hangs 5 s — never used
     (`upstream/PROTOCOL.md:116-118`).
  8. Rescan ≤1 s busy / 15 s idle; full re-push on reconnect; keys back ≤5 s target
     (`upstream/PROTOCOL.md:172-177`).
- **Failure modes:** read error ⇒ module removed + `Disconnected` event; rescan re-inits.
  Write error ⇒ queue dropped, counted in `/streams/<target>/stats`, module redrawn.
  Forbidden report ⇒ `Err(Forbidden)`, logged, never retried.
- **Dependencies:** `mh-protocol`, `mh-device` (git deps), `hidapi` (libusb static on
  Linux, `upstream/crates/mh-device/Cargo.toml:17-20`), `ds-platform` for location paths.
- **Test command:** `cargo test -p ds-device` with the virtual transport asserting exact
  report bytes (port of `hubdeck/tests/fakes/virtual_hub.py:17-130,196-246`); hardware:
  `DS_HARDWARE=1 cargo test -p ds-device -- --ignored` (smoke suite, docs/30-testing.md).
- **Minimum working example:**
  ```rust
  let hub = ds_device::Hub::start(ds_device::Transport::Virtual("/tmp/vdev.sock".into()))?;
  let keys = hub.module("keys").await?;                 // by kind, first unit
  keys.software_mode().await?;                          // 03 3A 01, settle 600 ms
  keys.send_frame(FrameTarget::Panel, &jpeg_bytes).await?; // chunked 02 35
  keys.set_solid(3, 0x3c, 0xc8, 0x6e).await?;           // 03 37 idx R G B
  ```
