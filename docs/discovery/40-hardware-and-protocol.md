# Discovery 40 — hardware and protocol facts for a new implementation

Prefixes: `upstream/` = `the Tauri predecessor checkout`, `hubdeck/` =
`the Python predecessor checkout`. Primary sources: `upstream/PROTOCOL.md` (verified on hardware,
"Verified here" column) and `hubdeck/docs/protocol/README.md` + per-module files + consolidated
`hubdeck/docs/masterhub-protocol.md` (tags `[hw]`/`[up]`/`[dll]`/`[hyp]`).

## 1. Modules and IDs

Each module is its own USB HID device, **VID `0x2516`, usage page `0x0C`**; the base contains a
Prolific USB hub `067B:2586` (upstream/PROTOCOL.md:7; hubdeck/docs/protocol/README.md:18).

| Module | Model | PID | family | output report | input tag (ch 0) |
|---|---|---|---|---|---|
| Base 4×6 | M0064 | `0x01F6` | M0 | 64 B | `0x2A` (orientation on `0x2B`) |
| 15 keys IPS | M4315 | `0x01FE` | screen | 320 B | `0x34` |
| 5 faders | M005S | `0x0200` | M0 | 64 B | `0x2A` |
| 3 knobs | M003K | `0x0202` | M0 | 64 B | `0x2A` |
| 2 rollers | M032LE | `0x0204` | M0 | 64 B | `0x2A` |
| Dial IPS | M131E | `0x0206` | screen | 320 B | `0x44` |

(upstream/PROTOCOL.md:9-16; hubdeck/docs/protocol/README.md:24-31; mh-protocol
`crates/mh-protocol/src/module.rs:20-40`). Screen sizes: keys panel 480×272, key 72×72
(feature `0x3B`, PROTOCOL.md:44); dial 240×240 (feature `0x4B`, PROTOCOL.md:64). Firmware/serial
per unit in PROTOCOL.md:9-16.

## 2. Report framing

- Input report `0x01`, 63 B + ID: `01 <channel tag> <len u16 LE> <payload>`, payload =
  `<event id> <len> <data…>` (upstream/PROTOCOL.md:21; hubdeck/docs/protocol/README.md:31-35).
  Channel = tag − tag_base; only channel 0 carries control events.
- Output report `0x02`: 319 B + ID on screen modules, 63 B + ID on M0 (PROTOCOL.md:22,
  README.md:31-35). Output length is read from the HID descriptor `_output_report_len`
  (PROTOCOL.md:67; `crates/mh-protocol/src/descriptor.rs:6-24`); data chunk = length − 8.
- Feature report `0x03` (set), 31 B + ID; module-specific feature IDs `0x2D..0x36` (M0) /
  `0x37..0x3E` (keys) / `0x47..0x4E` (dial) are the "get" reads, returning `<id> <len> <data…>`
  (PROTOCOL.md:23).

## 3. Commands a new implementation needs

Screen modules (software mode first: keys feature `03 3A 01`, dial `03 49 01` —
PROTOCOL.md:42, 60):

- backlight: keys `03 34 <0-100>`, dial `03 44 <0-100>`; readback `0x3C`/`0x4C`
  (PROTOCOL.md:45, 61). **Never send 0** — on macOS the dial then stops answering until reseat;
  screens-off = black image (PROTOCOL.md:61).
- solid colour: screen `03 36 R G B`, single key `03 37 idx R G B` (keys), dial `03 46 R G B`
  (PROTOCOL.md:47, 62; camera-verified 14/18 ms, PROTOCOL.md:240).
- image: output `02 36` (per-key, idx 0-14), `02 35` (whole panel 480×272, idx 0), `02 45` (dial),
  each a JPEG chunked as `02 <cmd> <chunk u16 LE> <len u16 LE> <index> <last> <≤312 B data>`
  (PROTOCOL.md:46, 50-52, 63; `report.rs:132-171`).
- input events: keys `01 34 … 02 0F k1..k15` (bitmap, 1 = held, PROTOCOL.md:48); dial `01 44 …
  03 01 00|01` rotation (**00 = clockwise, opposite of knobs**) and `04 01 01|00` press
  (PROTOCOL.md:65-66).

M0 family (always readable; LED writes `02 2C <mode> / 02 2D <R G B> / 02 2E <0-100>`, format
`02 <cmd> <target> <idx> … <save>`; target 0 = base (idx 0 only), 1 = faders (0-4 or FF),
2 = knobs (0-2 or FF); **always `save = 0`** so nothing is written to flash
(PROTOCOL.md:34-38; hubdeck/docs/protocol/m0-family.md:11-21)):

- LED modes 0-9: off, static, breathing, strobe, spring, cycling, random, wave, water, rainbow
  (PROTOCOL.md:37). Speed `02 30 <p>`. Single LEDs inside a knob ring or along the base edge are
  **not addressable** (PROTOCOL.md:38).
