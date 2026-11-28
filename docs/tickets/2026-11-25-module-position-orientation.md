# Ticket DS-112 — map modules by position and orientation, not serial alone

Date: 2026-11-25. Type: feature ticket (docs-only at filing; implementation later).
Scope guard: documentation-only ticket — no daemon code, schemas, contracts, or tests
change with this file.

**Status: IMPLEMENTED** on branch `vibe/impl-ds-112-position-orientation` (ADR-021,
decision-log D42). Identity key `(slot, orientation)` + serial; `/devices/<serial>` carries
`position` + `orientation`; rescan re-handshakes (≥ 600 ms settle, double command) and
re-pushes cached frames/LEDs keyed by position; ds-vdev/ds-vdevctl gained
`slots`/`move`/`orientation`/`replug` + the permutation harness
(`crates/ds-vdev/tests/ds112_permutations.rs`); conformance suite
`tests/conformance/position/run.py`. Hardware replug/reseat case for `tests/hw/smoke.py`
remains open (no hardware in this phase).

## Symptom

Reseating the knob module (or rotating the base between flat and upright) makes it
appear as a different device: the module keeps its USB serial, but the host re-derives
its identity from the wrong signal, so the app sees a "new" serial and the old
`/devices/<serial>` subtree — including its LED state and visuals — stops responding.
Observed on the predecessor stack: the Salon app hung until the daemon restarted
because the device task wedged waiting on the half-reconnected module. The base
orientation switch is a one-direction tilt sensor (flat = `01`, upright = `00`), so
rotating the base changes the logical layout (4×6 vs 6×4 grid) without any USB event.

> Note (DS-120, fixed on `vibe/impl-ds-120-no-restart-on-hotplug`): the 0.1.0 alpha also
> fully restarted the daemon on any module unplug (salon pids 824920 → 1356741 →
> 1415680 → 1487691; `docs/discovery/salon-001/README.md` §4) — a known defect of the
> current alpha, not of this design. With DS-120 the reconnect path re-handshakes and
> re-pushes within the ≤ 5 s budget without any process restart (ADR-021), so the
> procedure below can assume the daemon survives a reseat/rotation.

## Root cause (from the pinned upstream facts)

- Module identity in `docs/10-architecture.md` §5 is USB serial read at connect
  (`upstream/crates/mh-device/src/hub.rs:241-245`), with USB port path as fallback
  (`upstream/PROTOCOL.md:107`). Placement/rotation of a module is the USB port path
  relative to the base's top hub (`1-4-3` style; `upstream/PROTOCOL.md:107-127`); any
  module fits any of the 26 connectors (`upstream/PROTOCOL.md:109-127`).
- Base orientation: input tag `0x2B` payload `01`/`00`, read via feature `0x35` →
  `35 00 vv` (`upstream/PROTOCOL.md:33,106`; consolidated in
  `docs/discovery/40-hardware-and-protocol.md` §3). The daemon already decodes this
  into `deck/device/<serial>/orientation` (`crates/ds-daemon/src/router/mod.rs:283`,
  `crates/ds-device/src/hid/reader.rs`), but nothing consumes it for identity or layout.
- `schemas/state.json` `$defs.device` already carries `slot` ("USB port path, e.g.
  1-4-3") but no `position`/`orientation` fields; the daemon writes `connection` and
  `last_seen_rev` only.

## Requirements

1. **Map modules by USB port path + base orientation, not serial alone.** Extend the
   device identity key to `(slot, orientation)` in addition to serial, so a reseated or
   rotated module remaps to its logical position instead of appearing as a new device.
   Source files: `crates/ds-device/src/hid/enumerate.rs` (sysfs port path; Linux has no
   locationID — `upstream/crates/mh-device/src/location.rs:144-149` TODO),
   `crates/ds-device/src/hotplug.rs`, `crates/ds-daemon/src/device_task.rs`.
2. **Expose `/devices/<serial>/position` and `/devices/<serial>/orientation`** in the
   state document (schema change in `schemas/state.json` `$defs.device`, contract change
   in `docs/contracts/state.md`, same commit — per AGENTS.md invariant). `orientation`
   mirrors the existing `deck/device/<serial>/orientation` event payload; `position`
   is the connector index derived from the port path within the 26-connector grid.
3. **Re-handshake and re-push visuals on hot-plug.** On any rescan hit for a known
   `(slot, orientation)` pair: software-mode handshake with ≥600 ms settle and double
   command before first read (`upstream/PROTOCOL.md:197`), then full re-push of frames
   + LEDs from the cached per-target state (the reconnect path already exists —
   `docs/10-architecture.md` §5; assert it is idempotent and keyed by position).
4. **Support any module in any connector.** Control addressing stays logical
   (`/controls/knobs/<i>` etc.); only the physical mapping changes. The 26-connector
   fact (`upstream/PROTOCOL.md:109-127`) must hold for all six module kinds.
5. **Add a vdev + hardware test.** vdev: extend `ds-vdev`/`ds-vdevctl` so a module can be
   "moved" (slot change) and replugged, asserting the daemon re-handshakes, re-pushes
   the latest frame (intact sha256 per `ds-vdevctl frames`), and updates
   `/devices/<serial>/position` without an app restart (`docs/components/virtual-device.md`,
   `--drop-after` precedent). Hardware: extend `tests/hw/smoke.py` replug step with a
   reseat/rotation case (udev unbind/bind of a different port, or base rotation).
6. **Keep the app's `knobs.roles` config as the temporary escape hatch** until position
   mapping lands: the predecessor app pins knob roles by hand
   (`hubdeck` config `knobs.roles`); document it in `docs/70-porting-notes.md` as the
   interim workaround for the Salon deck, not a deckstack feature.

## Acceptance

A reseat or rotation of a module remaps it to the same logical position
(`/devices/<serial>/position` + `orientation` updated), the daemon re-handshakes and
re-pushes its LEDs/frames within the ≤5 s reconnect budget
(`upstream/PROTOCOL.md:172-177`), and the app keeps working without a restart.

## References

- `upstream/PROTOCOL.md:33,106-127,172-177,197,208-211` (orientation, slots, hot-plug,
  identity) as consolidated in `docs/discovery/40-hardware-and-protocol.md` §1, §3, §7, §9.
- `docs/10-architecture.md` §5 (hot-plug/reconnect, identity by serial).
- `crates/ds-device/src/hid/enumerate.rs`, `crates/ds-device/src/hotplug.rs`,
  `crates/ds-device/src/hid/reader.rs` (orientation decode),
  `crates/ds-daemon/src/device_task.rs`, `crates/ds-daemon/src/device.rs`,
  `crates/ds-daemon/src/router/mod.rs:283`.
- `schemas/state.json` `$defs.device`; `docs/contracts/state.md`; `docs/90-verification.md`
  (replug step of `tests/hw/smoke.py`); `docs/components/virtual-device.md`.
