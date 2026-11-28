# DS-125 live verification — preview webapp render vs the real MasterHub deck (hid: daemon with `--preview`, camera-oracle)

Date: 2026-11-28 (session 15:07–15:55 CET). Verifier: Mistral Vibe (verification agent,
deckstack repo). Scope: docs only — evidence under `docs/discovery/salon-001/`, ticket
update under `docs/tickets/salon/`; no code, matrix, or hardware state changed (the one
control write was restored to its original value). Tickets: DS-125, SALON-003
(DS-122/DS-123/DS-124 context). Nothing was restarted, killed, unplugged or reseated;
the webcam was only read.

## 1. Environment and method

- **Daemon (live, real deck)**: pid 3241858, started 2026-11-28 15:07:18 CET —
  `/home/catwars/Applications/deckstack-0.1.0/bin/ds-daemon run --device hid:
  --data-dir /run/user/1000/deckstack --preview`; preview surface
  `http://127.0.0.1:43541/preview` (`/run/user/1000/deckstack/http.port` = 43541),
  bearer token = `/run/user/1000/deckstack/token`
  (`a3d87b28…e7769`). CLI used for writes/reads:
  `/home/catwars/Applications/deckstack-0.1.0/bin/ds --data-dir /run/user/1000/deckstack`.
- **Webapp**: `clients/preview-web/` served by `serve.py` at `http://127.0.0.1:8080/`,
  opened with Playwright as
  `http://127.0.0.1:8080/?token=a3d87b28…e7769&port=43541` (viewport 1680x1500).
  Placements read from the DOM: `g.module` `transform="translate(x,y)"` with
  OX=OY=424, U=104 (CELL=96+GAP=8, M=4) → col=(x−424)/104, row=(y−424)/104; size from
  `rect.body` (w×h cells); `.mod-label` text `"kind · slot N · R°"`; "not on deck:"
  strip text; `#conn`, `#rev`, `#orientation`; inspector `#inspector-body`.
- **Live state source**: `GET /preview/api/state` (Bearer token) and
  `ds get /devices --json`; cross-checked against sysfs USB devpaths
  (slot = devpath minus `3-2.` prefix).
- **Camera**: `/dev/video1` = AUKEY PC—W1 (0c45:2690, usb 1-4; /dev/video2 = metadata),
  `ffmpeg -f v4l2 -input_format mjpeg -i /dev/video1 -frames:v 1 …`. The raw frame is
  the facing view (per verification-ds124.md §4; re-confirmed below).
- **No-reload proof**: `window.__nore = 1` armed in the page before the live events and
  a 100 ms recorder collected every distinct `#rev` text; both re-read after each event.

## 2. Live deck state (daemon `/preview/api/state`, first read rev 553217; sysfs agrees)

| serial | kind | slot (sysfs devpath) | usb pid | orientation | connection |
|---|---|---|---|---|---|
| MHBS01AA461251603330 | m0-base | 1-4-4 (3-2.1.4.4) | 2516:01f6 | horizontal | connected |
| MHKD15AA431251602831 | keys | 2-2 (3-2.2.2) | 2516:01fe | horizontal | connected |
| MHKB03AA221252301767 | knobs | 1-4-3 (3-2.1.4.3) | 2516:0202 | horizontal | connected |
| MHRR02AA411251602062 | rollers | 1-1-4 (3-2.1.1.4) | 2516:0204 | horizontal | connected |
| MHDD01AA221252301435 | dial | 1-3-4 (3-2.1.3.4) | 2516:0206 | horizontal | connected |
| — | faders | **absent from the USB bus** (no 2516:0200) | — | — | unplugged |

Base flat (horizontal) per the daemon readout; the photo confirms the facing/flat view.

## 3. Task 1 — webapp render vs expected cells (PASS)

Page: `#conn` = **"connected"** (class conn-up), `#rev` = "rev 554367 · ws" at first read
and incrementing on every read (554367 → 554467 → 555037 → 556201 → 556442 → 556657;
~2 rev/s heartbeat), `#orientation` = "orientation: flat (6×4)", slab label
"MasterHub base 6×4".

DOM placements (assertion run in-page; ALL_MATCH = true):

| module | transform | (col,row) | w×h cells | cells | label | expected | verdict |
|---|---|---|---|---|---|---|---|
| keys@2-2 | translate(424,424) | (0,0) | 4×3 | c0-3 r0-2 | `keys · slot 2-2 · 0°` | (0,0) 4×3 | **MATCH** |
| knobs@1-4-3 | translate(840,424) | (4,0) | 2×2 | c4-5 r0-1 | `knobs · slot 1-4-3 · 0°` | (4,0) 2×2 | **MATCH** |
| rollers@1-1-4 | translate(424,736) | (0,3) | 4×1 | c0-3 r3 | `rollers · slot 1-1-4 · 90°` | (0,3) 4×1 horiz. | **MATCH** |
| dial@1-3-4 | translate(840,632) | (4,2) | 2×2 | c4-5 r2-3 | `dial · slot 1-3-4 · 0°` | (4,2) 2×2 | **MATCH** |

