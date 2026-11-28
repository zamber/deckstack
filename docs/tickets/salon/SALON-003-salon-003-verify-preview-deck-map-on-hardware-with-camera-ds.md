# SALON-003 — SALON-003 verify preview deck map on hardware with camera (DS-124/DS-125)

Status: verified — DS-125 live check PASS (2026-11-28): the preview webapp render on the
real `hid:` `--preview` daemon (port 43541) matches the live `/preview/api/state` and the
fresh webcam frame cell-for-cell; live WS updates proven without reload. Evidence:
`docs/discovery/salon-001/verification-live-ds125.md`.
Date: 2026-11-28
Session: salon-deck live session 2026-11-28 15:07–15:55 CET (DS-124 vdev+camera session
13:54 CET the same day; DS-125 live `--preview` session 15:07–15:55 CET)
Owner: catwars (deck owner away during DS-125; webcam reconnected by owner mid-session)
Deck state: captured at session start (`ds get /devices --json` on
/run/user/1000/deckstack, daemon pid 3241858 `ds-daemon run --device hid: … --preview`,
port 43541): base MHBS01AA461251603330 slot 1-4-4 horizontal; keys MHKD15AA431251602831
slot 2-2; knobs MHKB03AA221252301767 slot 1-4-3; rollers MHRR02AA411251602062 slot 1-1-4;
dial MHDD01AA221252301435 slot 1-3-4; faders unplugged (no 2516:0200 on the USB bus).
Same seating cross-checked in sysfs (devpaths 3-2.2.2, 3-2.1.4.3, 3-2.1.1.4, 3-2.1.3.4,
3-2.1.4.4).
Hardware involved: MasterHub base + keys/knobs/rollers/dial modules (2516:01f6/01fe/
0202/0204/0206), faders module unplugged lying at the right edge, AUKEY PC—W1 webcam
(/dev/video1, 0c45:2690), preview webapp clients/preview-web on http://127.0.0.1:8080/.
Links: DS-122,DS-123,DS-124,DS-125,SALON-001

## Goal

Verify that the preview webapp's deduced deck map on the LIVE real-deck daemon (hid: with
--preview, port 43541) matches the physical deck as seen by the webcam, and that the
webapp updates live over /preview/ws (DS-125, camera-oracle per ADR-015).

## Steps

1. Read live state: `GET /preview/api/state` (Bearer token from /run/user/1000/deckstack/token)
   + `ds get /devices --json` + sysfs devpath cross-check (§2 of verification-live-ds125.md).
2. Open the webapp at `http://127.0.0.1:8080/?token=…&port=43541` (Playwright); confirm
   `#conn`=connected, `#rev` incrementing; extract DOM placements (`g.module` transform,
   OX=OY=424, U=104) and assert against the expected cells; screenshot full page.
3. Capture a fresh webcam frame from /dev/video1 (facing view), annotate the 6x4 grid,
   build the render-vs-photo side-by-side, verdict per module.
4. Live-event check without touching hardware: `ds set /controls/knobs/0/led …` (green),
   confirm the open page updates over WS (rev increments, inspector changes, no reload via
   `window.__nore`), then restore the original value.

## Verification evidence

- Render vs live state: all four on-deck modules at expected cells, no overlaps, faders
  absent (unplugged): keys@2-2 (0,0) 4x3 r0; knobs@1-4-3 (4,0) 2x2 r0; rollers@1-1-4
  (0,3) 4x1 r90; dial@1-3-4 (4,2) 2x2 r0 — in-page assertion ALL_MATCH=true.
  Screenshots `verify-live-ds125-webapp-full.png`, `verify-live-ds125-render-deck.png`.
- Camera (fresh /dev/video1 frame 2026-11-28 15:51:41 CET, facing view): same modules at
  the same cells — keys TL c0-3 r0-2, knobs TR c4-5 r0-1 (triangle 1-top), dial BR c4-5
  r2-3, rollers BL c0-3 r3, faders off-deck. Per-module verdicts all MATCH.
  `verify-live-ds125-cam-raw.jpg`, `verify-live-ds125-cam-annotated.jpg`,
  `verify-live-ds125-sidebyside.jpg`.
- Live events (hardware untouched): `ds set /controls/knobs/0/led '{"mode":"static",
  "color":"#00ff64","brightness":80}'` → daemon rev 556279→556280 (`ok rev=556280`),
  `/preview/api/state` updated immediately; open webapp page went rev 556201→556442 with
  inspector `/controls/knobs/0` showing `{"brightness":80,"color":"#00ff64","mode":"static"}`
  and `window.__nore` still 1 (no reload). Restore set → rev 556593, page rev 556657,
  inspector back to `#010101`/20/static. Screenshots
  `verify-live-ds125-webapp-live-event.png`, `verify-live-ds125-viewport-live-event.png`.
- Full write-up: `docs/discovery/salon-001/verification-live-ds125.md`.

## Outcome

- DS-125 acceptance met: photo + render side-by-side stored under
  `docs/discovery/salon-001/`; zero unexplained mismatches. The DS-124 camera-corrected
  matrix entries (rollers@1-1-4 → (0,3), dial@1-3-4 → (5,3)) now render correctly on the
  live deck: the preview webapp's deck map is verified against the real hardware.
- DS-124 (vdev permutations + JS/python solver cross-check) stays as previously recorded;
  its two matrix defects are closed by the camera-verified matrix entries.
- Open observations (reported, not fixed, docs-only scope): webapp knob/roller `fill`
  uses the `led` object (renders as `[object Object]` for knob 0); webcam had to be
  reconnected mid-session (unplugged 14:00:28 CET per kernel log).