- input tag `0x2A`: knobs rotation `00 03 d0 d1 d2` (01 cw / 00 ccw / ff none) and press
  `03 03 p0 p1 p2`; rollers `01 02 r1 r2` (00 up / 01 down / ff none, no press); faders
  `02 05 v1..v5` absolute 0-100, all five, up to ~120 reports/s while moving
  (PROTOCOL.md:39-41; hubdeck docs/masterhub-protocol.md:226-243).
- base orientation: input tag `0x2B` payload `01`/`00`, read feature `0x35` → `35 00 vv`; it is a
  one-direction tilt switch (flat = `01`, upright = `00`), not an accelerometer
  (PROTOCOL.md:33, 106).
- **Rollers send no input until the host sends HID SET_IDLE** (`21 0A 0000 0000 0000`, duration 0);
  Windows' driver does this, macOS does not — send it at connect on macOS (PROTOCOL.md:40;
  `crates/mh-device/src/recover.rs:15-18`).

## 4. Image format and chunking

- **Baseline JPEG only, standard Huffman tables, 4:4:4 chroma** — optimized Huffman tables make
  the decoder show the first rows then black (PROTOCOL.md:87-88). jpeg-encoder default sampling and
  the `image` crate both work (PROTOCOL.md:95-96). BMP accepted by the vendor DLL but unverified on
  hardware (`[dll]`, hubdeck/docs/protocol/m131e.md:81-84).
- Chunk = output_len − 8 = **312 B** on screen modules (PROTOCOL.md:67; `report.rs:135`); header 8 B;
  `last = 1` on the final chunk. Example: 72×72 q90 key ≈ 2.8 KB = 10 reports
  (hubdeck/docs/masterhub-protocol.md:113-114).
- Per-key image of key `i` lands at panel x = **5, 104, 203, 303, 402**, y = **1, 101, 199**
  (72×72 squares), camera-measured to the pixel (PROTOCOL.md:81; `crates/mh-device/examples/align.rs:20-22`).
  Coarser grid (i%5)*102 / (i/5)*100 was one pixel off in places and caused visible jumps
  (PROTOCOL.md:52-54, 70-87).

## 5. Timing / throughput (measured)

- Sending: ~32 fps of full panel (480×272, q80 ~7 KB) and dial (240×240) images **accepted**, both
  modules together each ~32 fps — device-side limit, not size (PROTOCOL.md:91-96,
  `examples/framerate.rs`).
- **Displayed: at most ~24 different key pictures/s** even at 30.4-37.9 sent/s (filmed at 30 fps,
  PROTOCOL.md:243-246; engine caps canvas at 24 fps, `engine/mod.rs:144`).
- USB throughput ≈ **2.1 MB/s** to one keys panel (PROTOCOL.md:139, 263-265): 99 KB panel 46.5 ms
  (21.5/s); macOS hidapi one-at-a-time 121-133 ms for q98 panels, with 8 writes in flight
  26.4 ms (37.9/s) on a second device object (PROTOCOL.md:248-258; `overlapped.rs` DEPTH=8 Windows,
  `pictures.rs` macOS). Lesson: pipeline writes (8 in flight) — one-at-a-time is 3-5× slower.
- LED writes: hidapi sustains ~250/s (`docs/masterhub-protocol.md:221-222`); a 64-byte colour takes
  **2.7 ms on its own handle vs 34.8 ms on the shared handle** (reader holds 20 ms slices);
  `send_output_report` (SET_REPORT control pipe) is refused by the base and hangs 5 s
  (PROTOCOL.md:116-118; `tests/output_timing.rs`). Suspected fader firmware limit ~25 LED reports/s —
  stay under 30 (PROTOCOL.md:118-121, "likely, not proven").
- M0 feature reads answer in 45-48 ms (keys 31 ms) without wedging (PROTOCOL.md:141-143).
- Faders report ~every 20 ms while moving; base heartbeat every ~0.5 s
  (PROTOCOL.md:39; hubdeck docs/masterhub-protocol.md:235-236).

## 6. Readback / ack behaviour

- **No per-write acknowledgement exists.** Writes return a byte count only. Readback is GET_FEATURE
  only: firmware `0x2E`/`0x38`/`0x48`, serial `0x30`/`0x3A`/`0x4A`, device info `0x3B`/`0x4B`,
  backlight `0x3C`/`0x4C`, fader values `0x33`, orientation `0x35`, LED mode/brightness `0x34`/`0x32`
  (PROTOCOL.md:28-33, 141-167; `report.rs:189-233`).
- Get-features return the **saved** LED mode/brightness, not the live state — track live state
  yourself (PROTOCOL.md:35, 152; hubdeck docs/masterhub-protocol.md:219-220). Backlight reads do
  return the value just set (PROTOCOL.md:45, 61).
