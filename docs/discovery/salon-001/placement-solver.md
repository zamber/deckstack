# DS-123 placement solver — algorithm design (SALON-001)

Date: 2026-11-28. Status: designed + stdlib-python prototype (`tools/placement_solver.py`,
tests `tools/test_placement_solver.py`). Consumes the DS-122 matrix
(`docs/discovery/salon-001/placement-matrix.json`, schema in
`docs/tickets/2026-11-28-placement-matrix-salomon.md`); integration into the preview
webapp is DS-124. Requirements ticket: `docs/tickets/2026-11-28-placement-solver.md`.

## Problem

The hardware's rotation readout is unreliable, so for every connected device
`{kind, slot, connection}` plus the base orientation (`horizontal`/`vertical`) the
solver must deduce `{col, row, rotation, footprint}` on the base grid, or `unplaced`
with a reason. Three layers exist — hardware (iffy), visual (what we draw), actual
(physical) — and the solver bridges hardware → visual
(`docs/tickets/2026-11-28-placement-solver.md`, Motivation).

## Grid model

- Flat base: 6 cols x 4 rows, cell (0,0) top-left as seen by the user facing the deck
  (`docs/tickets/2026-11-28-placement-matrix-salomon.md`, Rules).
- Vertical base **transposes** the logical grid 6x4 → 4x6 with no USB hot-plug
  (SALON-001 finding 1, `docs/discovery/salon-001/README.md`). The solver maps every
  matrix candidate `(col,row)` to `(row,col)` on the 4x6 grid and keeps `rot`.
- Footprints at rot0 (flat grid, w x h cells): keys 4x3, knobs 2x2, dial 2x2,
  rollers 1x4, faders 2x4 (`docs/tickets/2026-11-28-placement-matrix-salomon.md`).
  rot ∈ {0,90,180,270} CW; 90/270 swap w/h.
- Modules may overhang the edge but keep **>= 1 cell on the base**
  (`docs/tickets/2026-11-28-placement-matrix-salomon.md`, Rules — upstream
  clampToGrid rule confirmed by the sweeps). A candidate whose cells are all off-base
  is invalid.
- Not all modules fit at once — physically true (`docs/discovery/salon-001/README.md`);
  the solver must report `unplaced`, never overlap
  (`docs/tickets/2026-11-28-placement-solver.md`, req 3).
- One module per connector: `Hub::set_slot` rejects moves onto an occupied slot
  (`crates/ds-vdev/src/hub.rs`), so a duplicate slot in the input is `unplaced`
  ("connector taken").

## Matrix

Schema (DS-122): `{ "<kind>@<slot>": { col, row, rot, confidence } | { col, row, rotations: [...] } }`
in flat-grid coordinates. The prototype inlines a small subset
(`REFERENCE_MATRIX` in `tools/placement_solver.py`) derived from
`docs/discovery/salon-001/connector-map-ref-labeled.csv` and the README reference
table (reference seatings per kind: knobs 1-2-3 top-left, keys 1-3-4 bottom-right,
faders 1-1-3, rollers 1-3-4 right edge, dial 1-1-3 bottom-right), plus the vdev rot0
anchors from `crates/ds-vdev/src/hub.rs` `reference_slot` (knobs 1-2-3, keys 2-3,
faders 1-1-3, rollers 3-3, dial 1-3-4) and a few extra sweep rows for overhang /
ambiguity coverage. When DS-122 lands, the same solver loads the full
`placement-matrix.json`; unknown `(kind, slot)` → `unplaced` with reason
("no matrix entry"). The full 28-connector x 5-kind x 2-orientation permutation space
is exercised at the transport level by
`crates/ds-vdev/tests/ds112_permutations.rs` (282 cases); the solver is slot-agnostic
and only needs a matrix entry per seating.

## Algorithm

1. **Grid**: dims from orientation (6x4 horizontal, 4x6 vertical); vertical
   transposes every candidate `(col,row) → (row,col)`.
2. **Order**: largest footprint first — keys (4x3 = 12 cells) before faders (8),
   then the 4-cell modules — ties by `(kind, slot)`. This is deterministic and
   shrinks the solution space most aggressively first
   (`docs/tickets/2026-11-28-placement-solver.md`, req 3).
3. **Candidates per device**: the matrix entry for `<kind>@<slot>`, all listed
   rotations expanded; vertical transposes positions. Square 2x2 modules (knobs,
   dial) may rotate in place, so **all 4 rotations** of the entry's 2x2 area are
   candidates (req 3).
