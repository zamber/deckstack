# Ticket DS-120 — daemon fully restarts on any module hot-unplug

Date: 2026-11-27. Type: bug ticket. Filed from the DS-112 hardware mapping session
(SALON-001 evidence bundle in /tmp/ds112-hw/).

## Symptom

Every module unplug (even a 3 s reseat into the same connector) takes down the whole
`ds-daemon`, not just the module: the daemon PID changes and all WS/IPC connections are
dropped (`E_INTERNAL: connection closed by daemon`). The salon-deck app shows a booting
image on the keys panel during the restart.

Evidence (2026-11-27, deck `hid:`, daemon run from the installed 0.1.0 AppImage):

- baseline pid 824920 → knob module unplug at 21:48:05 (`deck/device/MHKD15AA431251602831/disconnected`)
  → pid 1356741
- knob moved to another connector at 21:59:37 (same `disconnected` event) → pid 1415680
- `ds status` before/after: `device=hid:` unchanged, `rev` keeps counting up from the
  persisted state (state survives; the process does not)

## Why it matters

DS-112 requires re-handshake and re-push within the ≤ 5 s reconnect budget *without an
app restart*. A full daemon restart on every hotplug makes that budget meaningless and
reproduces the "Salon app hung until the daemon restarted" class of failure from the
predecessor stack. The reconnect path must be fixed to survive module-level hotplug.

## Requirements

1. Root-cause why the daemon exits on module disconnect (device-task error propagated
   to the main loop? systemd unit `Restart=` masking a crash? salon-deck watchdog
   killing it?). Distinguish daemon crash vs. deliberate restart; capture the daemon's
   own log/stderr around the event (currently not visible at
   `$XDG_RUNTIME_DIR/deckstack/daemon.log` — journald or the unit's output).
2. A module-level disconnect must produce `deck/device/<serial>/disconnected`, mark the
   device unhealthy, and keep the daemon process alive; reconnect re-handshakes and
   re-pushes within ≤ 5 s (`docs/90-verification.md`).
3. Regression test on vdev: `ds-vdevctl throttle off 1` + a write → daemon stays alive
   (same pid), `disconnected` fires, replug → re-handshake + intact frame sha256. The
   existing rehearse step in `tools/observe-orientation.sh` asserts frame integrity
   across a *daemon* restart; add the no-restart assertion.
4. Docs: `docs/components/virtual-device.md` + the DS-112 procedure note the observed
   restart-on-unplug behavior of the current alpha as a known defect.

## Acceptance

On vdev and on hardware: unplug a module for 3 s, replug into the same connector —
daemon PID unchanged, `disconnected`/`connected` events both observed, cached visuals
re-pushed within ≤ 5 s, `ds status` never reports a restart.

## References

- SALON-001 evidence bundle (/tmp/ds112-hw/events*.log, usb-step*.txt, status-step*.txt)
- `docs/tickets/2026-11-25-module-position-orientation.md` (DS-112) symptom section
- `docs/90-verification.md` (≤ 5 s reconnect budget), `docs/components/virtual-device.md`
- `tools/observe-orientation.sh` (vdev replug rehearsal)
