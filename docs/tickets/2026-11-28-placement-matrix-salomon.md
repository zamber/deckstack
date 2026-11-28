# Ticket DS-122 — placement matrix re-derived from the recorded SALON-001 sweeps

Date: 2026-11-28. Type: research artifact (docs only; no daemon/client code).

## Motivation

DS-121's preview webapp first borrowed the upstream hubdeck's `slots.generated.json`
placement table, but the owner reports that table's placement model is broken for our
deck (it was derived from the official app's board layout, not from our hardware). We
have our own recorded evidence: the SALON-001 sweeps in
`docs/discovery/salon-001/` (connector-map-labeled.csv, -keys2-, -faders-, -rollers-,
-ref-, plus README.md and deck-map.svg).

## Goal

Produce `docs/discovery/salon-001/placement-matrix.json`: for every observed
`(module_kind, slot)` seating, the grid placement on the base's **flat 6x4 cell grid**
(col, row, rotation in degrees CW, footprint in cells), with the connector position
labeled as in the sweeps. Resolve the rotation-anchor caveat from the README
("rotation labels are session-relative"): re-anchor every sweep to the reference sweep
(`connector-map-ref-labeled.csv`, corner indicator in the fixed standard position) using
the cross-position rule — the module's corner indicator sits diagonally across from its
connector, so the connector cell is at the module corner nearest the deck center.

Rules:

- Grid: flat base = 6 cols x 4 rows, cell (0,0) = top-left as seen by the user facing the
  deck. Modules may overhang the edge but keep >= 1 cell on the base (upstream
  clampToGrid rule, confirmed by the sweeps: "wonky middle" seatings etc.).
- Footprints (owner-measured): keys 4x3, knobs 2x2, dial 2x2, rollers 1x4, faders 2x4
  at rotation 0 (flat grid).
- Every row of every labeled CSV must be accounted for (26 connectors + base MCU
  1-4-4). Mark entries with `confidence: "verified"` (ref sweep / repeated across
  sweeps) vs `"derived"` (session-relative alignment best-effort).
- Ambiguous rotations (same slot seatable in more than one rotation at the same
  connector) get all candidate rotations listed: `rotations: [0, 90]`.

## Acceptance

- `placement-matrix.json` covers all 4 sweep CSVs' distinct (kind, slot) pairs; a
  validation script (stdlib python) checks: every entry's footprint fits the 6x4 grid
  modulo overhang, no two entries for the same kind+slot disagree on position, ref
  entries match the README reference table.
- README.md gains a "Placement matrix" section explaining the derivation and the
  confidence flags.

## Links

SALON-001, DS-112, DS-121, DS-123 (solver consumes this matrix), DS-124.
