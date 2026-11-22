# Contract: streaming / frame plane

R5/R11. Design: ADR-005, D06. Binary framing schema: `schemas/stream-frame.json` (B2).
Fixes S1/S6/S14/S16.

## 1. Surfaces — one target vocabulary (S6)

| Target | Module | Size | HID cmd | Quality |
|---|---|---|---|---|
| `keys` | M4315 panel | 480×272 | output `02 35`, idx 0 | JPEG q80 baseline |
| `key:<0-14>` | M4315 | 72×72 | output `02 36`, idx N (lands at x=5,104,203,303,402 / y=1,101,199 — `upstream/PROTOCOL.md:81`) | q90 |
| `dial` | M131E | 240×240 | output `02 45` | q85 |

`panel`/`screen` are not targets; `ds stream push keys …` means the panel. Per-key
uploads remain as `key:N`.

## 2. Frame formats

- Wire (producer → daemon), DSF1 binary on the stream socket (ipc.md §2): meta JSON
  validated against `schemas/stream-frame.json`; payload PNG/JPEG/GIF ≤ 16 MB wire,
  decode caps ≤ 4096×4096 px, ≤ 300 frames, ≤ 30 fps, min frame 0.02 s (precedents
  `hubdeck/.../render/images.py:21-23`, `control.py:36`, `upstream/.../plugins.rs:266`).
- On the device: **baseline JPEG only, standard Huffman tables, 4:4:4** — the daemon
  re-encodes; optimized-Huffman input is rejected with `E_FORMAT`
  (`upstream/PROTOCOL.md:87-96`).
- Chunking: `02 <cmd> <chunk u16 LE> <len u16 LE> <index> <last> <≤312 B>` in 320 B
  reports (`upstream/crates/mh-protocol/src/report.rs:132-171`).

## 3. Queue policy — latest-frame-wins (S16/M8 semantics stated explicitly)

Per target: one writer thread owns the HID handle. Queue = 1 frame **in flight** + 1
**waiting**; a new frame replaces only the waiting one and increments
`telemetry.dropped`. **The in-flight frame is never dropped or timed out**: a picture is
sent whole via `send_all` (upstream writer loop
`upstream/app/src-tauri/src/engine/mod.rs:1488-1530`), and abandoning a chunked transfer
mid-sequence would tear the image on the module. Control-plane independence is
structural, not a timeout: the writer threads (`keys pictures`/`dial pictures`,
`mod.rs:1475-1535`) share no lock with the state task, and no lock is held across HID I/O —
so a slow or stuck frame transfer cannot starve queries or bindings (the Python engine's
synchronous paint, `hubdeck/.../engine.py:1325-1341`, is the anti-pattern).

## 4. Per-frame state churn: none (S1)

A frame push updates **no durable state, no rev, no `/controls` subscription events**. It
is acknowledged on the stream socket (`stream-ack` seq) and counted in
`/telemetry/streams/<target>` (sampled ≤ 1 Hz, non-durable, no rev). An
`image: {kind:"frame"}` descriptor in state changes only when a `set` changes the source.
Bench assertion (docs/30-testing.md §5): 32 fps × 60 s ⇒ `rev` delta == 0 and
`/controls` events == 0.

## 5. Limits

- Per-target queue depth ≤ 2; wire frame ≤ 16 MB; total frame-queue memory ≤ 256 MiB
  (oldest waiting dropped beyond, counted).
- Animation ≤ 300 frames @ ≤ 30 fps, `loop` in meta.
- LED writes (same discipline): ≤ `/policy/led_rate_per_s` (default 30/s, interim pending
  soak OPEN O2, `upstream/PROTOCOL.md:118-121`), always `save=0`.

## 6. Stats

`ds stream stats keys --json` → control op `stream-stats` → reads
`/telemetry/streams/keys` (no rev):
`{"sent":960,"dropped":12,"fps":32.0,"bytes":6700000,"queue":1,"in_flight":true,"last_sha256":"…","last_seq":959,"writer_ms_p99":4.1,"sampled_at":"…"}`.

## 7. Producer end-to-end

```sh
ds stream push keys --file frame.png            # one frame (control: stream-open, then DSF1 on stream.sock)
ds stream push keys --dir frames/ --fps 30      # animation from a directory
ffmpeg -i v.mp4 frames/%04d.png && ds stream push keys --dir frames/ --fps 30   # video = external decode (R4); core never links ffmpeg
```

Programmatic: connect stream.sock, `DSctl\n{"op":"stream-open","target":"keys","credits":8}`
(NDJSON), then per frame `"DSF1"|u32 meta_len|meta|u64 len|payload`; meta per
`schemas/stream-frame.json`; replies `DSctl\n{"op":"stream-ack","seq":n}` or
`{"op":"stream-nack","seq":n}`.

## 8. Test capture — what was sent and what the device could report (R5 answer)

The device has **no screen readback**: writes return byte counts only; feature reads
return saved config; input reports carry control events only
(`upstream/PROTOCOL.md:19-23,35,141-167,219-220`; discovery/40 §6, 50 T3). Tests capture
the strongest proxy:

1. host-side sent log — ds-vdev records every report (bytes, chunk index, timestamp,
   per-write return count) and reassembles chunked images into whole frames
   (`hubdeck/tests/fakes/virtual_hub.py:17-130,196-246`); `ds-vdevctl frames keys` prints
   the sha256; golden frames compare sha256 (S10);
2. liveness — feature reads answer in 45–48 ms (`upstream/PROTOCOL.md:141-143`); HID read
   errors = disconnect;
3. device-originated state — fader values `0x33`, orientation `0x35`;
4. optional camera oracle for pixel-level facts, never in CI (`upstream/PROTOCOL.md:70-87,243-246`).

Gap, documented: tests cannot observe displayed pixels or device-side frame drops; they
observe what the host sent + byte-count acceptance + liveness (open-questions C3).
