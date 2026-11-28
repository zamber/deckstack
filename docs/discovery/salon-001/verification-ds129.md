# DS-129 verification — end-to-end preview-webapp loop on the vdev (attach/reattach → daemon events → /preview/ws → solver → render + frame stream) + perf, and a read-only live pass

Date: 2026-11-28 (session ~16:45–17:15 CET). Verifier: Mistral Vibe (verification agent,
deckstack repo). Scope: **test tooling + docs only** — `clients/preview-web/tests/e2e_vdev.py`
(new), this document, and `ds129-*` captures. `app.js` and all other code untouched;
observations about the DS-126/127 implementation are reported in §6, not fixed.
Tickets: DS-129 (this e2e), DS-126 (frame-stream perf / no flicker / replug re-render),
DS-127 (styling), context DS-124/DS-125.

## 1. Environment and method

- Webapp: `clients/preview-web/` served by `serve.py` at `http://127.0.0.1:8080/`, opened
  as `http://127.0.0.1:8080/?token=<token>&port=<port>` in headless chromium
  (python-playwright 1.65.2, chromium from `~/.cache/ms-playwright`, `--no-sandbox`).
- vdev demo daemon: socket `/tmp/ds121-demo/vdev.sock`, preview port **39955**, token
  `dace7377…a7fa6` (verification-ds124.md §1). Control:
  `target/debug/ds-vdevctl --socket /tmp/ds121-demo/vdev.sock <slots|move <kind> <slot>|replug>`.
  Live state read via `GET /preview/api/state` (Bearer). Nothing was restarted or killed.
- Live real deck (read-only): preview port **43541**, token `a3d87b28…e7769`; only
  `GET /preview/api/state` and the webapp page were used — no writes, no hardware touch.
- Expected placements: `tools/placement_solver.py solve(devices, orientation,
  matrix=clients/preview-web/placement-matrix.json (full, 58 entries),
  connector_semantics=True)` run in-process over the daemon-reported `/devices`,
  mirroring the in-page JS solver (connector-cell semantics, `topLeftOf`). Disconnected
  devices are expected in the "not on deck" strip with reason `disconnected` (app.js never
  draws them as `g.module`).
- DOM read from the page: `g.module` transform (OX=OY=424, U=104 → col,row), `rect.body`
  size → w,h cells, `.mod-label` `"kind · slot N · R°"`, the "not on deck:" strip text,
  `#conn`, `#rev`, `window.__renderCount`.
- No-reload proof: `window.__nore = 1` armed after load, asserted after every step.
- Flicker watch: an init script wraps `WebSocket` (timestamps every `preview.frame` and
  every `/devices` event, plus the first post-event task) and runs an rAF sampler that
  records any `image.frameshot` whose `href` is empty after having been set
  (`window.__flicker`); additionally the 30 s streaming window samples every 100 ms.
- Perf timestamps: `window.__helloAt` / `window.__frameFirstAt` (app.js hooks),
  `window.__moveSentAt = performance.now()` injected immediately before each
  `ds-vdevctl move` and read back right after the DOM matched (cmd→DOM).
- The scripted sequence starts from the vdev reference slots (pre-flight restore if
  needed) and restores them at the end (`finally`), verified by `ds-vdevctl slots`:
  base 1-4-4, keys 2-3, faders 1-1-3, knobs 1-2-3, rollers 3-3, dial 1-3-4.

**How to re-run** (repo root; vdev demo + `serve.py` on 8080 running):

```
python3 -m pip install --user playwright && python3 -m playwright install chromium   # once
python3 clients/preview-web/tests/e2e_vdev.py                # vdev e2e + live read-only pass
python3 clients/preview-web/tests/e2e_vdev.py --vdev-only    # or --live-only
python3 clients/preview-web/tests/e2e_vdev.py --out /tmp/ds129.json   # + JSON dump
```

