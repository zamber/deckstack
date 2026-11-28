# Ticket DS-123 — runtime placement solver (fit + rotation deduction)

Date: 2026-11-28. Type: algorithm + tested prototype. Consumes DS-122's
`placement-matrix.json`; will be integrated into the preview webapp in DS-124.

## Motivation

The hardware's reported rotation is unreliable ("does not match rotation at all"), so
the renderer must **deduce** each module's placement and rotation from its slot
(port-path), the set of currently connected modules, and the base orientation. Three
layers exist: hardware (iffy), visual (what we draw), actual (physical). The solver
bridges hardware -> visual.

## Requirements

1. Input: list of connected devices `{kind, slot, connection}`, base orientation
   (`horizontal`/`vertical`), and the placement matrix (DS-122). Vertical base
   transposes the grid 6x4 -> 4x6 (SALON-001 finding 1).
2. Output: for each device `{col, row, rotation, footprint}` on the grid, or
   `unplaced` with a reason (grid full / connector taken).
3. Algorithm: constraint fit over the grid.
   - Place the largest footprint first (keys 4x3 shrinks the solution space most);
   - for each module, candidate placements = matrix entries for its slot (all listed
     rotations), filtered to those that don't overlap already-placed modules;
   - if several candidates remain (ambiguous rotation / overhanging positions), bias by
     base orientation (vertical/horizontal) and prefer the candidate whose connector
     cell is nearest the deck center (cross-position rule); 2x2 modules (knobs, dial)
     may rotate in place, so all 4 rotations of a free 2x2 area are candidates;
   - if no candidate fits, mark `unplaced` (not everything fits at once — physically
     true) rather than overlapping.
4. A real solver (backtracking) only if the greedy fit fails; the keys-first heuristic
   should suffice — document why.
5. Deterministic: same input -> same output. No randomness.

## Acceptance

- Prototype (python or TS, stdlib only) + a test that runs all permutations of the
  vdev reference set (knobs 1-2-3, keys 2-3, faders 1-1-3, rollers 3-3, dial 1-3-4)
  and subsets, asserting: no overlaps, every placed module's cells match one of its
  matrix candidates, unplaced only when the grid is genuinely full.
- Cross-check against `crates/ds-vdev/tests/ds112_permutations.rs` expectations.
- Short design doc `docs/discovery/salon-001/placement-solver.md` with the algorithm,
  the bias rules, and the complexity argument.

## Links

DS-122 (matrix), DS-112, DS-121, DS-124 (integration), SALON-001.
