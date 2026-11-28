#!/usr/bin/env python3
"""placement_solver.py — DS-123 runtime placement solver (prototype, stdlib only).

Deduces each connected module's grid placement + rotation from its slot
(port path), the set of connected modules, and the base orientation, using the
DS-122 placement matrix. The hardware's own rotation readout is unreliable, so
rotation is deduced, never read (docs/tickets/2026-11-28-placement-solver.md).

Grid: flat base 6 cols x 4 rows, cell (0,0) top-left as seen by the user facing
the deck; a vertical base transposes the logical grid to 4x6 (SALON-001
finding 1, docs/discovery/salon-001/README.md). Modules may overhang the edge
but keep >= 1 cell on the base (docs/tickets/2026-11-28-placement-matrix-salomon.md).

Matrix schema (DS-122): {"<kind>@<slot>": {"col", "row", "rot", "confidence"}
or {"col", "row", "rotations": [...], "confidence"}} in flat-grid coordinates.
REFERENCE_MATRIX below is a small inline subset derived from
docs/discovery/salon-001/connector-map-ref-labeled.csv and the README reference
table; the full matrix is owned by DS-122.

Usage:
  echo '{"devices":[{"kind":"keys","slot":"2-3"}],"orientation":"horizontal"}' \
    | python3 tools/placement_solver.py
  python3 tools/placement_solver.py input.json
"""

from __future__ import annotations

import json
import sys

# Footprints at rot0 on the flat grid (w x h cells), owner-measured
# (docs/tickets/2026-11-28-placement-matrix-salomon.md, DS-123 brief).
FOOTPRINTS = {
    "keys": (4, 3),
    "knobs": (2, 2),
    "dial": (2, 2),
    "rollers": (1, 4),
    "faders": (2, 4),
}

# Square 2x2 modules may rotate in place: all 4 rotations of a free 2x2 area
# are candidates (docs/tickets/2026-11-28-placement-solver.md, req 3).
SQUARE_KINDS = {"knobs", "dial"}

FLAT_COLS, FLAT_ROWS = 6, 4

# Inline matrix subset (flat-grid coordinates). Sources per entry:
#   - vdev reference anchors: crates/ds-vdev/src/hub.rs `reference_slot`
#     (knobs 1-2-3, keys 2-3, faders 1-1-3, rollers 3-3, dial 1-3-4)
#   - reference seatings: docs/discovery/salon-001/README.md reference table +
#     connector-map-ref-labeled.csv (keys 1-3-4 bottom-right corner TL,
#     faders 1-1-3 right edge, rollers 1-3-4 right edge, dial 1-1-3 bottom-right)
#   - extra sweep rows from the labeled CSVs for ambiguity / overhang coverage.
REFERENCE_MATRIX = {
    "knobs@1-2-3": {"col": 0, "row": 0, "rotations": [0, 90, 180, 270], "confidence": "verified"},
    "knobs@1-4-3": {"col": 4, "row": 0, "rotations": [0, 90, 180, 270], "confidence": "verified"},
    "knobs@1-1-1": {"col": 2, "row": -1, "rotations": [0, 90, 180, 270], "confidence": "derived"},
    "keys@2-3": {"col": 0, "row": 1, "rot": 0, "confidence": "verified"},
    "keys@1-3-4": {"col": 2, "row": 1, "rot": 0, "confidence": "verified"},
    "keys@4-2": {"col": 4, "row": 0, "rot": 90, "confidence": "derived"},
    "faders@1-1-3": {"col": 4, "row": 0, "rot": 0, "confidence": "verified"},
    "faders@2-4": {"col": 2, "row": 0, "rot": 90, "confidence": "derived"},
    "rollers@3-3": {"col": 0, "row": 0, "rotations": [90, 270], "confidence": "verified"},
    "rollers@1-3-4": {"col": 5, "row": 0, "rotations": [0, 180], "confidence": "verified"},
    "dial@1-3-4": {"col": 4, "row": 2, "rotations": [0, 90, 180, 270], "confidence": "verified"},
    "dial@1-1-3": {"col": 4, "row": 2, "rotations": [0, 90, 180, 270], "confidence": "verified"},
}

BACKTRACK_NODE_CAP = 100_000


def grid_dims(orientation):
    """(cols, rows) of the logical grid for the base orientation."""
    if orientation == "horizontal":
        return (FLAT_COLS, FLAT_ROWS)
    if orientation == "vertical":
        return (FLAT_ROWS, FLAT_COLS)  # SALON-001 finding 1: 6x4 -> 4x6
    raise ValueError(f"unknown orientation {orientation!r}")


def rotated_wh(kind, rot):
    """Footprint (w, h) at rotation rot CW; 90/270 swap w/h."""
    w, h = FOOTPRINTS[kind]
    return (h, w) if rot % 180 == 90 else (w, h)


def cells_of(cand):
    return {
        (cand["col"] + i, cand["row"] + j)
        for i in range(cand["w"])
        for j in range(cand["h"])
    }