Exit code 0 = all assertions PASS (observed: `DS-129 overall: PASS`, `EXIT=0`).
Screenshots land in `docs/discovery/salon-001/ds129-*.png`. Env overrides:
`DS129_VDEV_SOCK/PORT/TOKEN`, `DS129_LIVE_PORT/TOKEN`, `DS129_WEBAPP`, `DS129_SHOTS`.

## 2. Scripted sequence and results (vdev, port 39955) — ALL PASS

Sequence (the ticket's example targets, reordered only where a target slot was still
occupied — faders is moved off-deck first and the dial shifts to 1-1-3 to free 1-3-4 for
the keys move): baseline → `move faders 1-2-1` → `move dial 1-1-3` → `move keys 2-2` →
`move keys 1-3-4` → `move rollers 1-1-4` → `move knobs 1-4-3` → 30 s streaming with 6
concurrent moves → `replug` → restore moves (keys 2-3, dial 1-3-4, faders 1-1-3,
rollers 3-3, knobs 1-2-3) → final slots check.

Assertions per step (all PASS at every step): **dom==solver** (DOM placements ==
`placement_solver.py` with the full matrix + connector semantics, per (kind,slot):
col,row,rot,w,h), **no-overlap** (cell-level), **offdeck-strip** (strip lists exactly the
unplaced modules with the right reason), **no-reload** (`__nore=1`), **ws-connected**,
**no-flicker** (`__flicker` empty), screenshot taken. Plus per-step extras:
`move-accepted` + `state-published` (the daemon's `/preview/api/state` really showed the
new slot before the DOM was accepted — guards against vacuous matches), and for the
replug: `replug-reconnected` + `replug-transient-seen`.

| step | cmd | rev | DOM placements (col,row,rot) | off-deck strip | result |
|---|---|---|---|---|---|
| 00-baseline | open | 106 | keys 2-3 (0,1,r0), faders 1-1-3 (4,0,r0) | dial 1-3-4, knobs 1-2-3, rollers 3-3 (no fit) | **PASS** |
| 01 | move faders 1-2-1 | 107 | keys (0,1), dial 1-3-4 (4,2,r0) | faders 1-2-1 (no matrix entry), knobs, rollers (no fit) | **PASS** |
| 02 | move dial 1-1-3 | 108 | keys (0,1), dial 1-1-3 (4,2,r0) | faders 1-2-1 (no matrix entry), knobs, rollers (no fit) | **PASS** |
| 03 | move keys 2-2 | 109 | keys 2-2 (0,0,r0), dial (4,2) | faders, knobs, rollers (no fit) | **PASS** |
| 04 | move keys 1-3-4 | 110 | keys 1-3-4 (2,1,r0), knobs 1-2-3 (0,0,r0) | faders (no matrix entry), dial 1-1-3, rollers 3-3 (no fit) | **PASS** |
| 05 | move rollers 1-1-4 | 111 | keys (2,1), knobs (0,0) | faders, dial, rollers 1-1-4 (no fit) | **PASS** |
| 06 | move knobs 1-4-3 | 112 | keys (2,1) only | faders, dial, knobs 1-4-3, rollers (no fit) | **PASS** |
| 07 | 30 s stream + 6 concurrent moves | 118 | keys (2,1) (settled after stream moves) | as 06 | **PASS** |
| 08 | `replug` | 123 | keys (2,1) after reconnect; transient: all 5 in strip `(disconnected)`, observed in both `/preview/api/state` and the DOM strip | — | **PASS** (12 ws /devices events, reconnect within 1.53 s) |
| 09 ×5 | restore moves | 124–128 | progressively back to baseline | — | **PASS** each |
| 10-restored | `ds-vdevctl slots` | 128 | keys 2-3 (0,1), faders 1-1-3 (4,0) = baseline | dial, knobs, rollers (no fit) | **PASS**, `slots == reference` |