- **No overlaps**: cell-level check over all four modules → `overlap: []` (the four
  footprints tile the 6×4 grid exactly: 12+4+4+4 = 24 cells).
- **Faders absent**: no `g.module.m-faders` and no "not on deck:" strip — correct, because
  the daemon does not report a faders device at all (unplugged), so there is nothing to
  place. Expected "absent or in the not-on-deck strip" → **absent = MATCH**.
- The served matrix `clients/preview-web/placement-matrix.json` is byte-identical to
  `docs/discovery/salon-001/placement-matrix.json` and contains the camera-verified
  entries `rollers@1-1-4` (0,3) rotations [90,270] and `dial@1-3-4` (5,3) rot 0, which is
  what makes the render place rollers (0,3) r90 and dial (4,2) r0 (2×2 square in-place
  bias picks r0, matching the photo's knob triangle 1-top).
- Screenshots: `verify-live-ds125-webapp-full.png` (full page: deck + controls +
  inspector with /controls/knobs/0 selected, rev 554509), `verify-live-ds125-render-deck.png`
  (`#deck` element capture).

## 4. Task 2 — camera vs render (fresh frame, PASS)

Capture: `ffmpeg -f v4l2 -input_format mjpeg -i /dev/video1 -frames:v 1
verify-live-ds125-cam-raw.jpg` → 1280×720 JPEG, 2026-11-28 15:51:41 CET
(`verify-live-ds125-cam-raw.jpg`). The raw frame is the facing view: keys module
top-left landscape with the COOLER MASTER logo at the bottom, knobs top-right with the
triangle 1-top, dial below the knobs bottom-right, rollers strip bottom-left; the loose
faders module lies at the right edge off the deck.

Webcam availability note: the AUKEY had been unplugged from usb 1-4 at 14:00:28 CET
(kernel log `usb 1-4: USB disconnect, device number 8`); the owner reconnected it before
this capture (`/dev/video1` + `/dev/video2` present again at 15:51; kernel
`uvcvideo … Found UVC 1.00 device AUKEY PC—W1 (0c45:2690)`), so the 15:51 frame is a
fresh live frame, not a reused capture.

Annotated overlay (6×4 grid calibrated on the module-union bbox, same uniform-overlay
approach as `verify-ds124-cam-annotated.jpg`; mild perspective): `verify-live-ds125-cam-annotated.jpg`.
Side-by-side (annotated photo left, live `#deck` render right):
`verify-live-ds125-sidebyside.jpg`.

Verdict per module — render vs live state vs camera:

| module@slot | live state (daemon) | webapp render | camera (facing view) | verdict |
|---|---|---|---|---|
| keys@2-2 | connected, slot 2-2 | (0,0) 4×3 r0, cells c0-3 r0-2 | top-left, c0-3 r0-2, landscape, logo bottom | **MATCH** |
| knobs@1-4-3 | connected, slot 1-4-3 | (4,0) 2×2 r0, cells c4-5 r0-1 | top-right, c4-5 r0-1, triangle 1-top (= rot0) | **MATCH** |
| rollers@1-1-4 | connected, slot 1-1-4 | (0,3) 4×1 r90, cells c0-3 r3 | bottom-left, c0-3 r3, horizontal strip | **MATCH** |
| dial@1-3-4 | connected, slot 1-3-4 | (4,2) 2×2 r0, cells c4-5 r2-3 | bottom-right, c4-5 r2-3, below the knobs | **MATCH** |
| faders | not on bus (unplugged) | not rendered (no device reported) | loose at right edge, off the deck | **MATCH (off-deck)** |

**Zero mismatches.** The two DS-124 matrix defects (rollers@1-1-4 connector cell,
dial@1-3-4 missing entry) are corrected in the current matrix and now render exactly
where the camera sees the modules.

## 5. Task 3 — live events over `/preview/ws` without touching hardware (PASS)

Setup in the page before the events: `window.__nore = 1`, `window.__t0 =
performance.now()`, 100 ms recorder of distinct `#rev` texts; control
`/controls/knobs/0` selected (clicked) so the inspector shows its JSON.

| step | daemon-side evidence | webapp evidence (no reload) |
|---|---|---|
| before | `ds get /controls/knobs/0` → rev **556279**, value `{led:{brightness:20,color:"#010101",mode:"static"}}` | `#rev` "rev 556201 · ws"; inspector `/controls/knobs/0` = `{20, #010101, static}`; `__nore=1`; 1735 distinct revs recorded |
| set knob 0 LED | `ds set /controls/knobs/0/led '{"mode":"static","color":"#00ff64","brightness":80}'` → `ok rev=556280`; `ds get` → rev **556280**, value `#00ff64`/80/static; `GET /preview/api/state` immediately shows `knobs.0.led = {brightness:80,color:"#00ff64",mode:"static"}` | after ~1 s: `#rev` "rev 556442 · ws" (passed 556280 live), inspector now `{"brightness": 80, "color": "#00ff64", "mode": "static"}`; `__nore` still 1 — **no reload** |
| restore | `ds set /controls/knobs/0/led '{"mode":"static","color":"#010101","brightness":20}'` → `ok rev=556593`; `ds get` → rev **556593**, value back to `#010101`/20/static | `#rev` "rev 556657 · ws"; inspector back to `{20, #010101, static}`; `__nore` still 1; 2189 distinct revs recorded over a continuous page age of 291 836 ms |

- The page never reloaded: `window.__nore` stayed `1` and the page age grew
  continuously (24 s → 292 s) across all events; all updates arrived purely over
  `/preview/ws` (`#rev` text updates on every event, inspector `<pre>` re-rendered from
  WS state).
- Screenshot proof: `verify-live-ds125-webapp-live-event.png` (full page at rev 556516
  with the inspector showing `"color": "#00ff64", "brightness": 80`),
  `verify-live-ds125-viewport-live-event.png` (viewport during the event).
- The deck's knob-0 LED was driven to green and then restored to its original
  `#010101`/20/static — final daemon state equals the pre-session state
  (`ds get /controls/knobs/0` → `{brightness:20,color:"#010101",mode:"static"}`).
- No hardware was touched: no unplug/reseat/tilt; the only write was the control value
  above via the daemon CLI (restored).

## 6. Verdict

- **Task 1 (render vs live state): PASS** — WS connected, rev shown and incrementing,
  all four on-deck modules at the expected cells with no overlaps, faders absent
  (unplugged), orientation flat 6×4.
- **Task 2 (render vs camera): PASS** — fresh AUKEY frame (15:51:41 CET) shows the same
  four modules at the same cells/orientations; side-by-side saved; per-module verdicts
  all MATCH; zero unexplained mismatches.
- **Task 3 (live events): PASS** — CLI write → `/preview/ws` event → `#rev` increment
  and inspector value change in the open page without reload (rev before/after
  556279→556280 set, 556593 restore on the daemon; page 556201→556442→556657,
  `__nore=1` throughout).

Remaining observations (reported, not fixed — docs-only scope):

- In `clients/preview-web/app.js` the knob/roller `fill` uses `kc.led || kc.color`,
  but the state shape is `knobs.N.led = {mode,color,brightness}` (an object), so knob 0's
  SVG `fill` attribute serializes to `[object Object]` (knob 0 renders dark while knobs
  1-2 fall back to `#4a5468`). Pre-existing render quirk; does not affect placement.
- The keys-module frame stream loads over WS (the salon-deck panel image is visible in
  `verify-live-ds125-webapp-live-event.png`), so screen previews are live too.
- The webcam had to be reconnected by the owner mid-session (unplugged 14:00:28 CET);
  noted for reproducibility, not a deck issue.

## 7. Evidence files (all under `docs/discovery/salon-001/`)

- `verify-live-ds125-webapp-full.png` — full-page webapp screenshot (deck + controls +
  inspector, rev 554509, connected).
- `verify-live-ds125-render-deck.png` — `#deck` SVG element capture (the live render).
- `verify-live-ds125-webapp-live-event.png`, `verify-live-ds125-viewport-live-event.png` —
  page during/after the CLI knob-LED write (inspector shows `#00ff64`/80, rev 556516).
- `verify-live-ds125-cam-raw.jpg` — fresh `/dev/video1` frame, 2026-11-28 15:51:41 CET.
- `verify-live-ds125-cam-annotated.jpg` — raw frame with 6×4 grid overlay + per-module
  deduced cells + MATCH legend.
- `verify-live-ds125-sidebyside.jpg` — annotated camera frame (left) vs live webapp
  render (right).

## 8. Commands run (selection)

```
ps -o pid,lstart,cmd -p 3241858                      # ds-daemon … hid: --preview, port 43541
curl -s -H "Authorization: Bearer $(cat /run/user/1000/deckstack/token)" \
  http://127.0.0.1:43541/preview/api/state           # devices + rev (553217 … 574646)
ds --data-dir /run/user/1000/deckstack get /devices --json
ds --data-dir /run/user/1000/deckstack get /controls/knobs/0 --json      # rev 556279, #010101/20
ds --data-dir /run/user/1000/deckstack set /controls/knobs/0/led \
  '{"mode":"static","color":"#00ff64","brightness":80}'                 # ok rev=556280
ds --data-dir /run/user/1000/deckstack set /controls/knobs/0/led \
  '{"mode":"static","color":"#010101","brightness":20}'                 # ok rev=556593 (restore)
for d in /sys/bus/usb/devices/*; do … idVendor==2516 → devpath → slot …  # sysfs slot cross-check
ffmpeg -f v4l2 -input_format mjpeg -i /dev/video1 -frames:v 1 verify-live-ds125-cam-raw.jpg
# browser (Playwright MCP): navigate to
#   http://127.0.0.1:8080/?token=a3d87b28…e7769&port=43541
# evaluate: __nore=1, rev recorder, click g.ctl[data-path="/controls/knobs/0"],
#   extract g.module transforms/labels, overlap + expected-cell assertions (ALL_MATCH=true)
# take_screenshot: full page + #deck element, before and after the CLI write
```
