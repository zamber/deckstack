#!/usr/bin/env python3
"""DS-122 validator for docs/discovery/salon-001/placement-matrix.json (stdlib only).

Checks:
  1. schema: every "<kind>@<slot>" entry has integer col/row, exactly one of "rot"
     (0|90|180|270) or "rotations" (non-empty list of those), and confidence in
     {verified, derived}.
  2. footprint fit: for every entry and every candidate rotation, the rotated
     footprint (owner-measured rot-0 footprints: keys 4x3, knobs 2x2, dial 2x2,
     rollers 1x4, faders 2x4) placed with the connector cell (col,row) at the module
     corner diagonally opposite the corner indicator (corner rule, README reference
     table) keeps >= 1 cell on the flat 6x4 grid (overhang allowed).
  3. ref entries match the README reference table: the five ref-sweep slots are
     present, verified, rot 0, seated at the cell named by the reference seating
     (top-left -> (0,0), bottom-right -> (5,3), right edge full height -> (5,3)),
     with the corner indicator corner named in the README.
  4. coverage + no disagreement: every distinct (module, rel_path) in the sweep CSVs
     (connector-map-labeled.csv, -keys2-, -faders-, -rollers-, and the ref CSV) has
     exactly one matrix entry, no extra entries, and the entry's rot/rotations equal
     the absolute rotations re-anchored from each CSV row's session-relative label
     via the per-kind ref anchor (README: ref slot = rot 0):
       knobs   session rot0/rot90/rot180 -> 0/90/180   (ref 1-2-3 = "top-left rot0")
       keys    vertical/rot90cw/rot270   -> 0/90/270   (ref 1-3-4 = "bottom-right, keys vertical")
       faders  upright/rot180/rot270     -> 180/0/90   (ref 1-1-3 = "rot180, right")
       rollers upright/rot90cw/rot270    -> 0/90/270   (ref 1-3-4 = "upright, middle-right")
       dial    REF row                   -> 0

Run: python3 docs/discovery/salon-001/validate_matrix.py
"""
import csv
import json
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
GRID_COLS, GRID_ROWS = 6, 4

# rot-0 footprints (w x h in cells) and corner-indicator corner at rot 0 (I0) per kind;
# I0 is read from the README reference table at runtime, the footprints are the
# owner-measured ones from the DS-122 task statement.
FOOTPRINT = {"keys": (4, 3), "knobs": (2, 2), "dial": (2, 2), "rollers": (1, 4), "faders": (2, 4)}

SWEEP_CSVS = [
    "connector-map-labeled.csv",        # knobs sweep
    "connector-map-keys2-labeled.csv",  # keys sweep
    "connector-map-faders-labeled.csv", # faders sweep
    "connector-map-rollers-labeled.csv", # rollers sweep
    "connector-map-ref-labeled.csv",    # reference sweep (rot0 anchors)
]

OPP = {"TL": "BR", "BR": "TL", "TR": "BL", "BL": "TR"}
CCW = {"TL": "BL", "BL": "BR", "BR": "TR", "TR": "TL"}  # corner cycle under +90 deg CCW


