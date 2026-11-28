#!/usr/bin/env python3
"""test_placement_solver.py — DS-123 solver tests. Run: python3 tools/test_placement_solver.py

(a) all permutations of the 5-module vdev reference set and all subsets,
    asserting no overlaps + every placement matches a matrix candidate +
    unplaced only when justified (no candidate fits / connector taken);
(b) vertical-orientation transpose cases (6x4 -> 4x6);
(c) 2x2 in-place rotation cases (knobs/dial: all 4 rotations of a free 2x2).

Cross-checks crates/ds-vdev/tests/ds112_permutations.rs (28 connectors x 5 kinds
x 2 orientations at the transport level) against the solver's reference set:
crates/ds-vdev/src/hub.rs `reference_slot` — knobs 1-2-3, keys 2-3, faders 1-1-3,
rollers 3-3, dial 1-3-4.
"""

from __future__ import annotations

import itertools
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import placement_solver as ps

# The vdev reference set (crates/ds-vdev/src/hub.rs reference_slot).
REF_SET = [
    ("knobs", "1-2-3"),
    ("keys", "2-3"),
    ("faders", "1-1-3"),
    ("rollers", "3-3"),
    ("dial", "1-3-4"),
]
HUB_REF_SLOTS = {
    "knobs": "1-2-3",
    "keys": "2-3",
    "faders": "1-1-3",
    "rollers": "3-3",
    "dial": "1-3-4",
}

failures = []
checks = 0


def check(cond, msg):
    global checks
    checks += 1
    if not cond:
        failures.append(msg)
        print("FAIL:", msg)


def dev(pair):
    kind, slot = pair
    return {"kind": kind, "slot": slot, "connection": f"virtual:{kind}-{slot}"}


def solve_pairs(pairs, orientation="horizontal"):
    return ps.solve([dev(p) for p in pairs], orientation)


def assert_invariants(pairs, orientation, label):
    """No overlaps; every placement matches a matrix candidate; unplaced only
    when every candidate is off-base or overlaps, or the connector is taken."""
    res = solve_pairs(pairs, orientation)
    cols, rows = ps.grid_dims(orientation)
    occupied = {}
    for r in res:
        tag = f"{r['kind']}@{r['slot']}"
        if r.get("unplaced"):
            continue
        cells = {(c, r_) for (c, r_) in r["cells"]}
        on = ps.on_base(cells, cols, rows)
        check(len(on) >= 1, f"{label}: {tag} keeps >=1 cell on the {cols}x{rows} base")
        for cell in on:
            check(
                cell not in occupied,
                f"{label}: overlap at {cell} between {occupied.get(cell)} and {tag}",
            )
            occupied[cell] = tag
        cands = ps.candidates_for(ps.REFERENCE_MATRIX, r["kind"], r["slot"], orientation)
        check(
            any(c["col"] == r["col"] and c["row"] == r["row"] and c["rot"] == r["rotation"] for c in cands),
            f"{label}: {tag} placement ({r['col']},{r['row']},rot{r['rotation']}) matches a matrix candidate",
        )
    for r in res:
        if not r.get("unplaced"):
            continue
        tag = f"{r['kind']}@{r['slot']}"
        if "no matrix entry" in r["reason"] or "connector" in r["reason"]:
            continue
        cands = ps.candidates_for(ps.REFERENCE_MATRIX, r["kind"], r["slot"], orientation)
        check(bool(cands), f"{label}: unplaced {tag} has candidates but was not placed ({r['reason']})")
        justified = all(
            (not ps.on_base(ps.cells_of(c), cols, rows)) or (ps.cells_of(c) & set(occupied))
            for c in cands
        )
        check(justified, f"{label}: unplaced {tag} is justified (all candidates off-base/overlapping): {r['reason']}")
    return res, occupied


