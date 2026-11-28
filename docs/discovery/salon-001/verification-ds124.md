# DS-124 / SALON-003 verification — preview webapp deck map vs vdev permutations and the physical deck (webcam)

Date: 2026-11-28. Verifier: Mistral Vibe (verification agent, deckstack repo). Scope:
docs only — `placement-matrix.json`, code and tickets are untouched; disagreements are
reported, not fixed (see §5). Tickets: DS-124 (preview webapp solver-driven deck map),
DS-125 / SALON-003 (camera-oracle verification).

## 1. Environment and method

- Webapp: `clients/preview-web/` served by `serve.py` at `http://127.0.0.1:8080/`,
  opened as
  `http://127.0.0.1:8080/?token=dace73…a7fa6&port=39955`
  (driven with Playwright; deck placements read from the DOM: `g.module` transform →
  col/row with OX=OY=424, U=104, plus the `mod-label` "kind · slot N · R°" text and the
  "not on deck:" strip; `#rev` text for the WS rev counter).
- Vdev demo daemon: `target/debug/ds-vdev --socket /tmp/ds121-demo/vdev.sock` +
  `ds-daemon run --device virtual:/tmp/ds121-demo/vdev.sock --data-dir /tmp/ds121-demo
  --preview` on port 39955; control via
  `target/debug/ds-vdevctl --socket /tmp/ds121-demo/vdev.sock <verb>` (`slots`, `move
  <kind> <slot>`, `orientation …`, `replug`). Preview state polled via
  `GET /preview/api/state` with `Authorization: Bearer <token>`.
- Python reference solver: `tools/placement_solver.py` (DS-123 prototype);
  `python3 tools/test_placement_solver.py` → `--- 9840 checks, 0 failures ---`
  (re-run during this verification, PASS).
- No running daemon was restarted or killed. The vdev demo was moved through slots
  with `ds-vdevctl move` and restored to the DS-112 reference slots afterwards
  (final `slots`: base 1-4-4, keys 2-3, faders 1-1-3, knobs 1-2-3, rollers 3-3,
  dial 1-3-4).
- Live-update check: `window.__nore = 1` was set in the page before the move sequence
  and re-read after every config (`nore=1` in all records ⇒ no page reload; the map
  updated purely via `/preview/ws` events, `#rev` incremented on every event).

## 2. Task 1 — vdev permutations: webapp render vs python solver (PASS, mismatches recorded)

15 configurations were recorded (baseline + 9 `move`s to matrix-covered slots + 1
refused move + 1 move to a non-matrix slot + 2 orientation flips), plus a `replug`
disconnect probe. For each: `ds-vdevctl` move → poll `/preview/api/state` until `rev`
changed and the slot showed up (~2 s rescan tick) → read DOM → screenshot
(`verify-ds124-vdev-*.png`) → run `tools/placement_solver.py` on the same device set
(two arms: default inline `REFERENCE_MATRIX`, and the full
`clients/preview-web/placement-matrix.json` passed as `matrix`).

Results table (DOM render; `(col,row,rrot)` = top-left cell + rotation; OFF[...] =
"not on deck" strip reason; `mm` = mismatches vs python-default solver):

| config | move | rev | DOM placements | overlap | mm |
|---|---|---|---|---|---|
| cfg01 baseline (knobs@1-4-3) | — (knobs moved earlier) | 5→5 | keys (0,1,r0), faders (4,0,r0); OFF: dial@1-3-4 no matrix entry, knobs/rollers no fit | none | 1 |
| cfg02 | rollers→4-3 | 5→6 | same as cfg01, rollers@4-3 OFF no fit | none | 0 |
| cfg03 | keys→2-4 | 6→7 | keys (3,0,r90), rollers (1,0,r0); OFF: faders, dial, knobs | none | 3 |
| cfg04 | faders→4-2 | 7→8 | keys (3,0,r90), rollers (1,0,r0); OFF: faders@4-2, dial, knobs | none | 4 |
| cfg05 | dial→1-1-3 | 8→9 | keys (3,0,r90), rollers (1,0,r0); OFF: faders, dial@1-1-3, knobs | none | 4 |
| cfg06 | knobs→3-1 | 9→10 | same as cfg05 | none | 3 |
| cfg07 | rollers→1-2-3 | 10→11 | keys (3,0,r90) only; OFF: faders, dial, knobs, rollers | none | 2 |
| cfg08 | keys→2-3 | 11→12 | keys (0,1,r0), dial (4,2,r0); OFF: faders, knobs, rollers | none | 0 |
| cfg09 | faders→1-3-4 | 12→13 | same as cfg08 | none | 0 |
| cfg10 | knobs→3-4 | 13→14 | keys (0,1,r0), dial (4,2,r0), knobs (4,0,r0); OFF: faders, rollers | none | 1 |
| cfg11 | knobs→2-3 refused ("slot 2-3 is occupied by pid 0x01fe") | 14→14 | unchanged | none | 1 |
| cfg12 | dial→4-2 (no matrix entry) | 14→15 | keys (0,1,r0), knobs (4,0,r0); OFF: dial@4-2 no matrix entry, faders, rollers | none | 1 |
| cfg13 | orientation→vertical | 15→16 | keys (0,0,r0), faders (0,4,r90); OFF: dial, knobs, rollers; grid 4×6 | none | 2 |
| cfg14 | orientation→horizontal | 16→17 | keys (0,1,r0), knobs (4,0,r0); OFF: faders, dial, rollers | none | 1 |
| replug probe | `replug` | 19…24 | transient: 0 modules on deck, all 5 in strip with reason `disconnected`, then reconnect | none | — |

