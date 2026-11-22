# Contract: streaming / frame plane

R5/R11. Design: ADR-005, D06. Binary framing: `docs/contracts/ipc.md` §2.

## 1. Surfaces (targets)

| Target | Module | Size | HID cmd | Quality |
|---|---|---|---|---|
| `keys` (panel) | M4315 | 480×272 | output `02 35`, idx 0 | JPEG q80 baseline |
| `key:<0-14>` | M4315 | 72×72 | output `02 36`, idx N (lands at x=5,104,203,303,402 / y=1,101,199 — `upstream/PROTOCOL.md:81`) | q90 |
| `dial` | M131E | 240×240 | output `02 45` | q85 |

Per-key uploads remain supported as the fallback when panel-as-one-image is not wanted
(discovery/30 panel row).

## 2. Frame formats

- Wire (producer → daemon): PNG or JPEG or GIF, ≤ 16 MB wire bytes, decode caps
  ≤ 4096×4096 px and ≤ 300 frames, min frame duration 0.02 s (precedents:
  `hubdeck/.../render/images.py:21-23`, `control.py:36`, `upstream/.../plugins.rs:266`).
- On the device: **baseline JPEG only, standard Huffman tables, 4:4:4 chroma**
  (`upstream/PROTOCOL.md:87-96`) — the daemon re-encodes anything else; optimized-Huffman
  JPEGs are rejected with `E_FORMAT` (they decode as black rows on the module).
- Chunking: `02 <cmd> <chunk u16 LE> <len u16 LE> <index> <last> <≤312 B data>` in 320 B
  output reports (`upstream/crates/mh-protocol/src/report.rs:132-171`). A 72×72 q90 key
  ≈ 2.8 KB ≈ 10 reports (`hubdeck/docs/masterhub-protocol.md:113-114`).

## 3. Queue policy — latest-frame-wins

Per target: a writer thread owns the HID handle. Queue = 1 frame **in flight** + 1
**waiting**; a new frame replaces the waiting one (never the in-flight one) and
increments `stats.dropped` (pattern: `upstream/.../engine/mod.rs:1303-1335`). The control
plane is never blocked (separate thread, separate lock; anti-pattern:
`hubdeck/.../engine.py:1325-1341`). Producer side: stream credits (ipc.md §6); the
daemon paces to the device ceilings — ~32 fps accepted, ~24 fps displayed, ~2.1 MB/s
(`upstream/PROTOCOL.md:91-97,244,265`).

## 4. Per-module limits

- ≤ 2 queued frames per target; wire frame ≤ 16 MB; total frame-queue memory ≤ 256 MiB
  (oldest waiting dropped beyond, counted).
- Animation: ≤ 300 frames, ≤ 30 fps playback (`hubdeck/.../config/schema.py:727`), loop
  flag in meta.
- LED writes (not frames, same discipline): ≤ 30/s per module pending soak test OPEN O2
  (`upstream/PROTOCOL.md:118-121`), always `save=0`.

## 5. Stats

`/streams/<target>/stats` (in state, subscribable):
`{"sent":N,"dropped":N,"fps":24.1,"bytes":N,"queue":0|1,"in_flight":true,"last_sha256":"…","last_seq":N,"writer_ms_p99":…}`.

## 6. Producer end-to-end (how a program pushes video)

```sh
ds stream push keys --target panel --dir frames/ --fps 30     # frames/ = 0001.png…
# or programmatically: open <dir>/stream.sock, auth, then per frame:
#   "DSF1" | u32 meta_len | {"target":"keys","seq":n,"ts_ms":…,"format":"jpeg","w":480,"h":272,"crc32":…} | u64 len | payload
# daemon replies per frame: {"op":"stream-ack","seq":n,"accepted":true} or {"op":"stream-nack","seq":n,"code":"E_LIMIT"}
```

`ds stream push … --file f.mp4` is NOT supported — decoding video is an external program's
job (R4): `ffmpeg -i v.mp4 frames/%04d.png` then `ds stream push`. The core never links
ffmpeg.

## 7. Test capture — what was sent and what the device could report (R5 answer)

The device has **no screen readback**: writes return byte counts only; feature reads
return saved config, not live state; input reports carry control events only
(`upstream/PROTOCOL.md:19-23, 35, 141-167, 219-220`; discovery/40 §6, 50 T3). Therefore
tests capture the strongest proxy:

1. **Host-side sent log** — ds-vdev records every report (bytes, chunk index, timestamp,
   per-write return count) and reassembles chunked images into whole frames
   (`hubdeck/tests/fakes/virtual_hub.py:17-130,196-246`); tests assert sha256 of
   reassembled frames against golden frames.
2. **Liveness** — feature reads answer in 45–48 ms (`upstream/PROTOCOL.md:141-143`);
   a read error = disconnect signal; `ds test frame-sha <control>` exposes the vdev sha.
3. **Device-originated state** — fader values `0x33`, orientation `0x35` are the only
   device-originated state; asserted as input events.
4. **Camera oracle (optional, never in CI)** — filmed checks for pixel-level facts
   (`upstream/PROTOCOL.md:70-87, 243-246`).

Gap, documented: tests cannot observe what pixels the module actually displayed, nor
device-side frame drops; they observe what the host sent + that the device accepted
(byte counts) + liveness. OPEN: USBPcap hunt for any screen-feedback report
(docs/open-questions.md C3).
