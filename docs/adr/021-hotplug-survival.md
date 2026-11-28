# ADR-021 — the daemon survives module hot-unplug (no process restart)

Status: accepted (ticket DS-120, 2026-11-27, SALON-001 evidence). Implemented on branch
`vibe/impl-ds-120-no-restart-on-hotplug`.

## Context

SALON-001 (`docs/discovery/salon-001/README.md` §4) recorded that on the 0.1.0 AppImage
every module unplug fully restarted `ds-daemon` — the PID changed
(824920 → 1356741 → 1415680 → 1487691 across one session) and all WS/IPC connections
dropped with `E_INTERNAL: connection closed by daemon`. The installed unit
(`packaging/systemd/deckstack.service`) sets `Restart=on-failure`, so a full crash is
silently masked as a "restart", but a restart still blows the ≤ 5 s reconnect budget of
`docs/90-verification.md` and reproduces the predecessor stack's "Salon app hung until
the daemon restarted" failure class.

Root cause in this tree (reproduced on ds-vdev, `ds-vdevctl throttle off 1` + a write):
the daemon's device loop (`crates/ds-daemon/src/device.rs::run_loop`, the path
`daemon::run` actually spawns) treated a `DeviceEvent::Disconnected` or any
non-timeout transport error as fatal for the task — `break 'outer` logged
`device task stopped` and the device thread exited permanently. No reconnect, no
re-push, no further `connected` event; the deck was dead until the whole daemon was
restarted (which is what the salon session did by hand and what the unit's `Restart=`
masks on hardware). The resilient loop in `crates/ds-daemon/src/device_task.rs` existed
but was not wired into `daemon::run`.

## Decision

- **Per-device errors never stop the daemon.** The device loop in
  `crates/ds-daemon/src/device.rs` is now a reconnect loop: on a `Disconnected` event or
  a gone-class transport error (`hotplug::classify` → `IoFault::Gone`) the device is
  marked unhealthy (status `disconnected (<reason>)`), `deck/device/<serial>/disconnected`
  is published, the LED/frame planes are detached, the transport is closed — and the
  loop re-opens the URI, re-handshakes every module (software-mode first) and re-pushes
  the cached latest targets, all within the ≤ 5 s replug budget. The daemon PID is
  unchanged; other modules keep working; non-fatal transport errors are logged and
  counted, never propagated to the main loop.
- **Re-push cache**: every planned push is cached latest-frame-wins per target
  (DS-111 panel/key exclusion shared with `device_task`); a reconnect re-sends exactly
  the cache (images directly, LED-plane pushes through the latest-wins `LedLane`, whose
  dedup cache is invalidated on each reconnect so a fresh connection gets the current
  values again). Re-push is a pure transport operation — no state read, no `rev` bump (S1).
- **Initial open failure** also retries with backoff (200 ms … 2 s cap) instead of idling
  forever, so a daemon started before the deck is plugged in picks it up.
- systemd `Restart=on-failure` stays as the safety net for *full process* crashes; it is
  not part of the hotplug path and must not be relied on for module unplugs.

## Consequences

- vdev regression (`tools/observe-orientation.sh rehearse`): `throttle off 1` + a write →
  same daemon pid, `disconnected` + `connected` events, re-push within ≤ 5 s, frame
  sha256 intact. Cargo tests in `device.rs` cover the fake-transport and real-vdev
  (fresh server on the same socket) replug paths.
- `ds status` never reports a restart on unplug; `device_status` shows
  `disconnected (<reason>)` while down and `connected <uri>` after the re-handshake.
- DS-112 (identity remap) can build on this: the reconnect path already re-handshakes
  and re-pushes within budget, so a reseat/rotation only has to update the
  slot/position mapping, not revive the daemon.
