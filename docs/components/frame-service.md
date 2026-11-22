# Component: frame/stream service (in-daemon module + `ds stream` CLI verbs)

Contract: `docs/contracts/streaming.md`. Design: ADR-005, D06.

- **Language:** Rust, inside ds-daemon. Producer side is `ds stream push …` (CLI) or any
  API client opening the binary stream endpoint.
- **Surfaces (unified target vocabulary, S6):** `keys` = 480×272 panel (cmd `0x35`),
  `key:N` = 72×72 key N (cmd `0x36`, window table x=5,104,203,303,402 / y=1,101,199 —
  `upstream/PROTOCOL.md:81`), `dial` = 240×240 (cmd `0x45`). Sizes: `upstream/PROTOCOL.md:44,64`.
  No `panel`/`screen` synonyms.
- **Frame path:** producer → binary stream socket (16-byte header + JSON meta + payload;
  payload is PNG/JPEG up to 16 MB wire cap) → daemon validates (decode caps: ≤ 4096×4096,
  ≤ 300 frames for animation) → re-encode to **baseline JPEG, standard Huffman, 4:4:4**,
  q80 panel / q90 key / q85 dial (matches `hubdeck/src/hubdeck/device/m4315.py:30-72`,
  `device/modules.py:35-65`; standard tables because optimized tables break the decoder,
  `upstream/PROTOCOL.md:87-88`) → per-module queue (latest-frame-wins, 1 in flight + 1
  waiting) → dedicated writer thread → chunked HID reports (312 B chunks on 320 B reports).
- **Queue policy:** a newer frame replaces only the waiting one; **the in-flight frame is
  never dropped or timed out** (S16/M8 — a picture is sent whole via `send_all`,
  `upstream/.../engine/mod.rs:1488-1530`; abandoning mid-chunk would tear the image).
  Control-plane independence is structural: separate threads, no lock held across HID I/O
  (the Python engine's synchronous paint is the anti-pattern,
  `hubdeck/src/hubdeck/engine.py:1325-1341`).
- **Per-frame state churn: none (S1):** a push updates no durable state, no `rev`; stats
  are sampled ≤ 1 Hz into non-durable `/telemetry/streams/<target>`; an
  `image:{kind:"frame"}` descriptor changes only on `set`.
- **Limits:** per-target queue depth 2; wire frame ≤ 16 MB (`MAX_BODY` precedent:
  `upstream/app/src-tauri/src/engine/plugins.rs:266`); animation ≤ 300 frames @ ≤ 30 fps
  (`hubdeck/src/hubdeck/render/images.py:22-23`, `schema.py:727`); device ceilings
  ~32 fps accepted / ~24 fps displayed / ~2.1 MB/s (`upstream/PROTOCOL.md:91-97,244,265`);
  LED ≤ `/policy/led_rate_per_s` (default 30/s, interim pending OPEN O2).
- **Invariants:** frame bytes never enter the JSON state document; state holds only
  references (`image = {kind:"frame", target, seq, sha256}` or `{kind: color|text|icon|file, …}`);
  live stats are telemetry, not state (`/telemetry/streams/<target>`); frame pushes never
  bump `rev` (S1).
- **Failure modes:** module gone → frames dropped + counted, `Disconnected` event;
  encode error → `E_FORMAT`, frame dropped; writer thread stuck > 5 s → watchdog drops
  the in-flight frame and logs (never blocks control).
- **Test capture (R5):** ds-vdev reassembles chunked reports into whole frames and records
  every write (port of `hubdeck/tests/fakes/virtual_hub.py:17-130,196-246`); tests assert
  sha256 of reassembled frames (golden frames) + sent-log + per-write return counts +
  feature-read liveness. The device itself has no screen readback — proxy and limits in
  `docs/30-testing.md` §R5.
- **Dependencies:** ds-device, ds-state (stats + refs), image codec crates (`jpeg-encoder`
  with standard tables, `png`).
- **Test command:** `cargo test -p ds-daemon::frames`; bench: `cargo bench -p ds-bench
  -- stream-32fps-query-p99` (budget in docs/30-testing.md).
- **Minimum working example:**
  ```sh
  ds stream push keys --file frame.png        # one frame (target = keys = panel)
  ds stream push keys --dir frames/ --fps 30  # animation dir
  ds stream push dial --file anim.gif --loop
  ds stream stats keys --json                 # reads /telemetry/streams/keys (no rev)
  ```