- **Reading any feature report from keys or dial in hardware mode locks the module until power
  reset** (USB strings go garbage; writes silently no-op). Only reseat or `pnputil /restart-device`
  (admin) unlocks (PROTOCOL.md:182-197). mh-device therefore refuses reads outside software mode
  (`Error::HardwareMode`, `crates/mh-device/src/module.rs:158-166`) and identifies modules by USB
  path, not USB string (PROTOCOL.md:208-211). After software-mode-on, wait ≥600 ms (and send the
  command twice) before the first read on macOS (PROTOCOL.md:197).

## 7. Hot-plug / reconnect / handback

- Removing a module ⇒ HID read error; hub closes the handle, rescans every 1 s, re-runs
  initialize + redraw on return; keys measured back **5 s** after unplug (PROTOCOL.md:172-177;
  `hub.rs:20-21, 128-165`). Loose contacts can drop USB ~0.5 s on a press (PROTOCOL.md:176-177).
- The base only detects newly attached modules at power-up — replug the base's USB after adding a
  module (hubdeck/CLAUDE.md:59).
- Handback: on exit/sleep/settings the official app sends software-mode **off**
  (`03 3A 00` / `03 49 00`); mh-device `Module::release()` does the same, and the guard process does
  it after a crash (`PROTOCOL.md:213-216`; `module.rs:227-240`; `src/guard.rs:116-134`).
- Vendor MasterHUB + its `MHDaemon` drive the same modules — they must be closed or the two fight
  (hubdeck/CLAUDE.md:66; PROTOCOL.md:217-221). macOS hidapi opens are exclusive — one program at a
  time (hubdeck docs/masterhub-protocol.md:36-37).

## 8. Commands that must remain unrepresentable

Blocked in both drivers; a new protocol layer must not be able to encode them
(BRIEF.md:122 — "unrepresentable, not merely unused"):

- M0 output: `0x2A` firmware update, `0x2B` set serial, `0x2F` tilt recalibrate,
  `0x31` save factory data, `0x49` firmware recovery, `0x4A` factory reset
  (upstream `crates/mh-protocol/src/safety.rs:8-28`; hubdeck `device/hid.py:52-53`).
- Keys: feature `0x35` set serial, `0x39` fw recovery, `0x3C` save factory data, `0x3D` factory
  reset, `0x3E` animation recovery; output `0x34` fw update, `0x38` save picture to flash
  (`safety.rs`; `hid.py:57-58`). Note: feature `0x34` on keys is **backlight and safe** — the same
  byte means fw-update only as an *output* command (hubdeck/docs/protocol/README.md:56-57).
- Dial: feature `0x45` serial, `0x48` fw recovery, `0x4B` save factory, `0x4C` factory reset,
  `0x4D` animation recovery; output `0x44` fw update, `0x47` save picture
  (`safety.rs`; `hid.py:61-62`).

Mechanism that works: builders exist only for safe commands **and** the raw-report path checks
`is_forbidden([id, cmd, ..])` (`report.rs:28-30` → `module.rs:104-106` → `Err(Error::Forbidden)`),
with tests asserting no builder output is forbidden (safety.rs:40-79). Python equivalent:
denylist + allowlist in `check_write` (`device/hid.py:85-104`). Firmware updates stay out of scope
(official app only, PROTOCOL.md:168-170).

## 9. Other facts the device layer must know

- Two modules of the same kind: each is its own HID device with its own serial; keys/dial mirror,
  a second knobs/faders/rollers gets its own controls by serial (upstream `deck.rs assign_groups`,
  PROTOCOL.md:130-139).
- Module placement/rotation = USB port path relative to the base's top hub (`1-4-3` style;
  Windows `DEVPKEY_Device_LocationPaths`, macOS `locationID`, Linux none yet —
  PROTOCOL.md:107; `crates/mh-device/src/location.rs`, Linux TODO at `:144-149`). Any module fits any
  of the 26 connectors (PROTOCOL.md:109-127). Base grid 4×6 or 6×4 by orientation.
- The keys module can drop off USB by itself; reopening the handle recovers it
  (hubdeck docs/masterhub-protocol.md:191-192).
- Keys module returns corrupted USB string descriptors; use enumeration names / report serial
  (PROTOCOL.md:272-273).
- No audio/vibration/mic on the device — HID + Prolific hubs only (PROTOCOL.md:229-237).

## What this means for the new base

The protocol layer should be a small crate/module with builders only for §3 and a forbidden-byte
check on any raw path (§8) — exactly mh-protocol's shape. The device layer must: software-mode
handshake with ≥600 ms settle before reads, never brightness 0, SET_IDLE on macOS, pipelined
writes (8 in flight) on a second handle where the OS allows, save=0 on every LED write, rescan
≤1-2 s, full repaint on reconnect, handback on exit/crash. Budget the frame path at ~24 fps
displayed / ~32 fps accepted / ~2.1 MB/s, with latest-frame-wins above that. There is **no device
readback of what is displayed** — see 50-risks-and-open-questions.md for the proxy.
