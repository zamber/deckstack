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

## Placement matrix (DS-122)

2026-11-28. `placement-matrix.json` maps every observed `(module_kind, slot)` seating of
the five labeled CSVs to its placement on the base's flat 6x4 cell grid:
`"<kind>@<slot>": {"col", "row", "rot" (or "rotations"), "confidence"}`. `validate_matrix.py`
(stdlib only) re-checks the file against the CSVs and this README:

    $ python3 docs/discovery/salon-001/validate_matrix.py
    placement-matrix.json: 57 entries; per kind {'dial': 1, 'faders': 12, 'keys': 11, 'knobs': 16, 'rollers': 17}; confidence {'derived': 52, 'verified': 5}
    grid 6x4; ref anchors from README: dial@1-1-3, faders@1-1-3, keys@1-3-4, knobs@1-2-3, rollers@1-3-4
    PASS: schema ok; every rotated footprint keeps >=1 cell on the 6x4 grid; ref entries match the README reference table; all CSV (kind, rel_path) pairs covered; no kind+slot disagreements

### Derivation method

- Grid: 6 cols x 4 rows, (0,0) top-left as seen by the user facing the deck flat
  (`deck-map.svg` is drawn in this view). The six labeled connectors of the SVG sit on the
  top/bottom edges and map to cells: top-left (0,0), middle-top (3,0), top-right (5,0),
  bottom-left (0,3), bottom-middle (3,3), bottom-right (5,3).
- `(col, row)` is the **connector cell** of the slot — fixed per slot, which is why a slot
  observed at several rotations keeps one (col,row) and lists `rotations` instead of `rot`.
- `rot` is the absolute module rotation, re-anchored to the reference sweep (the ref slot is
  rot 0) per the caveat above that session labels are relative. The per-kind anchors found in
  the sweeps: knobs ref 1-2-3 = "top-left rot0", so session rot0/90/180 -> 0/90/180; keys
  ref 1-3-4 = "bottom-right, keys vertical", so vertical/rot90cw/rot270 -> 0/90/270; faders
  ref 1-1-3 = "rot180, right", so upright/rot180/rot270 -> 180/0/90; rollers ref 1-3-4 =
  "upright, middle-right", so upright/rot90cw/rot270 -> 0/90/270; dial has only the ref row
  1-1-3 -> 0. Positive rotation follows the CCW convention of the knob sweep (deck-map.svg
  header "per module rotation (CCW)"); the per-session direction differences are absorbed by
  the ref anchor — the connector cell below was chosen to satisfy the cross-position rule,
  which pins the direction. (The DS-122 ticket words rotations as CW; under that reading the
  90/270 values of entries whose session used the opposite direction swap — the connector
  cells (col,row) are unaffected.)
- Cross-position rule: the corner indicator sits diagonally across from the connector, so
  the connector cell is the module corner nearest the deck center. From the reference table
  the indicator corner at rot 0 is TR for knobs/faders/rollers/dial and TL for keys, so the
  connector sits at the opposite corner of the rot-0 footprint (BL resp. BR); at rotation r
  it sits at that corner turned by r, and the footprint extends away from it. Rows labeled
  with a deck-map connector name use that connector's cell (the knob sweep's table is
  literally "connector (flat base)"); rows whose labels describe the module (keys
  rot90/rot270 "left side"/"full right", faders upright "right edge" vs "full right" — the
  latter hangs over the edge at cell (6,0), like the documented over-edge connectors
  1-1-1/1-2-4 — and the rollers rows) get the connector cell from that placement.
- Footprints at rot 0 (owner-measured): keys 4x3, knobs 2x2, dial 2x2, rollers 1x4, faders
  2x4. Modules may overhang the base edge but keep >= 1 cell on the grid; the validator
  checks this for every rot candidate of every entry.

### Confidence flags

- `verified` (5 entries): the reference-sweep slots — knobs@1-2-3 (0,0) rot 0, keys@1-3-4
  (5,3) rot 0, faders@1-1-3 (5,3) rot 0, rollers@1-3-4 (5,3) rot 0, dial@1-1-3 (5,3) rot 0 —
  each matches the README reference table's seating, slot and corner indicator.
- `derived` (52 entries): every other sweep row.
- `rollers@3-3` was logged twice (rot90cw "bottom-middle" and rot270 "middle-left + fader
  space"); the README warns the last rollers round is best-effort, so the entry keeps the
  rot90cw connector cell (4,3) and lists `"rotations": [90, 270]`.

### Disagreements with upstream `slots.generated.json`

- Upstream encodes the same connector slots but on a 4x6 **portrait** grid (vertical base)
  with clockwise rotations and its own 180-degree board flip (`prototype/tools/gen_slots.py`
  in hubdeck-upstream). Transposing it to our flat grid and trying all dihedral mappings and
  both rotation-chirality flips, at best 4 of the 56 shared keys agree with our CSV-derived
  matrix — its placement model is broken for our deck, as the owner said. Our CSVs win.
- Upstream's `verified` flags mark the **reference unit's** slots (knobs@1-4-3, keys@2-3,
  rollers@3-3, dial@1-3-4, faders@1-1-3), which differ from our ref-sweep anchors for
  knobs/keys/rollers/dial (1-2-3, 1-3-4, 1-3-4, 1-1-3). Note `crates/ds-vdev/src/hub.rs`
  `reference_slot()` currently encodes the reference-unit values for keys/rollers/dial
  (2-3, 3-3, 1-3-4), disagreeing with `connector-map-ref-labeled.csv` for those kinds.
- 21 upstream entries hang outside its own 4x6 grid (negative cols, marked `derived`); every
  entry of our matrix keeps >= 1 cell on the 6x4 grid (validator check). knobs@3-4 is
  observed in our knob sweep but absent from upstream's table entirely.

### Camera-verified corrections (SALON-003 / DS-125, 2026-11-28)

The webcam facing view of the physical deck (current seating: keys `2-2` top-left,
knobs `1-4-3` top-right, rollers `1-1-4` bottom-left horizontal, dial `1-3-4`
bottom-right, faders unplugged) confirmed `keys@2-2` and corrected two entries, now
`confidence: "verified-camera"` with a `source` note (see `verification-ds124.md`):

- `rollers@1-1-4`: was (2,2) rot 270 from the best-effort last rollers round; camera
  shows connector cell (0,3), cells c0-3 r3 horizontal → now (0,3), rotations [90, 270].
- `dial@1-3-4`: was missing (the dial sweep logged only the ref row); camera shows
  connector (5,3), cells c4-5 r2-3 → added as (5,3) rot 0.

### Not placed confidently

- `rollers@3-3`'s second observation (rot270 "middle-left + fader space") does not yield the
  same connector cell as its rot90cw row; it is kept only as a `rotations` candidate at (4,3)
  per the README best-effort note on the last rollers round.
- faders upright "right edge" (1-2-2, cell (5,0)) vs "full right" (2-4, cell (6,0)): the
  README notes one upright position was described twice; the two are distinguished only by
  the over-edge reading of "full right" — both `derived`.
- keys vertical rows use the deck-map connector cells (e.g. keys@2-2 at (0,0), footprint
  overhanging the top-left corner with 1 cell on grid); the alternative on-deck reading
  would put the connector at the inner corner (3,2). Both satisfy the cross-position rule;
  the connector-cell reading matches the knob sweep's "connector (flat base)" table.