Screenshots: `ds129-00-baseline.png` … `ds129-10-restored.png` (per step, full page),
`ds129-08-replug.png` (post-replug re-render).

## 3. Perf measurements (DS-126 acceptance)

| metric | value | note |
|---|---|---|
| (a) page-open → `preview.hello` (ws) | **20.9 ms** (in-page, since timeOrigin) | |
| (a) page-open → first frame applied (`__frameFirstAt`) | **24.5 ms** in-page; **228 ms** wall-clock from `goto()` (incl. browser nav + page load) | DS-126 "frame stream starts < 1 s after page open" → **PASS**; polling starts on `preview.hello`, before the snapshot |
| (b) cmd→DOM per move (timestamp injected before `ds-vdevctl move`, read after DOM matched) | 904.9, 906.0, 907.0, 908.2, 1960.1, 1963.1 ms (moves 01–06); replug 1530.6 ms; restores 938.8–1999.1 ms | dominated by the **daemon-side** rescan tick (`RESCAN_BUSY` = 1 s, `crates/ds-daemon/src/device.rs`) — not the webapp |
| (b) ws `/devices` event → DOM re-render | **0.9–1.4 ms** per move (replug 1.1 ms) | DS-126 "re-render within one event round-trip" → **PASS** (`onDevicesEvent` renders synchronously, no debounce) |
| (c) 30 s streaming with 6 concurrent moves | 297 samples @100 ms, **0 blank-screen samples, 0 flicker events**; keys frames: 277 responses, inter-response gap min 138.4 / mean 140.0 / max 141.2 ms (= `FRAME_POLL_MS` 140); 1 distinct sha (the vdev keys screen is static) | dial target: 277 responses, all empty (vdev serves no dial frame — see §6) |
| (d) total re-renders | `window.__renderCount` = **50** for the whole session (2 at open; +12 during the 30 s stream = 6 moves × 2 events; 29 `/devices` ws events total; 714 frame responses) | no render storm: one render per event |

## 4. Live deck read-only pass (port 43541) — PASS

Opened `http://127.0.0.1:8080/?token=a3d87b28…e7769&port=43541` (no writes; only
`GET /preview/api/state`, rev 608579, 4 devices, flat). DOM == solver(live devices, full
matrix, connector semantics):

| module | live state | DOM render | verdict |
|---|---|---|---|
| keys@2-2 | connected | (0,0) 4×3 r0 | **MATCH** |
| rollers@1-1-4 | connected | (0,3) 4×1 r90 | **MATCH** |
| dial@1-3-4 | connected | (4,2) 2×2 r0 | **MATCH** |
| knobs@1-4-3 | **disconnected** (owner physically reseating it) | not drawn; strip: `knobs · slot 1-4-3 (disconnected)` | **MATCH — off-deck strip handles the disconnected module correctly** |

No overlaps, ws connected, `__nore=1`, no flicker. Screenshot: `ds129-live.png`.

## 5. Verdict

- **DS-129: PASS** — 14/14 steps PASS, exit code 0. The full loop
  (attach/reattach → `ds-vdevctl` → daemon rescan → `/devices` ws events → in-page solver
  → DOM render, plus the frame stream) was verified end-to-end at known attachment and
  re-attachment points on the vdev, including `replug` (transient all-disconnected window
  observed in state and DOM, clean remount after reconnect, no stale modules), with DOM ==
  `tools/placement_solver.py` (full matrix, `connector_semantics=True`) at every step,
  zero overlaps, correct off-deck strip, no page reload, and no screen flicker.
- **DS-126 acceptance: PASS** on the vdev and live — frame stream starts 24.5 ms after
  page open (< 1 s), no blank screen across 50 re-renders and a 30 s stream with concurrent
  moves (cached frame re-applied on every `renderDeck()`), replug remounts within one event
  round-trip (ws→DOM 0.9–1.4 ms), reconnect completes within the ≤ 5 s daemon budget.
