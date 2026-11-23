# deckstack-verify — camera harness (DS-027)

Pixel-level verification of the deck through a webcam. The device has **no screen
readback** (writes return byte counts only), so the camera is the only oracle for
"what is actually displayed" (`docs/90-verification.md`). This tool is Python 3
**stdlib only** plus the external `ffmpeg` / `ffprobe` / `v4l2-ctl` binaries, always
invoked as argv lists (no shell, no eval). It lives in-tree at `tools/verify/`
during alpha and is subtree-splittable (single-file CLI, no imports from the repo).

```
python3 tools/verify/deckstack_verify.py <subcommand>   # see --help
```

| Subcommand | What it does | Proves |
|---|---|---|
| `devices` | `v4l2-ctl --list-devices` + `--info` per node; picks the default camera | camera discovery; default is `/dev/video2` (AUKEY PC-W1), never the OBS loopback `/dev/video0` |
| `capture --out f.jpg [--device …] [--width --height] [--frames N] [--clip S] [--settle ms]` | one still (or burst/clip) via ffmpeg, argv-only, with warm-up + retries | the camera path works; produces the frames everything else consumes |
| `calibrate --frame f.jpg --out cal.json [--overlay o.png] [--grid 5x3] [--panel x,y,w,h] [--orientation 0|90|180|270] [--face-luma lo,hi]` | detects the panel + 15-key grid + LED zones from a captured frame; writes a JSON calibration (rect per target, cell masks for LED zones) and a debug overlay PNG | the device→image mapping is known, so per-target ROIs exist |
| `compare --expected <spec> --actual f.jpg [--calibration cal.json] [--tolerance N] [--json]` | per-target mean-RGB comparison with tolerance; NDJSON with `--json` | camera-verified pixels per target; exit 7 on mismatch |
| `check --target key:0 --expected key:0=#ff0000 …` | capture + compare in one step | the ticket-style one-shot check |
| `selftest` | full pipeline on a synthetic frame (PNG roundtrip → calibrate → compare pass → mismatch detected), plus camera capture if a camera is present | the harness itself is correct without any deck (CI-safe) |

Exit codes: `0` ok · `2` usage · `6` camera unreachable · `7` verification mismatch
(`docs/90-verification.md` §5 reuses the E_DEVICE-class exit 7).

## Expected-spec grammar (`compare --expected` / `check --expected`)

- inline colours: `key:0=#ff0000,led:1=#00ff00` (comma-separated `target=#rrggbb`)
- spec JSON file: `{"targets": {"key:0": "#ff0000", "led:1": "#00ff00"}, "tolerance": 12}`
  (a bare `{"key:0": "#ff0000"}` map also works)
- golden image: any `.png`/`.jpg` path — every calibrated target's ROI in the golden
  is compared to the actual (capture golden and actual at the same `--width/--height`)

Targets: `key:0`…`key:14` (72×72 key windows at device x=5,104,203,303,402 /
y=1,101,199 on the 480×272 `keys` panel), `keys` (whole panel), `dial`, `led:N`
(calibrated saturated LED zones), `frame` (whole image).

Compare metric: per-target mean RGB over the ROI (LED zones: mean over the blob's
strongly-lit cells), delta = mean per-channel |actual−expected|, pass when
`delta ≤ tolerance` (default **12**, matching `--metric color --tolerance 12` in
`docs/90-verification.md` §5). `--json` prints one NDJSON line per target plus a
summary line.

## Calibration procedure (operator)

1. Mount the camera **frontally** so the keys panel fills a good part of the frame;
   avoid direct light on the LCDs (glare) and keep the camera fixed afterwards —
   calibration is only valid for the pose it was taken in.
2. Preferred (dark room): turn the room lights down, set all keys to bright
   colours (`ds set /controls/keys/N/image --json '{"kind":"color","value":"#ffffff"}'`),
   then `capture` + `calibrate --frame f.jpg --out calibration.json --overlay overlay.png`.
   On a dark background the lit key faces are detected automatically (15 blobs →
   grid; orientation auto from the panel aspect).
3. Bright room (auto-detect may pick the wrong blobs): measure the panel once from
   a debug frame and pass it explicitly:
   `calibrate --frame f.jpg --out calibration.json --overlay overlay.png --panel x,y,w,h`
   (reference setup, 1280×720: `--panel 505,150,285,345`, deck rotated 90° in the
   image → `orientation 90` auto). Inspect `overlay.png`: green rects must sit
   inside the key faces, the red rect around the panel.
4. Hints: `--grid 5x3` (device grid), `--orientation 0|90|180|270` (device rotation
   in the image), `--face-luma lo,hi` (key-face brightness band override).
5. Reuse: `compare --calibration calibration.json …`. Re-calibrate whenever the
   camera or deck is moved.

Assumptions (also stored in the calibration JSON): roughly frontal camera
(axis-aligned affine device→image mapping, no perspective correction — a strong
perspective needs a homography, out of scope for alpha), key faces brighter or more
saturated than the bezel gaps, key windows are the fixed 72×72 device grid.

## What each check proves — and the limits

- `selftest` proves the pipeline (decode, calibration, compare, exit codes) with
  zero hardware: synthetic frame → 16/16 targets match exactly, and a wrong spec is
  detected as a mismatch (exit 7). It also exercises a real camera capture when
  one is present. **CI runs only `selftest` — hardware checks never run in CI.**
- `compare` with a colour spec proves "this target region shows roughly this colour"
  within tolerance (calibrated lighting). It cannot prove sub-frame timing, exact
  fps, or device-side dropped frames (camera fps ≪ device 24 fps cap), and nothing
  when the view is occluded.
- `compare` with a golden image proves "the panel looks like this reference frame"
  per ROI (noise floor measured on the reference setup: per-target delta 2–11 at
  1280×720, tolerance 12).
- Lighting, focus and camera placement directly change the measured RGB: fixed
  exposure (`--settle`), fixed camera, same room lighting between golden and
  actual. The AUKEY auto-exposure drifts a few units between frames — that is why
  the default tolerance is 12, not 0.
- **Known hardware finding (2026-11-23, this deck):** `ds set /controls/keys/N/image`
  (colour or `--file`) is accepted by the device and logged as `pushed`, and state
  readback is correct, but the M4315 key LCDs do **not** render the pushed image —
  the keys stay uniformly backlit. Knob LED-zone writes (`/controls/knobs/N/led`)
  **are** camera-visible. So on this unit `compare --expected key:0=#ff0000`
  correctly reports a mismatch (exit 7) with the exact deltas — the camera oracle
  detecting a device-side non-render — while LED zones and golden compares pass.
  See `demo/run.log` for the recorded run.

## Live demo

`tools/verify/demo/run_live_demo.sh` drives the real deck end to end (daemon on
`hid:`, set key:0 + knob LEDs, capture, calibrate, compare, check, clean SIGTERM):

```sh
DS_BIN_DIR=<dir with ds/ds-daemon> sh tools/verify/demo/run_live_demo.sh
```

Recorded output: `tools/verify/demo/run.log`; artifacts: `frame.jpg`,
`check-frame.jpg`, `calibration.json`, `overlay.png`.
