# Component: frame/stream service (in-daemon module + `ds stream` CLI verbs)

Contract: `docs/contracts/streaming.md`. Design: ADR-005, D06.

- **Language:** Rust, inside ds-daemon. Producer side is `ds stream push …` (CLI) or any
  API client opening the binary stream endpoint.
- **Surfaces:** key panel 480×272 (cmd `0x35`), per-key 72×72 (cmd `0x36`, window table
  x=5,104,203,303,402 / y=1,101,199 — `upstream/PROTOCOL.md:81`), dial 240×240
  (cmd `0x45`). Sizes: `upstream/PROTOCOL.md:44,64`.
- **Frame path:** producer → binary stream socket (16-byte header + JSON meta + payload;
  payload is PNG/JPEG up to 16 MB wire cap) → daemon validates (decode caps: ≤ 4096×4096,
  ≤ 300 frames for animation) → re-encode to **baseline JPEG, standard Huffman, 4:4:4**,
  q80 panel / q90 key / q85 dial (matches `hubdeck/src/hubdeck/device/m4315.py:30-72`,
  `device/modules.py:35-65`; standard tables because optimized tables break the decoder,
  `upstream/PROTOCOL.md:87-88`) → per-module queue (latest-frame-wins, 1 in flight + 1
  waiting) → dedicated writer thread → chunked HID reports (312 B chunks on 320 B reports).
- **Queue policy:** a newer frame replaces a waiting one (never the in-flight one);
  drops counted in `/streams/<target>/stats {sent, dropped, fps, bytes, last_sha256}`.
  The control plane is never blocked by this path (separate threads, separate locks —
  the Python engine's synchronous paint is the anti-pattern,
  `hubdeck/src/hubdeck/engine.py:1325-1341`).
- **Limits:** per-target queue depth 2; wire frame ≤ 16 MB (`MAX_BODY` precedent:
  `upstream/app/src-tauri/src/engine/plugins.rs:266`); animation ≤ 300 frames @ ≤ 30 fps
  (`hubdeck/src/hubdeck/render/images.py:22-23`, `schema.py:727`); device ceilings
  ~32 fps accepted / ~24 fps displayed / ~2.1 MB/s (`upstream/PROTOCOL.md:91-97,244,265`).
- **Invariants:** frame bytes never enter the JSON state document; state holds only
  references (`/controls/keys/0/image = {kind: frame, target: "keys", seq: N, sha256}` or
  `{kind: color|text|icon, …}`); stats are state.
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
  ds stream push keys --target panel --file video-frame.png      # one frame
  ds stream push keys --target panel --dir frames/ --fps 30     # animation dir
  ds stream push dial --target screen --file anim.gif --loop
  ds get /streams/keys/stats --json
  ```