- vdev reference slots restored and verified (`ds-vdevctl slots` = base 1-4-4, keys 2-3,
  faders 1-1-3, knobs 1-2-3, rollers 3-3, dial 1-3-4).

## 6. Observations / bugs in the DS-126/127 implementation (REPORT ONLY — not fixed)

1. **vdev serves no frame for the `dial` target**: 277/277 `frame` responses for
   `target=dial` came back with `sha256=null`, no data, so the dial `<image>` never gets an
   href on the vdev (the keys screen streams fine). The no-flicker guarantee for the dial
   screen is therefore only exercised on the live deck. Not a webapp bug (the cache/re-apply
   logic is kind-agnostic); likely a vdev frame-source gap.
2. **cmd→DOM latency is daemon-bound**: 0.9–2.0 s per move is the daemon's 1 s rescan tick
   (`RESCAN_BUSY`), not the webapp (ws→DOM is ~1 ms). If sub-second move→paint is ever
   required, the rescan must become event-driven (the vdev already emits a
   `/deck/device/<id>/moved` event that the webapp receives — see 3).
3. **The webapp receives `/deck/device/<id>/moved` events but does not re-render on them**:
   they fall through to `renderSoon()` (60 ms debounce) which re-renders from `state.devices`
   — unchanged until the rescan publishes `/devices/<serial>`. A re-render keyed on the
   `moved` payload would cut move→paint to ~event time, but would need the new slot in the
   payload (currently it carries position, not slot). Improvement suggestion only.
4. **`app.js` defines `setPath` and `baseOrientation` twice** (the later definition wins);
   harmless today but a latent hazard. Cosmetic, report-only.
5. **`tools/placement_solver.py` backtracking is inert**: `_backtrack` receives
   `valid_candidates`, which closes over the greedy `occupied` set, so backtrack candidates
   are pre-filtered against the final greedy placements and it can never place strictly
   more than the greedy pass. Python and the (greedy-only) webapp JS therefore agree by
   construction — but if that closure were fixed, the python solver could diverge from the
   webapp for some device sets (the webapp has no backtracking). Report-only; `tools/` is
   outside this ticket's commit scope.
6. Pre-existing DS-125 note (knob `fill` serializing an LED object) is already fixed by
   DS-126's `ledColor()` helper — re-checked, knobs render with the LED color.

## 7. Evidence files (all under `docs/discovery/salon-001/`)

- `ds129-00-baseline.png` … `ds129-10-restored.png` — full-page webapp screenshots at
  every scripted step (00 baseline, 01–06 moves, 07 after the 30 s stream, 08 post-replug,
  09 restore moves, 10 restored reference slots).
- `ds129-live.png` — read-only live-deck pass (port 43541): keys/rollers/dial placed,
  knobs in the off-deck strip as `disconnected`.
- `clients/preview-web/tests/e2e_vdev.py` — the repeatable e2e script (see §1 for how to
  run; header documents requirements and env overrides).

## 8. Commands run (selection)

```
python3 clients/preview-web/tests/e2e_vdev.py --out /tmp/ds129.json     # DS-129 overall: PASS, EXIT=0
target/debug/ds-vdevctl --socket /tmp/ds121-demo/vdev.sock slots        # base 1-4-4 keys 2-3 faders 1-1-3 knobs 1-2-3 rollers 3-3 dial 1-3-4 (restored)
target/debug/ds-vdevctl --socket /tmp/ds121-demo/vdev.sock move <kind> <slot>   # scripted moves
target/debug/ds-vdevctl --socket /tmp/ds121-demo/vdev.sock replug
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:39955/preview/api/state   # vdev state (read)
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:43541/preview/api/state   # live state (read-only)
# in-page: window.__nore=1, WebSocket wrapper (__wsDevTimes/__frameLog), rAF flicker sampler,
# __moveSentAt before each move, DOM extraction (g.module transform/label, strip, #rev, __renderCount)
```