Assertions that hold for every config: **no overlaps** (cell-level check on DOM
placements), unplaced modules always listed in the "not on deck" strip with kind +
slot + reason, WS events update the map live without reload (rev increments
4→36 across the session, DOM `#rev` matches, `nore=1` throughout).

Recorded mismatches vs `tools/placement_solver.py` (default inline matrix), all
explained in §3:

- `rollers@3-3` (cfg01/02): python places (0,0,r90); webapp cannot place (connector
  (4,3) → derived cells (1,3) overlap keys) → OFF.
- `dial@1-3-4`: webapp matrix has **no `dial@1-3-4` entry** (57 entries, only
  `dial@1-1-3`) → OFF "no matrix entry"; python inline matrix has it.
- moved-slot entries (`keys@2-4`, `faders@4-2`, `faders@1-3-4`, `knobs@3-1`,
  `knobs@3-4`, `rollers@4-3`, `rollers@1-2-3`, …): webapp places them (full matrix),
  python inline says "no matrix entry" (its `REFERENCE_MATRIX` is a 12-entry
  subset) — coverage difference, not an algorithm disagreement.
- cfg13 vertical: `keys@2-3` webapp (0,0,r0) vs python (1,0,r0); `faders@1-3-4`
  webapp (0,4,r90) placed vs python no entry — transpose-semantics difference (§3).
- Where both solvers have the entry and place the module horizontally
  (`keys@2-3`, `faders@1-1-3`, `knobs@1-2-3`, `knobs@1-4-3`, `knobs@3-4`,
  `dial@1-1-3`), positions and rotations agree exactly.

Verdict task 1: **PASS** (render matches solver output wherever both have matrix
entries; no overlaps; off-deck strip correct; live WS updates proven) with the
mismatches above recorded.

## 3. Task 2 — JS (in-page) vs python solver cross-check

Method: the solver code was extracted **verbatim** from `clients/preview-web/app.js`
(the `// ---- deck map` … `// ---- frame stream` block: `MATRIX` = the served
`placement-matrix.json`, `FOOTPRINT`, `topLeftOf`, `solvePlacement`) and executed in a
JS harness over 678 cases: all subsets × all orderings of the vdev reference set
(knobs@1-2-3, keys@2-3, faders@1-1-3, rollers@3-3, dial@1-3-4; 325 orders × 2
orientations) plus the 14 recorded vdev configs × 2 orientations. The same cases were
run through `tools/placement_solver.py` (`solve()` imported in-process). First, the
harness was validated against the live page: DOM placements vs extracted-JS solver on
the 14 recorded configs → **0 mismatches**.

Result: 636 placed-module agreements, 756 both-unplaced agreements, **1358 divergent
module outcomes in 23 groups**. Top groups and root causes:

1. `POSITION keys@2-3 js=(0,0,r0) py=(1,0,r0)` ×270 — vertical only. The python
   solver transposes the matrix (col,row)→(row,col) and treats it as the footprint
   top-left; the webapp transposes the **connector cell** and then derives the
   top-left via `topLeftOf`. For connector-cell semantics these differ.
2. `dial@1-3-4 js=unplaced(no matrix entry) py=placed(...)` ×315 —
   `placement-matrix.json` has no `dial@1-3-4`, although `crates/ds-vdev/src/hub.rs`
   `reference_slot(Dial)` = 1-3-4 (the reference-unit dial slot) **and** the physical
   deck currently has the dial at 1-3-4 (§4). Matrix gap, not a solver bug.
3. `rollers@3-3`: js derives cells from connector (4,3) → (1,3,r90)/(0,4,r90);
   py inline entry (0,0) → (0,0,r90). Same slot, different cells → placed-vs-unplaced
   flips (×229 + ×220 + position diffs ×6). Same connector-cell vs top-left semantics
   difference as (1).
