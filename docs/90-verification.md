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

## Hardware smoke suite — `tests/hw/smoke.py` (DS-026)

The phase-2 hardware smoke is a separate, black-box Python suite (stdlib only) that drives
the real `ds` / `ds-daemon` binaries and reads sysfs + `/proc` — it never imports driver
internals. It complements the camera oracle above: the camera proves pixels, the smoke
proves the host-side device path (enumerate → handshake → frame → replug → handback).

### How to run

```sh
DS_HARDWARE=1 python tests/hw/smoke.py    # hardware run (deck attached, see below)
python tests/hw/smoke.py                  # CI: prints SKIP, exit 0 — no hardware needed
```

Exit 0 = no FAIL (SKIPs print their exact reason and still exit 0); exit 1 = at least one
FAIL with precise evidence (pid, report bytes, error code, errno). Every wait has a
timeout, all subprocess calls are argv lists (no shell interpolation), and the daemon plus
temp dirs are always cleaned up in a `finally`.

### What it proves, step by step

| Step | Command path | Proves |
|---|---|---|
| enumerate | sysfs scan (the descriptor source of `ds-device::hid::enumerate`) + `ds daemon start --device hid:` + `ds daemon status --json` | all five attached modules (VID `0x2516`, pids `01f6` base, `01fe` keys, `0202` knobs, `0204` rollers, `0206` dial) are visible, and the daemon accepts the `hid:` URI (running, `device=hid:`) |
| handshake | poll `device_status` until `connected hid: (healthy)`; assert `N module(s) handshaken [keys fw=…, …]` in `daemon.log` | the software-mode handshake (DS-022) completed for **every** module, each confirming firmware read answered |
| frame | `ds set /controls/keys/0/image --file examples/key.png` (staged under `/tmp`, inside `/policy/fs/roots`), then `ds set /controls/keys/1/image '{"kind":"color","value":"#3cc86e"}'` | the daemon accepts every chunk: `pushed …` write-log lines for both targets, zero push errors/timeouts; state readback + `rev` delta == the two durable `set` commits (stream frames never bump `rev`, S1) |
| replug | udev `unbind`/`bind` of one module (needs root — see fallback) | rescan + re-handshake + **re-push of the latest frame** within the ≤ 5 s budget (`upstream/PROTOCOL.md:172-177`): disconnect noticed, then `connected hid: (healthy)` + `re-pushing N cached target(s)` in `daemon.log` |
| handback | SIGTERM to the daemon pid | exit 0 (`stopped cleanly (exit 0)` + guard `ds-daemon::guard` handback lines in `daemon.log`), control socket + pid file removed, `ds daemon status` afterwards exits 6, and **no lingering hidraw/usb handles** of ours (`/proc/*/fd` scan) — the deck is left in a clean state |

Liveness (≤ 100 ms) is the daemon device task's own watchdog probe
(`hotplug::DEFAULT_PROBE_TIMEOUT`); the smoke asserts its externally visible effect —
`device_status` stays `connected … (healthy)` while idle.

### Permissions and the manual replug fallback

Opening the modules needs the DS-025 udev rule (`packaging/udev/99-deckstack.rules`,
`TAG+="uaccess"` on the hidraw **and** usb nodes) installed and triggered, or root:

```sh
sudo install -Dm644 packaging/udev/99-deckstack.rules /etc/udev/rules.d/99-deckstack.rules
sudo udevadm control --reload-rules && sudo udevadm trigger
ls -l /dev/hidraw*    # uaccess nodes carry an ACL for the logged-in user
```

Without it, `ds-device` enumeration still works (libusb reads sysfs), but opening fails
with `EACCES` on `/dev/bus/usb/…` / `/dev/hidraw…` — in that state the suite still runs
enumerate + handback for real and prints `SKIP` with the exact node, uid and errno for
handshake / frame / replug (hardware absence or missing permissions are states to detect,
not failures — same rule as `tests/hw/hid.rs`).

