# 90 — Camera verification (hardware truth oracle)

Owner steering 2026-11-22 (delta 6): a webcam is pointed at the live deck. The device has
**no screen readback** (no per-write ack; feature reads return saved config; input reports
carry control events only — `upstream/PROTOCOL.md:19-23,35,141-167,219-220`), so the
host-side proxy (sent log + reassembled sha256 + liveness) cannot prove pixels. The camera
is the pixel-level oracle. Tool: `deckstack-verify` (separate publish target, `repos.toml`;
in-tree at `tools/verify/` during alpha).

## What the camera can and cannot prove

| Can prove | Cannot prove |
|---|---|
| A given key/panel region shows the expected image (golden-frame diff, SSIM/perceptual) | What the device *would* show without the host (no readback exists) |
| Colour of a region within tolerance (calibrated) | Sub-frame timing / exact fps displayed (camera fps << 24 fps device cap) |
| LED zone colour/brightness roughly (calibrated, low fps ok) | Device-side dropped frames between accepted writes |
| Key window table (x=5,104,203,303,402 / y=1,101,199, `upstream/PROTOCOL.md:81`) | Anything when the webcam view is occluded |

Complements the no-readback proxy (`docs/30-testing.md` §6): vdev/CI assert protocol-level
traffic; the camera asserts end-to-end pixels on hardware. Both are required; neither
substitutes for the other.

## Pipeline

1. **Discovery**: enumerate cameras (`v4l2-ctl --list-devices` / `ffmpeg -f v4l2 -list_formats all`), pick by stable device path, record in `verify.toml`.
   On this machine (2026-11-22): `/dev/video0` is the OBS virtual camera, not the deck; the
   physical camera is `/dev/video2` (`AUKEY PC-W1`, with `/dev/video3` as its metadata node).
   `ffmpeg` is installed. Record the AUKEY path in `verify.toml`; do not default to `/dev/video0`.
2. **Calibration**: capture a reference frame with a known full-panel image (e.g. 4-quadrant
   red/green/blue/white at known key indices); compute per-region ROIs for the 15 key
   windows + panel + dial; store ROI map + white-point/gamma correction in `verify.toml`.
3. **Capture**: `deckstack-verify capture --target key:3 --out shot.png` (ffmpeg/v4l2, fixed
   exposure/gain to avoid flicker artifacts).
4. **Reference frames**: `tests/golden/*.png` produced once on hardware with the known
   image; checks compare capture vs reference per ROI.
5. **Checks**: `deckstack-verify check --target key:3 --reference tests/golden/key3.png
   --metric ssim --threshold 0.95` or `--metric color --tolerance 12` (per-channel mean
   delta after calibration) or perceptual (pHash distance ≤ 10). Exit 0 pass, 7 mismatch
   (reuse `E_DEVICE`-class exit 7 for "verification failed").
6. **Complements the proxy**: the sent-log says what was written; the camera says what is
   visible. A mismatch means either a device-side drop or a model error — investigate with
   `ds-vdevctl writes` + capture pairs.

## Where it runs

- **CI**: never (vdev only). **Hardware runs**: phase 2+ smoke suite calls
  `deckstack-verify check` for at least **one key (key:0) and the panel (`keys`)** —
  this is a phase-2 exit criterion (`docs/60-roadmap.md`). Nightly hardware job runs the
  full 15-key + dial sweep.
- Precedent: upstream filmed the device at 30 fps for exactly these facts
  (`upstream/PROTOCOL.md:70-87, 243-246`, `crates/mh-device/examples/align.rs`).

## Tool split

`deckstack-verify` = `capture` / `calibrate` / `check` / `sweep` subcommands, config
`verify.toml` (camera path, ROI map, thresholds), output NDJSON for CI ingestion. Depends
only on `schemas/stream-frame.json` + golden frames, not on the daemon — runnable against
any capture directory.