4. `faders@1-1-3` vertical js=(2,2,r0) py=(0,4,r0) ×49 — same transpose difference.
5. Moved-slot entries (`keys@2-4`, `knobs@3-4`, `rollers@4-3`, `faders@1-3-4`, …):
   js places (full 57-entry matrix), py inline "no matrix entry" — coverage.
6. Feeding the **full** `placement-matrix.json` to the python solver as `matrix`
   produces systematically different (wrong) positions, e.g. faders@1-1-3 → (5,3)
   instead of (4,0): the python solver interprets matrix (col,row) as the footprint
   top-left, while the DS-122 matrix (and the webapp) define it as the **connector
   cell**. The two solvers therefore only agree when fed semantically equivalent
   matrices (python's inline `REFERENCE_MATRIX` is in top-left coordinates).

Verdict task 2: **PASS with divergences reported** — the in-page JS solver is
deterministic and matches the live DOM exactly; divergences vs the python reference
solver are fully explained by (a) matrix coordinate semantics (connector cell vs
top-left), (b) the vertical transpose being applied before vs after the
connector→top-left derivation, and (c) matrix coverage (`dial@1-3-4` missing from
the webapp matrix; moved-slot entries missing from the python inline subset).
Recommendation (reported, not applied): give the python solver the connector-cell
semantics (or convert the matrix to top-left per rotation) so the two can be diffed
mechanically, and add `dial@1-3-4` to `placement-matrix.json`.

## 4. Task 3 — camera vs render on the physical deck

Capture: `ffmpeg -f v4l2 -input_format mjpeg -i /dev/video1 -frames:v 1 out.jpg`
(/dev/video1 = AUKEY; /dev/video0 loopback, /dev/video2 metadata — not used). Fresh
frame saved as `verify-ds124-cam-raw.jpg`; `rotate(-90)`/`rotate(+90)`/`rotate(180)`
variants were generated and compared with the previous capture `/tmp/deck-now.jpg`.

**Facing view**: the raw frame is the facing view — it matches the deck-map.svg
orientation (keys top-left landscape logo-bottom = matrix `keys@2-2` (0,0) r0; knobs
top-right = `knobs@1-4-3` (5,0)). The r90/l90 views put modules on side edges that
have no connectors in the map, so they are not the facing view. Base assumed flat
(horizontal); the old daemon exposes no orientation readout (see §6).

**Real deck slots — discoverability**: NOT discoverable from the old daemon:
`/run/user/1000/deckstack/state.json` has `"devices": {}` (installed daemon is
`deckstack-0.1.0` `ds-daemon run --device hid: --data-dir /run/user/1000/deckstack`
without `--preview`, pid 1928462 — DS-120/SALON-001 finding 4), and the salon-deck
app on port 8766 serves only a key-map UI (no slot/device data). The slots were
instead recovered from **sysfs** (the same source a hid: daemon uses; slot = USB devpath
minus `<bus>-<rootport>`):

| module | devpath | slot | matrix entry |
|---|---|---|---|
| base | 3-2.1.4.4 | 1-4-4 | — |
| keys (15xKey 2516:01fe) | 3-2.2.2 | 2-2 | yes, (0,0) r0 |
| knobs (3xKnob 2516:0202) | 3-2.1.4.3 | 1-4-3 | yes, (5,0) r90 |
| rollers (2xRoller 2516:0204) | 3-2.1.1.4 | 1-1-4 | yes, (2,2) r270 |
| dial (1xDial 2516:0206) | 3-2.1.3.4 | 1-3-4 | **MISSING** |
| faders (2516:0200) | — absent from the bus | — | physically **unplugged** |

The photo agrees: the faders module lies loose at the right edge, off the deck.

The vdev was then moved to exactly these slots (`move keys 2-2`, `move knobs 1-4-3`,
`move rollers 1-1-4`; dial already at 1-3-4; faders → `1-2-1`, a valid slot with no
matrix entry, as an off-deck stand-in because vdev has no per-module disconnect —
only global `replug`). Webapp render at rev 36: `verify-ds124-vdev-18-real-slots.png`;
annotated photo: `verify-ds124-cam-annotated.jpg`; side-by-side:
`verify-ds124-sidebyside-real-deck.jpg`.

Render vs photo per module (facing view, flat 6×4 grid):

| module@slot | webapp render | camera-observed | verdict |
|---|---|---|---|
| keys@2-2 | (0,0) r0, cells c0-3 r0-2 | top-left, landscape 5×3 keys, logo bottom, c0-3 r0-2 | **MATCH** |
| knobs@1-4-3 | (4,0) r0 (square in-place bias overrides the matrix's listed r90), cells c4-5 r0-1, triangle 1-top/2-3-bottom | top-right, c4-5 r0-1, triangle 1-top | **MATCH** (position and effective rotation) |
| rollers@1-1-4 | OFF deck ("connector taken / no fit": matrix (2,2) r270 → cells c2-5 r2 overlap keys) | **on deck**, bottom-left, cells c0-3 r3, horizontal strip (rot 90/270) | **MISMATCH** — matrix (2,2) wrong, camera says connector (0,3) |
| dial@1-3-4 | OFF deck ("no matrix entry") | **on deck**, bottom-right, cells c4-5 r2-3 (below the knobs) | **MISMATCH** — matrix entry missing, camera says connector (5,3) |
| faders (unplugged) | OFF deck (stand-in slot 1-2-1, "no matrix entry") | unplugged, lying loose at right edge | **CONSISTENT** (off-deck) |

Verdict task 3: **render-vs-photo = partial match** — keys and knobs match exactly
(position and rotation); rollers and dial are physically on the deck but missing
from the render because of matrix defects (§5). The full side-by-side is
`verify-ds124-sidebyside-real-deck.jpg`.

## 5. placement-matrix entries that disagree with camera evidence (REPORT ONLY — matrix not edited)

| entry | matrix value | camera-observed |
|---|---|---|
| `rollers@1-1-4` | col 2, row 2, rot 270 (cells c2-5 r2) | connector cell **(0,3)** bottom-left, cells **c0-3 r3**, horizontal (rot 90 or 270). The rollers sweep labeled 1-1-4 "rot270 middle-right + fader space bottom" — that round was flagged best-effort (README caveat); the camera shows bottom-left. |
| `dial@1-3-4` | **entry missing** (only `dial@1-1-3` exists) | dial seated at bottom-right connector **(5,3)**, cells c4-5 r2-3; knob sweep table has bottom-right r90 = 1-3-4, consistent. Rotation not determinable from the photo (dial screen blank); 2×2 so all rotations are in-place candidates anyway. |
| `keys@2-2` | col 0, row 0, rot 0 | **confirmed** by camera: keys fully on-deck at c0-3 r0-2 (not the "overhanging top-left corner" alternative reading in the README). |

Note: `knobs@1-4-3` lists rot 90, but both solvers treat 2×2 modules as freely
rotatable and the horizontal bias picks rot 0, which is what the camera shows
(triangle 1-top) — so the render matches the photo despite the listed rot.

## 6. Blockers for real-deck `/preview` verification

- The installed daemon driving the real deck is the **0.1.0 alpha without
  `--preview`** (`/home/catwars/Applications/deckstack-0.1.0/bin/ds-daemon run
  --device hid: --data-dir /run/user/1000/deckstack`, pid 1928462); its
  `/devices` is empty (`/run/user/1000/deckstack/state.json`), so the preview webapp
  has no slot/position/orientation input on the real deck. DS-120 (any unplug
  restarts the daemon) makes this worse. **Until a `--preview` daemon with DS-112
  slot reporting runs on `hid:`, the webapp cannot be verified against the live
  real-deck daemon**; this session worked around it by reading sysfs and replaying
  the same slots on the vdev (§4).
- Base orientation on the real deck is not readable without touching hidraw
  (out of scope); the flat/horizontal assumption is inferred from the photo.
- vdev has no per-module disconnect (only global `replug`), so "faders unplugged"
  was emulated with a non-matrix slot (1-2-1) in the render-vs-photo comparison.

## 7. Evidence files (all under `docs/discovery/salon-001/`)

- `verify-ds124-vdev-00-baseline.png` … `verify-ds124-vdev-18-real-slots.png` —
  full-page webapp screenshots per vdev config (00 baseline, 01–10 moved configs,
  12 unknown-slot, 13/14 orientation flips, 15 replug, 16 disconnect window,
  17 restored baseline, 18 vdev at the real deck's slots, rev 36).
- `verify-ds124-cam-raw.jpg` — fresh `/dev/video1` frame (facing view).
- `verify-ds124-cam-annotated.jpg` — raw frame with 6×4 grid overlay and per-module
  labels (deduced cells).
- `verify-ds124-sidebyside-real-deck.jpg` — annotated camera frame (left) vs webapp
  render at the real slots (right).

## 8. Commands run (selection)

```
python3 tools/test_placement_solver.py                 # 9840 checks, 0 failures
target/debug/ds-vdevctl --socket /tmp/ds121-demo/vdev.sock slots|move <kind> <slot>|orientation <h|v>|replug
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:39955/preview/api/state
echo '{"devices":[...],"orientation":"horizontal"}' | python3 tools/placement_solver.py
ffmpeg -f v4l2 -input_format mjpeg -i /dev/video1 -frames:v 1 cam-raw.jpg
for d in /sys/bus/usb/devices/*; do … idVendor==2516 → devpath → slot …   # sysfs slot discovery
# browser: Playwright navigate to http://127.0.0.1:8080/?token=…&port=39955,
# evaluate DOM extraction, take_screenshot per config (see §1)
```