def test_a_permutations_and_subsets():
    # reference set matches the hub's rot0 anchors exactly (hub.rs reference_slot)
    for kind, slot in HUB_REF_SLOTS.items():
        check((kind, slot) in REF_SET, f"(a) reference set contains {kind}@{slot} (hub.rs reference_slot)")
    solves = 0
    for k in range(1, len(REF_SET) + 1):
        for subset in itertools.combinations(REF_SET, k):
            for perm in itertools.permutations(subset):
                assert_invariants(list(perm), "horizontal", f"(a) perm={perm}")
                solves += 1
    # subsets in vertical orientation too
    vsolves = 0
    for k in range(1, len(REF_SET) + 1):
        for subset in itertools.combinations(REF_SET, k):
            assert_invariants(list(subset), "vertical", f"(a-vertical) subset={subset}")
            vsolves += 1
    # the full reference set cannot fit (12+8+4+4+4 = 32 footprint cells > 24):
    # when something is unplaced, the grid must be genuinely full
    res, occupied = assert_invariants(REF_SET, "horizontal", "(a) full reference set")
    check(len(occupied) == 24, f"(a) full reference set leaves the grid genuinely full (got {len(occupied)}/24)")
    check(any(r.get("unplaced") for r in res), "(a) full reference set: not all 5 modules fit at once")
    # determinism: same input twice, and input order must not matter
    r1 = json.dumps(solve_pairs(REF_SET), sort_keys=True)
    r2 = json.dumps(solve_pairs(REF_SET), sort_keys=True)
    check(r1 == r2, "(a) deterministic: same input -> same output")
    by_dev = lambda res: {f"{r['kind']}@{r['slot']}": r for r in res}
    check(
        by_dev(solve_pairs(REF_SET)) == by_dev(solve_pairs(list(reversed(REF_SET)))),
        "(a) permutation-invariant: reversed input gives identical per-device placements",
    )
    print(f"PASS (a) permutations+subsets: {solves} horizontal solves, {vsolves} vertical solves, "
          f"full-set occupancy {len(occupied)}/24, all invariants hold")


def test_b_vertical_transpose():
    extra = [("keys", "1-3-4"), ("rollers", "1-3-4"), ("dial", "1-1-3"),
             ("faders", "2-4"), ("keys", "4-2"), ("knobs", "1-4-3"), ("knobs", "1-1-1")]
    for pair in REF_SET + extra:
        rh = solve_pairs([pair], "horizontal")[0]
        rv = solve_pairs([pair], "vertical")[0]
        cols, rows = ps.grid_dims("vertical")
        check((cols, rows) == (4, 6), "(b) vertical grid is 4x6 (SALON-001 finding 1)")
        check(not rh.get("unplaced") and not rv.get("unplaced"),
              f"(b) {pair} placeable alone in both orientations")
        check(
            (rv["col"], rv["row"]) == (rh["row"], rh["col"]),
            f"(b) {pair}: vertical ({rv['col']},{rv['row']}) is the transpose of horizontal ({rh['col']},{rh['row']})",
        )
    res, occupied = assert_invariants(REF_SET, "vertical", "(b) full reference set vertical")
    print(f"PASS (b) vertical transpose: {len(REF_SET) + len(extra)} single-module cases + full reference set "
          f"({len(occupied)}/24 cells occupied), all invariants hold")