The replug step needs to *disconnect* a module. There is no in-tree hotplug hook for the
`hid:` transport (`ds-vdev --drop-after` simulates hot-unplug for the **virtual** device
only), so the suite uses the udev unbind/bind path when `/sys/bus/usb/drivers/usb/unbind`
is writable (root), and otherwise SKIPs with a pointer here. **Manual fallback** (no root
at runtime): physically unplug and replug one module, or as root:

```sh
echo 3-2.3.4 > /sys/bus/usb/drivers/usb/unbind   # one module, e.g. the knobs
sleep 1
echo 3-2.3.4 > /sys/bus/usb/drivers/usb/bind
```

Expected while the daemon runs on `hid:`: `device_status` drops (`disconnected` /
`reconnecting`), then within ≤ 5 s after the replug it returns to
`connected hid: (healthy)` and `daemon.log` shows `re-pushing N cached target(s) after
connect` followed by the `pushed … for re-push …` lines — the latest frame is back on
the deck. The smoke automates exactly this when it has the permissions.

## Operator section — deckstack-verify (tools/verify/, DS-027)

The harness is `python3 tools/verify/deckstack_verify.py <devices|capture|calibrate|compare|check|selftest>`
(stdlib + ffmpeg/v4l2-ctl, argv-only; usage and limits in `tools/verify/README.md`).

- **Device**: enumerate with `deckstack_verify.py devices`. On the reference machine
  the camera is the **AUKEY PC-W1 at `/dev/video2`** (`/dev/video3` is its metadata
  node; `/dev/video0` is the OBS virtual camera and is never the default). Override
  with `--device` or `$DSV_CAMERA`. Capture: `capture --out shot.jpg --settle 800`
  (1280×720 MJPEG by default, warm-up frame + retries).
- **Placement**: camera fixed and roughly frontal to the keys panel, panel filling a
  good part of the frame, no direct glare on the LCDs. Calibration is only valid for
  the pose it was taken in — re-calibrate after any camera/deck movement. Limits:
  lighting changes shift measured RGB (tolerance 12 absorbs auto-exposure noise),
  strong perspective is not corrected (axis-aligned affine only), occlusion proves
  nothing.
- **Calibration**: dark room + lit panel → `calibrate --frame shot.jpg --out
  calibration.json --overlay overlay.png` auto-detects the 15 key faces; in a bright
  room pass `--panel x,y,w,h` measured from the overlay (reference 1280×720 pose:
  `--panel 505,150,285,345`, deck rotated 90° in frame). Hints: `--grid 5x3`,
  `--orientation`, `--face-luma lo,hi`. Verify `overlay.png` (green rects inside the
  key faces). The JSON stores the panel rect, per-target ROIs (`key:0..14`, `keys`,
  `led:N` with cell masks) and the assumptions.
- **Golden checks**: `compare --expected key:0=#ff0000 --actual shot.jpg
  --calibration calibration.json` (colour spec, per-target mean-RGB delta vs
  tolerance 12, exit 7 on mismatch, `--json` for NDJSON) or
  `compare --expected golden.jpg --actual shot.jpg` (per-ROI golden image).
  `check --target key:0 --expected key:0=#ff0000 …` = capture + compare in one step.
  Recorded live-deck run: `tools/verify/demo/run.log` (real deck, key:0 set to
  #ff0000: compare exits 7 with the exact deltas because this unit's M4315 LCDs do
  not render pushed images — the oracle catching a device-side non-render — while
  LED zones and golden compares pass).
- **CI excludes the hardware path**: CI runs only `deckstack_verify.py selftest`
  (synthetic frame, no deck and no camera needed; it also exercises a real capture
  when a camera is present). `devices`/`capture`/`calibrate`/`compare`/`check`
  against the live deck are hardware-gated, phase 2+ only, like `tests/hw/smoke.py`
  (`DS_HARDWARE=1`).

