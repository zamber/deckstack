# ADR-021 — module identity by (slot, orientation), not serial alone (DS-112)

Status: accepted (implemented on branch `vibe/impl-ds-112-position-orientation`).
Deciders: ticket DS-112 (`docs/tickets/2026-11-25-module-position-orientation.md`) with
the SALON-001 hardware evidence (`docs/discovery/salon-001/`).

## Context

SALON-001 (2026-11-27) proved on the salon deck that **each physical connector exposes a
different USB slot per module rotation** — slot = USB port path relative to the base's
top hub (abs `3-2-1-4-3` → `1-4-3`; base MCU `1-4-4`) — and that the base orientation is a
one-direction tilt switch (flat = `01` horizontal, upright = `00` vertical, ~2 reports/s)
that transposes the logical grid 6x4 ↔ 4x6 **without any USB event**. On the predecessor
stack, reseating a module or rotating the base made it appear as a different device and the
app hung until the daemon restarted, because identity was derived from the USB serial read
at connect alone. The 0.1.0 alpha also left `/devices` empty (every unplug wedged/restarted
the daemon), so there was no position/orientation state surface at all.

## Decision

- **Identity key = `(slot, orientation)` in addition to the serial** (`DeviceKey` in
  `crates/ds-device/src/slot.rs`). `Slot` parses/validates the port path and derives a
  collision-free **connector position** (`4^n + Σ (d_i−1)·4^(n−i)` over the 1-based port
  digits; e.g. `1-4-3` → 78, `4-4` → 31, base `1-4-4` → 79). On `hid:` the slot comes from
  the sysfs-style hidapi path (Linux has no locationID); on `virtual:` from the vdev's slot
  feature read (`0x39`, a vdev-only answer — real HID never reads it).
- **`/devices/<serial>` gains `position` and `orientation`** (`schemas/state.json`
  `$defs.device` + `docs/contracts/state.md`, same commit): a device registry thread
  subscribes to `deck/device/<serial>/{connected,moved,orientation,disconnected}` and
  upserts the entries through the state task — the single writer, normal rev semantics.
- **Hot-plug keyed by position, idempotent**: the rescan (≤ 1 s busy) compares the live
  `(slot, orientation)` keys with the keys recorded at connect; a moved module triggers a
  re-handshake (≥ 600 ms settle + double command, per module, in parallel) and a full
  re-push of the latest-frame-wins cache (frames + LEDs), exactly like a reconnect after a
  real unplug. The base orientation is additionally re-read from feature `0x35` on every
  rescan tick, so a rotation whose broadcast was lost in a reconnect window is recovered
  within one tick (the hardware heartbeats ~2/s the same way).
- **Any module in any connector stays logically addressed**: control paths hang off the
  kind slot (`/controls/knobs/<i>` …), never the serial or position, so the 26-connector
  fact holds for all six module kinds; the permutation harness proves it.
- **vdev models the permutations**: `ds-vdevctl slots|move <module> <slot>|orientation
  <h|v>|replug` (control socket only — no host→device raw path; safety suite extended
  accordingly). `move` changes the slot in place (same serial/pid), `replug` drops all host
  connections, `orientation` broadcasts the base tilt INPUT frame. Default slots are the
  SALON-001 per-module rot0 anchors.

## Alternatives

- Keep serial-only identity and pin knob roles by hand (`knobs.roles` escape hatch) —
  rejected as the primary mechanism: it is exactly what made the Salon app hang; it stays
  documented in `docs/70-porting-notes.md` as the interim workaround only.
- Derive position from a physical-connector lookup table — rejected: the same connector
  yields different slots per rotation, so no slot→connector table can be total; the
  arithmetic decode is total, collision-free and needs no table.
- Replay the orientation broadcast from the vdev — rejected: the hardware heartbeats, so
  polling feature `0x35` on the rescan tick is the faithful model and covers reconnect
  windows.

## Evidence

- `docs/discovery/salon-001/README.md` + `connector-map-*.csv` (slot tables, rot0 anchors,
  orientation bytes); `docs/discovery/40-hardware-and-protocol.md` §9 (26 connectors,
  port-path placement, base grid by orientation).
- `cargo test -p ds-device` (slot/position/DeviceKey), `cargo test -p ds-vdev --test
  ds112_permutations` (282-case matrix + move/replug sha), `cargo test -p ds-daemon --lib`
  (placement-change reconnect + re-push unit test), `python3
  tests/conformance/position/run.py` (black-box: /devices updates, moved/orientation
  events, frame sha intact after re-push, daemon pid unchanged).
