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

## Keys-module sweep (2026-11-27, second pass, labeled live)

Probe: 15xKey IPS (`2516:01fe`). 11 seatings logged; the "not possible" positions
(top-middle vertical, everything at 180, parts of 270) produced no rows, as expected.

| position | slot |
|---|---|
| top-left, vertical, logo bottom (its shipped spot) | 2-2 |
| top-right, vertical | 1-3-2 |
| bottom-right, vertical | 1-3-4 |
| bottom-middle, vertical | 1-1-2 |
| bottom-left, vertical | 2-3 |
| rot90cw (logo left), left side | 1-2-2 |
| rot90cw, left with roller space on left | 4-1 |
| rot90cw, right with roller space on right | 4-2 |
| rot90cw, full right | 2-4 |
| rot270 (logo right), right edge | 1-4-1 |
| rot270, left with roller space on left | 3-3 |

Note the screen module uses different connector points than the knob: the same
physical connector yields a different slot per module kind (e.g. bottom-left is
1-2-2 for the knob at rot0 but 2-3 for the keys vertical). 2-3 is the reference
unit's keys slot; 1-3-4 is the reference dial slot — the screen physically covers
the dial's connector. `connector-map-keys2-labeled.csv` has the raw rows.

## Faders-module sweep (2026-11-27)

Probe: 5xFaders (`2516:0200`). 12 seatings logged (90cw round was entirely
un-seatable). Alignment note: the first upright round described 5 positions but one
was a duplicate, so 4 upright + 2 rot180 + 6 rot270 = 12 logged.

| position | slot |
|---|---|
| upright, right edge | 1-2-2 |
| upright, right with roller space | 4-1 |
| upright, middle (knob spaces both sides) | 4-2 |
| upright, full right | 2-4 |
| rot180, right | 1-1-3 |
| rot180, middle | 1-4-1 |
| rot270, top-left | 3-3 |
| rot270, top-right | 1-1-1 |
| rot270, middle-right | 1-4-3 |
| rot270, bottom-right | 1-3-4 |
| rot270, bottom-middle | 1-1-2 |
| rot270, bottom-left | 2-3 |

`connector-map-faders-labeled.csv` has the raw rows. Slots repeat across module kinds
at the same physical connector+rotation (e.g. bottom-right rot0: knob 1-1-3,
keys vertical 1-3-4, faders rot270 1-3-4), confirming the connector's port set is
shared and the module kind disambiguates the footprint.

## Caveat: rotation labels are session-relative

The rot0/90/180/270 labels above are relative to each sweep's starting orientation,
not an absolute reference. Before DS-112 implementation, redo one reference sweep per
module kind with the module's corner indicator in a fixed standard position, and
re-anchor all tables to it.

## Roller-module sweep (2026-11-27)

Probe: 2xRoller (`2516:0204`), 18 seatings logged. The 180 round seated nothing. The
final 270 round described 9 positions but only 7 distinct paths logged (the repeated
"top-left" and one middle-left variant did not produce new rows) — alignment of the
last rows is best-effort, see caveat above about session-relative rotations.

| position | slot |
|---|---|
| upright (corner TR), left | 1-2-1 |
| upright, left + fader space left | 4-4 |
| upright, middle-left | 4-3 |
| upright, middle-right | 1-3-4 |
| upright, right + fader space | 1-2-3 |
| upright, full right | 3-1 |
| rot90cw, left-center + roller space top | 1-2-2 |
| rot90cw, same but right | 4-1 |
| rot90cw, bottom-left | 4-2 |
| rot90cw, bottom-middle | 3-3 |
| rot90cw, bottom-right | 1-4-1 |
| rot270, top-left | 1-4-2 |
| rot270, top-right | 1-3-1 |
| rot270, middle-right + fader space top | 1-3-3 |
| rot270, middle-right + fader space bottom | 1-1-4 |
| rot270, bottom-right | 2-1 |
| rot270, bottom-left | 3-2 |
| rot270, middle-left + fader space | 3-3 |

Cross-checks: 3-3 is the reference unit's roller slot; 1-1-4 was this deck's shipped
roller slot (appears at rot270 middle-right + fader space bottom).
`connector-map-rollers-labeled.csv` has the raw rows.

## Reference (rot0) anchors per module — 2026-11-27, base flat, corner indicator in a fixed standard position

| module | reference seating | slot (rot0 anchor) |
|---|---|---|
| knobs (0202) | top-left connector, corner indicator TR | 1-2-3 |
| keys (01fe) | bottom-right, logo facing user, corner TL | 1-3-4 |
| faders (0200) | bottom-right ("right"), vertical, corner TR, drag-right=100% | 1-1-3 |
| rollers (0204) | right edge, full height, corner TR | 1-3-4 |
| dial (0206) | bottom-right, corner TR | 1-1-3 |

All earlier sweep rotation labels should be re-anchored to these per-module rot0
slots. `connector-map-ref-labeled.csv` has the raw rows.