## Golden camera checks (tools/verify/golden/, DS-028)

Committed goldens pin the three representative visual states: **key0** (key:0 set from a
fresh daemon, LEDs off), **panel** (all 15 key windows backlit `#ffffff`, LEDs off) and
**led** (knob 0-2 + roller 0-1 LED rings static `#00ff00`, panel at its backlit default).
Each golden is a 1280x720 capture from the reference pose plus a per-target comparison
over the calibrated ROIs; the state that produced it is in `manifest.json`, and
`run_golden.py` re-applies that state to a **fresh daemon data dir** before every check,
so the durable state starts empty (empty-state invariant).

These are test fixtures only: nothing under `tools/verify/golden/` is installed as a deck
default, shipped in a state file, or loaded by the daemon.

```sh
python3 tools/verify/golden/run_golden.py                 # all goldens, exit 0 pass / 7 mismatch
python3 tools/verify/golden/run_golden.py --only key0,led # one or two
python3 tools/verify/golden/run_golden.py --keep /tmp/f    # keep the captured frames
```

Requires the deck on `hid:` (DS-028's runs used the daemon in the repo's `target/debug`)
and the AUKEY camera; `DSV_CAMERA` overrides the node and `DS_BIN_DIR` the binaries.
Each golden runs against a fresh data dir, sets the manifest state with `ds set`, waits
`settle_ms`, captures, and compares every target at the golden's `tolerance` (20: stable
same-state noise measured at delta ~9-13 per target; the median-selected golden and the
min-over-attempts rule suppress the single-frame glare spikes that reach ~90 on one key
face). The recorded run is in `prs/DS-028.md`.

**Device limitation (measured 2026-11-24)**: on this M4315 unit the key windows keep the
last pushed colour per index instead of returning to a per-index default, so a fresh
daemon data dir does not mean a fresh device: the same manifest state captured minutes
apart renders different keys differently (key0 matched 17/17 targets immediately after
recording, then failed on the untouched keys 1-14 and `led:0`; the panel golden failed
15/17, the led golden 3/17), and the LED rings ignore `mode: "off"` (they hold the last
colour across daemon restarts). A golden is therefore a *recorded fixture* until every
state also pins every key index and a working LED-off path exists; each golden carries a
`skip_reason` and `run_golden.py` reports it as `SKIP` (exit 0) unless `--strict` is
passed. The comparison mechanics themselves are proven: `--record` regenerates the
calibration and all goldens, and a wrong state fails with the per-target deltas.

**Calibration procedure** (re-run after any camera/deck/lighting change):

1. Light the panel: all 15 keys `#ffffff`, knob 0-2 + roller 0-1 LEDs static `#ff00ff`
   at brightness 100 (the `calibration.state` block in `manifest.json`), capture
   `tools/verify/golden/cal-frame.jpg`.
2. `python3 tools/verify/deckstack_verify.py calibrate --frame tools/verify/golden/cal-frame.jpg
   --out tools/verify/golden/calibration.json --overlay tools/verify/golden/overlay.png
   --panel 395,148,205,347 --led-region 150,300,220,260 --orientation 90`
   (`--led-region` keeps saturated room clutter out of the LED-zone detection; `--panel`
   is the reference pose rect measured from the overlay). Check `overlay.png`: green
   rects inside the key faces, magenta cells on the LED rings.
3. Regenerate goldens deliberately: `run_golden.py --record` re-captures
   `cal-frame.jpg`, `calibration.json`, `overlay.png` and every golden, updating
   `manifest.json` (`record_command`). Never hand-edit a golden.

CI runs none of this (hardware-gated, phase 2+); CI keeps `deckstack_verify.py selftest`,
which needs no deck and no camera. The check wiring is the harness's existing
`check --expected <golden.jpg> --calibration <calibration.json> --tolerance 20`
(`manifest.json` carries the exact equivalent command per golden).