def test_c_2x2_in_place_rotation():
    cands = ps.candidates_for(ps.REFERENCE_MATRIX, "knobs", "1-2-3", "horizontal")
    check(sorted(c["rot"] for c in cands) == [0, 90, 180, 270],
          "(c) knobs 2x2: all 4 rotations of the free 2x2 area are candidates")
    r = solve_pairs([("knobs", "1-2-3")], "horizontal")[0]
    check((r["col"], r["row"], r["rotation"]) == (0, 0, 0),
          f"(c) knobs horizontal picks (0,0,rot0) by orientation bias + tie-break (got {r})")
    rv = solve_pairs([("knobs", "1-2-3")], "vertical")[0]
    check(rv["rotation"] == 90 and (rv["col"], rv["row"]) == (0, 0),
          f"(c) knobs vertical picks rot90 in place (got rot{rv['rotation']} at {rv['col']},{rv['row']})")
    rd = solve_pairs([("dial", "1-3-4")], "horizontal")[0]
    check((rd["col"], rd["row"], rd["rotation"]) == (4, 2, 0),
          f"(c) dial picks (4,2,rot0) (got {rd['col']},{rd['row']},rot{rd['rotation']})")
    # 2x2 rotation in place next to a placed keys module: keys@2-3 occupies the
    # bottom-left 4x3, dial@1-3-4's 2x2 at (4,2) is free and rotates in place
    res, _ = assert_invariants([("keys", "2-3"), ("dial", "1-3-4")], "horizontal", "(c) keys+dial")
    dial = next(r for r in res if r["kind"] == "dial")
    check(not dial.get("unplaced") and dial["rotation"] in (0, 90, 180, 270)
          and (dial["col"], dial["row"]) == (4, 2),
          f"(c) dial rotates in place at (4,2) next to keys (got {dial})")
    print("PASS (c) 2x2 in-place rotation: knobs/dial generate all 4 rotations, "
          "orientation bias picks rot0 horizontal / rot90 vertical")


def test_d_edge_cases():
    # overhang: >=1 cell on the base, the rest may hang off the edge
    r = solve_pairs([("keys", "4-2")], "horizontal")[0]
    cells = {(c, rr) for (c, rr) in r["cells"]}
    on = ps.on_base(cells, 6, 4)
    check(len(cells) == 12 and len(on) == 8,
          f"(d) keys@4-2 overhangs the right edge with 8/12 cells on the base (got {len(on)})")
    r = solve_pairs([("knobs", "1-1-1")], "horizontal")[0]
    on = ps.on_base({(c, rr) for (c, rr) in r["cells"]}, 6, 4)
    check((r["col"], r["row"]) == (2, -1) and len(on) == 2,
          "(d) knobs@1-1-1 overhangs the top edge with 2 cells on the base")
    # a candidate fully off the base is never accepted
    off = {"col": 6, "row": 0, "rot": 0, "w": 2, "h": 2}
    check(not ps.on_base(ps.cells_of(off), 6, 4), "(d) fully off-base candidate rejected")
    # one module per connector: same slot twice -> second unplaced, connector taken
    res = solve_pairs([("faders", "1-1-3"), ("dial", "1-1-3")], "horizontal")
    dial = next(r for r in res if r["kind"] == "dial")
    check(dial.get("unplaced") and "connector" in dial["reason"],
          f"(d) duplicate slot: dial unplaced, connector taken (got {dial.get('reason')})")
    # unknown slot -> unplaced with reason, never overlapped
    res = solve_pairs([("keys", "9-9")], "horizontal")
    check(res[0].get("unplaced") and "no matrix entry" in res[0]["reason"],
          "(d) unknown slot 9-9 unplaced with 'no matrix entry'")
    # every inline matrix entry is individually placeable and matches a candidate
    for key in sorted(ps.REFERENCE_MATRIX):
        kind, slot = key.split("@")
        res, _ = assert_invariants([(kind, slot)], "horizontal", f"(d) matrix entry {key}")
        check(not res[0].get("unplaced"), f"(d) matrix entry {key} places alone")
    print("PASS (d) edge cases: overhang (>=1 cell on base), duplicate slot, unknown slot, "
          f"{len(ps.REFERENCE_MATRIX)} matrix entries placeable alone")


def main():
    test_a_permutations_and_subsets()
    test_b_vertical_transpose()
    test_c_2x2_in_place_rotation()
    test_d_edge_cases()
    print(f"--- {checks} checks, {len(failures)} failures ---")
    if failures:
        print("PLACEMENT SOLVER TESTS FAILED")
        return 1
    print("ALL PLACEMENT SOLVER TESTS PASSED")
    return 0


if __name__ == "__main__":
    sys.exit(main())
