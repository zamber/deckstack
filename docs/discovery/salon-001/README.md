# SALON-001 evidence — connector/orientation mapping session (2026-11-27)

Hardware session on the salon deck (base `MHBS01AA461251603330`), daemon 0.1.0 on
`hid:`. Probe module: 3xKnob (`MHKD15AA431251602831`, USB 2516:0202), swept across the
accessible connectors at rotations 0/90/180 deg CCW. Raw captures: /tmp/ds112-hw/
(events*.log, usb-step*.txt, connector-map.csv).

## Findings

1. **Base orientation** is a one-direction tilt switch: flat = tag 0x2B payload `01`
   (`horizontal`), upright = `00` (`vertical`); ~2 reports/s heartbeat; the magnetic
   leg is NOT a separate sensor — attaching it only tilts the base. Vertical base
   transposes the logical grid 6x4 -> 4x6 with no USB hot-plug.
2. **Each physical connector exposes a different USB slot per module rotation** —
   (slot, rotation) is the identity key, confirming DS-112's premise:

   | connector (flat base) | rot0 | rot90 | rot180 |
   |---|---|---|---|
   | top-left    | 1-2-3 | 1-2-4 | — |
   | middle-top  | 3-1   | 1-1-1 | 3-3 |
   | top-right   | 3-4   | 1-4-3 | 1-4-1 |
   | bottom-left | 1-2-2 | 4-4   | — |
   | bottom-middle | 4-2 | 2-3  | 2-1 |
   | bottom-right | 1-1-3 | 1-3-4 | 1-3-1 |

   Slot = USB port path relative to the base's top hub (abs path minus leading
   `<bus>-<rootport>`, e.g. abs `3-2-1-4-3` = slot `1-4-3`). Base MCU = `1-4-4`.
3. **Cross-validation with the upstream reference unit**: our slots 1-3-4 (dial),
   1-4-3 (knobs), 2-3 (keys), 3-3 (rollers), 1-1-3 (faders), 1-4-4 (base) all match the
   reference slot table; 1-1-1 and 1-2-4 are the documented over-edge connectors.
   Corner connectors were not seatable in some rotations (as documented).
4. **Defect (DS-120)**: every module unplug fully restarts the daemon (pid changed
   824920 -> 1356741 -> 1415680 -> 1487691 across the session); `/devices` stays empty
   in the 0.1.0 alpha, so there is no slot/position state surface today.

## Files

- `connector-map-labeled.csv` — raw sweep rows with physical labels
- `deck-map.svg` — visual map of the base (flat orientation) with slots per rotation
- `tools/map_connectors.py` (repo root tools/) — capture/watch/render tooling