4. **Filter**: drop candidates with 0 cells on the base (overhang rule), then drop
   candidates overlapping already-placed cells. Overlap is never allowed.
5. **Ambiguity resolution** (deterministic total order on the survivors):
   a. *base-orientation bias* — prefer `rot % 180 == 0` when horizontal,
      `rot % 180 == 90` when vertical (rot is measured CW from the base's long
      axis; the vertical base is re-anchored by the transpose);
   b. *cross-position rule* — prefer the candidate whose **connector cell is
      nearest the deck center**; the connector cell is the module's corner nearest
      the deck center, because the corner indicator sits diagonally across from the
      connector (`docs/discovery/salon-001/README.md`, sweep notes;
      `docs/tickets/2026-11-28-placement-matrix-salomon.md`, Goal);
   c. tie-break `(col, row, rot)` — total order, so the output is a pure function of
      the input (req 5: no randomness).
6. **No fit → `unplaced` with reason**: "no matrix entry for kind@slot",
   "connector slot already taken", "all candidates overlap placed modules"
   (grid full), or "no candidate keeps >= 1 cell on the base" (req 2).
7. **Backtracking fallback** only if the greedy pass left a device unplaced: a
   bounded DFS over the same candidate sets in the same bias order, replacing the
   greedy result only when it places strictly more devices.

## Why greedy keys-first suffices (complexity)

Let n = number of connected modules (<= 5 kinds + base, `crates/ds-vdev/src/hub.rs`
serves 6), C = candidates per module after expansion (one position per
`<kind>@<slot>` in the matrix schema, <= 4 rotations, so C <= 4), F = max footprint
cells (12, keys). Greedy costs O(n · C · F) = at most ~240 cell operations — trivial.
The matrix gives exactly one position per `(kind, slot)`, so the largest module
(keys) has the fewest candidates and the most constraining footprint; placing it
first maximally prunes every smaller module's candidate set, which is why the
greedy fit is expected to suffice (req 4). The backtracking fallback is a safety
net with worst case O((C+1)^n) = 5^6 ≈ 3k nodes, node-capped at 100k; in all tested
permutations it never improves on greedy (the tests assert the greedy outcome is
justified, and the full reference set ends with a genuinely full 24/24 grid).
Memory is O(grid + n·F). Determinism: all iteration orders are fixed sorts; same
input → same output (asserted by the tests).

## Verification

`python3 tools/test_placement_solver.py` runs:

- (a) all permutations of the 5-module vdev reference set and all subsets
  (325 horizontal + 31 vertical solves), asserting no overlaps, every placement
  matches a matrix candidate, unplaced only when every candidate is off-base /
  overlapping / connector taken, determinism and permutation invariance, and that
  the full reference set leaves the grid genuinely full (24/24) when something is
  unplaced (32 footprint cells > 24 — not everything fits, physically true);
- (b) vertical-orientation transpose cases: grid 4x6, each single-module placement
  is the `(row,col)` transpose of its horizontal placement;
- (c) 2x2 in-place rotation: knobs/dial generate all 4 rotations of a free 2x2
  area; orientation bias picks rot0 horizontal / rot90 vertical;
- (d) edge cases: overhang (>= 1 cell on base), duplicate slot (connector taken),
  unknown slot, every inline matrix entry placeable alone.

Latest run: `--- 9840 checks, 0 failures ---` / `ALL PLACEMENT SOLVER TESTS PASSED`.

## Known limitations

- The inline matrix is a subset; until DS-122's `placement-matrix.json` lands,
  `(kind, slot)` pairs outside it are `unplaced` ("no matrix entry"). The solver
  loads the real matrix unchanged (same schema).
- Vertical mode transposes `(col,row) → (row,col)` and keeps `rot` (SALON-001
  finding 1 says "transposes"); the rotation re-anchor under a physical tilt is
  session-relative (`docs/discovery/salon-001/README.md`, caveat) and unverified on
  hardware — the orientation bias (rot90 preferred when vertical) is the working
  assumption.
- `confidence` flags in the matrix are not used for ranking; there is one entry per
  `(kind, slot)`, so there is nothing to rank between.
- The connector-nearest-center bias uses the bounding-box corner nearest the deck
  center; for heavily overhanging candidates whose corners are off-base it falls
  back to the nearest on-base cell.
- Greedy is not provably optimal in general; the backtracking fallback covers the
  residual cases within a node cap, and tests show it never fires on the reference
  set.
