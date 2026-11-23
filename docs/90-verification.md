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
