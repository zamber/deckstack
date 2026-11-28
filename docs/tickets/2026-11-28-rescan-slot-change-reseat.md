# Ticket DS-130 — daemon rescan must detect a reseat (same serial, new slot)

Date: 2026-11-28. Type: daemon bug fix. Status: fixed in `crates/ds-daemon/src/device_task.rs`
(regression test `deck_set_changed_sees_a_reseat_to_another_connector`).

## Symptom (live deck, SALON-003)

After reseating the knobs module (it physically glows), the daemon kept reporting it
`disconnected` at the old slot (`1-4-3`) while sysfs showed it attached at a new port
path (`3-2.3.4` → slot `3-4`). LED pushes failed with "module knobs not connected on
this transport" (15k+ journal errors). salon-deck never saw the reattachment.

## Root cause

`periodic_rescan`'s hid enumeration comparison keyed modules by `ModuleId` = serial
only. A reseat keeps the USB serial but changes the port path (slot), so
`now == current` and the "deck changed → reconnect" branch never fired. The DS-112
identity key is `(slot, orientation)` — the comparison must include the slot.

## Fix

`deck_set_changed()` compares `(ModuleId, Option<Slot>)` pairs: enumerated
`(serial, slot)` from sysfs vs. the connect-time identity keys plus slot-less open
modules. Any mismatch (reseat, unplug, new module) triggers `on_gone` → reconnect.

## Verification

- `cargo test -p ds-daemon --lib`: 221 passed, incl. the new regression test (same
  serial at a new slot ⇒ changed; unplug ⇒ changed; new module ⇒ changed; unchanged
  and slot-less fallback ⇒ no change).
- Live: after deploying the fixed build, `/preview/api/state` shows
  `knobs 3-4 connected`; the preview webapp renders it at top-right (4,0) per the
  DS-122 matrix. Future reseats are caught by the periodic rescan without a restart.

## Links

DS-112, DS-120, SALON-003, DS-126 (webapp reconnect rendering).