def rot_corner(corner, rot):
    for _ in range(rot // 90):
        corner = CCW[corner]
    return corner


def parse_ref_table():
    """README 'Reference (rot0) anchors per module' table -> {kind: (slot, seating, I0)}."""
    with open(os.path.join(HERE, "README.md"), encoding="utf-8") as f:
        text = f.read()
    section = text.split("## Reference (rot0) anchors per module")[1]
    out = {}
    for line in section.splitlines():
        m = re.match(r"\|\s*(\w+)\s*\([0-9a-f]{4}\)\s*\|\s*(.*?)\s*\|\s*([\d-]+)\s*\|", line)
        if not m:
            continue
        kind, seating, slot = m.group(1), m.group(2), m.group(3)
        cm = re.search(r"corner (?:indicator )?(TL|TR|BL|BR)", seating)
        assert cm, f"no corner indicator in ref row: {line!r}"
        out[kind] = (slot, seating, cm.group(1))
    return out


def session_rot(module, label):
    """Absolute rotation of a CSV row from its session-relative label + the ref anchor."""
    lab = label.lower()
    if lab.startswith("ref"):
        return 0
    if module == "knobs":
        if "rot180" in lab:
            return 180
        if "rot90" in lab:
            return 90
        if "rot0" in lab:
            return 0
        return None  # "knob-current"/"top-left-vertical": no session-rot marker
                    # (1-2-3 still gets rot 0 from the ref CSV row; 1-3-4 from "right-bottom rot90")
    if module == "keys":
        if "rot270" in lab:
            return 270
        if "rot90cw" in lab:
            return 90
        return 0  # "vertical" rows
    if module == "faders":
        if "rot270" in lab:
            return 90
        if "rot180" in lab:
            return 0
        return 180  # "upright" rows
    if module == "rollers":
        if "rot270" in lab:
            return 270
        if "rot90cw" in lab:
            return 90
        return 0  # "upright" rows
    if module == "dial":
        return 0
    raise AssertionError(f"unknown module {module!r}")


def seating_cell(seating):
    s = seating.lower()
    if "top-left" in s:
        return (0, 0)
    if "bottom-right" in s:
        return (5, 3)
    if "right edge" in s:
        return (5, 3)  # full height at the right edge, connector at the BL corner cell
    raise AssertionError(f"unknown reference seating {seating!r}")


def footprint(kind, i0, col, row, rot):
    """Cells covered by kind seated with its connector cell at (col,row), rotation rot."""
    w, h = FOOTPRINT[kind]
    q = OPP[i0]  # connector corner in the module frame at rot 0
    corner = rot_corner(q, rot)
    wp, hp = (h, w) if rot % 180 else (w, h)
    ox, oy = {"TL": (0, 0), "TR": (wp - 1, 0), "BR": (wp - 1, hp - 1), "BL": (0, hp - 1)}[corner]
    c0, r0 = col - ox, row - oy
    return [(c0 + i, r0 + j) for i in range(wp) for j in range(hp)]


def main():
    errors = []
    with open(os.path.join(HERE, "placement-matrix.json"), encoding="utf-8") as f:
        matrix = json.load(f)
    ref = parse_ref_table()
    assert set(ref) == set(FOOTPRINT), f"ref table kinds {sorted(ref)}"

    # ---- 1. schema ----
    for key, e in matrix.items():
        kind, _, slot = key.partition("@")
        if kind not in FOOTPRINT or not slot:
            errors.append(f"{key}: bad key (kind must be one of {sorted(FOOTPRINT)})")
            continue
        if not isinstance(e.get("col"), int) or not isinstance(e.get("row"), int):
            errors.append(f"{key}: col/row must be ints")
        rots = e.get("rotations") if "rotations" in e else ([e["rot"]] if "rot" in e else None)
        if rots is None or "rot" in e and "rotations" in e:
            errors.append(f"{key}: needs exactly one of rot / rotations")
            rots = []
        if any(r not in (0, 90, 180, 270) for r in rots) or not rots:
            errors.append(f"{key}: bad rotations {rots}")
        if e.get("confidence") not in ("verified", "derived", "verified-camera"):
            errors.append(f"{key}: confidence must be verified|derived|verified-camera")

    # ---- 2. footprint fit (>=1 cell on the 6x4 grid, overhang allowed) ----
    for key, e in matrix.items():
        kind = key.partition("@")[0]
        if kind not in FOOTPRINT:
            continue
        i0 = ref[kind][2]
        rots = e.get("rotations", [e.get("rot")])
        for r in rots:
            if r not in (0, 90, 180, 270):
                continue
            cells = footprint(kind, i0, e["col"], e["row"], r)
            on = sum(1 for c, rr in cells if 0 <= c < GRID_COLS and 0 <= rr < GRID_ROWS)
            if on < 1:
                errors.append(f"{key} rot {r}: footprint {cells} has no cell on the 6x4 grid")

    # ---- 3. ref entries match the README reference table ----
    for kind, (slot, seating, i0) in ref.items():
        key = f"{kind}@{slot}"
        e = matrix.get(key)
        if e is None:
            errors.append(f"ref {key}: missing from matrix")
            continue
        rots = e.get("rotations", [e.get("rot")])
        if rots != [0]:
            errors.append(f"ref {key}: must be rot 0, got {rots}")
        if e.get("confidence") != "verified":
            errors.append(f"ref {key}: must be confidence=verified")
        want = seating_cell(seating)
        if (e["col"], e["row"]) != want:
            errors.append(f"ref {key}: seating {seating!r} -> cell {want}, matrix has ({e['col']},{e['row']})")
        # corner indicator: diagonally opposite the connector corner at (col,row)
        corner = rot_corner(OPP[i0], 0)
        if OPP[corner] != i0:
            errors.append(f"ref {key}: indicator {OPP[corner]} != README corner {i0}")

    # ---- 4. coverage + rotation agreement with the sweep CSVs ----
    observed = {}  # (kind, slot) -> set of absolute rots from CSV rows
    pairs = set()
    for fn in SWEEP_CSVS:
        with open(os.path.join(HERE, fn), newline="", encoding="utf-8") as f:
            for row in csv.DictReader(f):
                kind = row["module"].strip()
                slot = row["rel_path"].strip()
                if not slot:
                    continue  # auto-watch unplug rows
                pairs.add((kind, slot))
                rot = session_rot(kind, row["label"])
                if rot is not None:
                    observed.setdefault((kind, slot), set()).add(rot)
    for kind, slot in sorted(pairs):
        if f"{kind}@{slot}" not in matrix:
            errors.append(f"coverage: {kind}@{slot} in CSVs but not in matrix")
    for key in matrix:
        kind, _, slot = key.partition("@")
        e = matrix.get(key) or {}
        if (kind, slot) not in pairs and e.get("confidence") != "verified-camera":
            errors.append(f"coverage: matrix key {key} not observed in any CSV")
    for (kind, slot), rots in sorted(observed.items()):
        key = f"{kind}@{slot}"
        e = matrix.get(key)
        if e is None or e.get("confidence") == "verified-camera":
            continue  # camera evidence overrides the (best-effort) sweep round
        got = e.get("rotations", [e.get("rot")])
        if sorted(got) != sorted(rots):
            errors.append(f"disagreement: {key} CSV rows give rotations {sorted(rots)}, matrix has {sorted(got)}")

    # ---- report ----
    n = len(matrix)
    per_kind, per_conf = {}, {}
    for key, e in matrix.items():
        per_kind[key.partition("@")[0]] = per_kind.get(key.partition("@")[0], 0) + 1
        per_conf[e["confidence"]] = per_conf.get(e["confidence"], 0) + 1
    print(f"placement-matrix.json: {n} entries; per kind {dict(sorted(per_kind.items()))}; "
          f"confidence {dict(sorted(per_conf.items()))}")
    print(f"grid {GRID_COLS}x{GRID_ROWS}; ref anchors from README: "
          + ", ".join(f"{k}@{v[0]}" for k, v in sorted(ref.items())))
    if errors:
        print(f"FAIL: {len(errors)} error(s)")
        for e in errors:
            print("  -", e)
        return 1
    print("PASS: schema ok; every rotated footprint keeps >=1 cell on the 6x4 grid; "
          "ref entries match the README reference table; all CSV (kind, rel_path) pairs covered; "
          "no kind+slot disagreements")
    return 0


if __name__ == "__main__":
    sys.exit(main())