def on_base(cells, cols, rows):
    return {(c, r) for (c, r) in cells if 0 <= c < cols and 0 <= r < rows}


def transpose(col, row, orientation):
    """Vertical base transposes the grid: (col,row) -> (row,col)."""
    return (row, col) if orientation == "vertical" else (col, row)


def top_left_from_connector(kind, cc, cr, rot, cols, rows):
    """DS-122 matrix semantics: (cc, cr) is the CONNECTOR cell — the module's
    outward (deck-edge) corner; the module extends inward onto the grid.
    Edge-middle connectors are ambiguous between two corners; the cross-position
    rule picks: the corner indicator at rot 0 is TL for keys, TR for the others,
    the connector sits diagonally opposite, turned by rot (CCW positive).
    Mirrors topLeftOf() in clients/preview-web/app.js (DS-124)."""
    w, h = rotated_wh(kind, rot)
    cx, cy = (cols - 1) / 2.0, (rows - 1) / 2.0
    dx = -1 if cc < cx else 1 if cc > cx else 0
    dy = -1 if cr < cy else 1 if cr > cy else 0
    corners = [(0, 0), (w - 1, 0), (w - 1, h - 1), (0, h - 1)]  # TL, TR, BR, BL
    if dx != 0 and dy != 0:
        idx = 0 if dx < 0 and dy < 0 else 1 if dx > 0 and dy < 0 else 2 if dx > 0 else 3
    else:
        cands = [0, 1] if dy < 0 else [2, 3] if dy > 0 else [0, 3] if dx < 0 else [1, 2]
        kind0 = 0 if kind == "keys" else 1
        want = (kind0 + 3 * ((rot // 90) % 4) + 2) % 4
        idx = want if want in cands else cands[0]
    off = corners[idx]
    return cc - off[0], cr - off[1], w, h


def candidates_for(matrix, kind, slot, orientation, connector_semantics=False):
    """Expand the matrix entry "<kind>@<slot>" into candidate placements on the
    orientation's grid. Square 2x2 modules get all 4 in-place rotations.
    connector_semantics=True reads (col,row) as the connector cell (DS-122);
    False reads it as the footprint top-left (legacy inline matrices)."""
    entry = matrix.get(f"{kind}@{slot}")
    if entry is None:
        return []
    col, row = transpose(entry["col"], entry["row"], orientation)
    rots = [0, 90, 180, 270] if kind in SQUARE_KINDS else list(entry.get("rotations") or [entry.get("rot", 0)])
    cols, rows = grid_dims(orientation)
    out, seen = [], set()
    for rot in rots:
        if connector_semantics:
            c, r, w, h = top_left_from_connector(kind, col, row, rot, cols, rows)
        else:
            c, r = col, row
            w, h = rotated_wh(kind, rot)
        key = (c, r, rot)
        if key not in seen:
            seen.add(key)
            out.append({"col": c, "row": r, "rot": rot, "w": w, "h": h})
    return out


def connector_cell(cand, cols, rows):
    """Cross-position rule: the module's corner indicator sits diagonally across
    from its connector, so the connector cell is the module corner nearest the
    deck center (docs/discovery/salon-001/README.md)."""
    center = ((cols - 1) / 2.0, (rows - 1) / 2.0)
    corners = {
        (cand["col"], cand["row"]),
        (cand["col"] + cand["w"] - 1, cand["row"]),
        (cand["col"], cand["row"] + cand["h"] - 1),
        (cand["col"] + cand["w"] - 1, cand["row"] + cand["h"] - 1),
    }
    pool = corners & on_base(cells_of(cand), cols, rows) or on_base(cells_of(cand), cols, rows)
    return min(pool, key=lambda p: ((p[0] - center[0]) ** 2 + (p[1] - center[1]) ** 2, p[0], p[1]))


def bias_key(cand, cols, rows, orientation):
    """Deterministic candidate ranking: base-orientation bias first, then the
    cross-position rule (connector cell nearest the deck center), then
    coordinates/rotation for a total order."""
    center = ((cols - 1) / 2.0, (rows - 1) / 2.0)
    cc = connector_cell(cand, cols, rows)
    dist2 = (cc[0] - center[0]) ** 2 + (cc[1] - center[1]) ** 2
    orient_rank = 0 if (orientation == "horizontal") == (cand["rot"] % 180 == 0) else 1
    return (orient_rank, dist2, cand["col"], cand["row"], cand["rot"])


def _result(dev, cand=None, reason=None):
    out = {"kind": dev["kind"], "slot": dev["slot"]}
    if "connection" in dev:
        out["connection"] = dev["connection"]
    if cand is None:
        out["unplaced"] = True
        out["reason"] = reason
    else:
        w, h = cand["w"], cand["h"]
        out.update(
            col=cand["col"],
            row=cand["row"],
            rotation=cand["rot"],
            footprint={"w": w, "h": h},
            cells=sorted([c, r] for (c, r) in cells_of(cand)),
        )
    return out


def _area(kind):
    w, h = FOOTPRINTS[kind]
    return w * h


def solve(devices, orientation="horizontal", matrix=None, connector_semantics=False):
    """Place every device; returns one result dict per input device, in input
    order. Deterministic: same input -> same output."""
    matrix = REFERENCE_MATRIX if matrix is None else matrix
    cols, rows = grid_dims(orientation)
    n = len(devices)
    for d in devices:
        if d["kind"] not in FOOTPRINTS:
            raise ValueError(f"unknown module kind {d['kind']!r}")
    # Largest footprint first (keys 4x3 = 12 cells shrinks the solution space
    # most); ties broken by kind then slot for determinism.
    order = sorted(range(n), key=lambda i: (-_area(devices[i]["kind"]), devices[i]["kind"], devices[i]["slot"]))
    results = [None] * n
    occupied = set()
    used_slots = {}

    def valid_candidates(dev):
        cands = candidates_for(matrix, dev["kind"], dev["slot"], orientation, connector_semantics)
        valid, n_overlap, n_off = [], 0, 0
        for cand in cands:
            cells = cells_of(cand)
            if not on_base(cells, cols, rows):
                n_off += 1
                continue
            if cells & occupied:
                n_overlap += 1
                continue
            valid.append(cand)
        return cands, valid, n_overlap, n_off

    for i in order:
        dev = devices[i]
        kind, slot = dev["kind"], dev["slot"]
        if slot in used_slots:
            # one module per connector (crates/ds-vdev/src/hub.rs set_slot
            # rejects occupied slots the same way)
            results[i] = _result(dev, reason=f"connector {slot} already taken by {used_slots[slot]}")
            continue
        cands, valid, n_overlap, n_off = valid_candidates(dev)
        if not cands:
            results[i] = _result(dev, reason=f"no matrix entry for {kind}@{slot}")
            continue
        if not valid:
            if n_overlap:
                reason = f"no free candidate: all {len(cands)} candidate(s) for {kind}@{slot} overlap placed modules"
            else:
                reason = f"no candidate keeps >= 1 cell on the {cols}x{rows} base"
            results[i] = _result(dev, reason=reason)
            continue
        best = min(valid, key=lambda c: bias_key(c, cols, rows, orientation))
        occupied |= cells_of(best)
        used_slots[slot] = f"{kind}@{slot}"
        results[i] = _result(dev, cand=best)

    # Backtracking only if the greedy fit left a device unplaced; replace the
    # greedy result only when it places strictly more devices.
    if any(r.get("unplaced") for r in results):
        alt = _backtrack(devices, order, matrix, orientation, cols, rows, valid_candidates)
        if alt is not None:
            greedy_placed = sum(1 for r in results if not r.get("unplaced"))
            alt_placed = sum(1 for r in alt if not r.get("unplaced"))
            if alt_placed > greedy_placed:
                results = alt
    return results


def _backtrack(devices, order, matrix, orientation, cols, rows, valid_candidates, node_cap=BACKTRACK_NODE_CAP):
    """Bounded DFS over the same candidate sets in the same bias order; returns
    the first assignment placing the maximum number of devices (or None if the
    node cap is hit). Worst case (C+1)^n nodes with C <= 4 rotations per slot."""
    n = len(devices)
    best = {"count": -1, "assign": None}
    assign = [None] * n
    nodes = 0

    def rec(k, occupied, used_slots, count):
        nonlocal nodes
        nodes += 1
        if nodes > node_cap:
            return
        if count + (n - k) <= best["count"]:
            return
        if k == n:
            best["count"] = count
            best["assign"] = list(assign)
            return
        i = order[k]
        dev = devices[i]
        if dev["slot"] in used_slots:
            assign[i] = _result(dev, reason=f"connector {dev['slot']} already taken by {used_slots[dev['slot']]}")
            rec(k + 1, occupied, used_slots, count)
            return
        cands, valid, _, _ = valid_candidates(dev)
        if not cands:
            assign[i] = _result(dev, reason=f"no matrix entry for {dev['kind']}@{dev['slot']}")
            rec(k + 1, occupied, used_slots, count)
            return
        for cand in sorted(valid, key=lambda c: bias_key(c, cols, rows, orientation)):
            assign[i] = _result(dev, cand=cand)
            rec(k + 1, occupied | cells_of(cand), {**used_slots, dev["slot"]: f"{dev['kind']}@{dev['slot']}"}, count + 1)
        assign[i] = _result(dev, reason="backtrack: no candidate fits")
        rec(k + 1, occupied, used_slots, count)

    rec(0, set(), {}, 0)
    return best["assign"] if nodes <= node_cap else None


def main(argv):
    if len(argv) > 1:
        with open(argv[1]) as f:
            data = json.load(f)
    else:
        data = json.load(sys.stdin)
    orientation = data.get("orientation", "horizontal")
    matrix = data.get("matrix") or None
    placements = solve(data["devices"], orientation, matrix)
    cols, rows = grid_dims(orientation)
    out = {
        "orientation": orientation,
        "grid": {"cols": cols, "rows": rows},
        "placements": placements,
    }
    print(json.dumps(out, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
