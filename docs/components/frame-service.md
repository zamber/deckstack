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
  LED ≤ `/policy/led_rate_per_s` **per module** (default 30/s; faders 25/s) via the
  latest-wins LED lane (`crates/ds-daemon/src/led.rs`, ADR-018, streaming.md §5).
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

## In-daemon implementation (DS-031, `crates/ds-daemon/src/frames/`)

| File | Role |
|---|---|
| `service.rs` | `FrameService`: per-target queue slots (1 in flight + 1 waiting, latest-frame-wins), the global 256 MiB frame-queue budget, the `FrameSink` impl that routes accepted DSF1 stream-socket frames into the queues, `serve_stream_socket` (binds `<dir>/stream.sock` 0600 with the DS-013 `StreamServer`), `stop_stream`, `push_image` (DS-095 state-push path through the same queues), stats views, graceful join. |
| `writer.rs` | One writer thread **per module** owning the send side of the transport for that module (`keys` panel + `key:N` share the M4315 handle, so one writer serializes their chunked transfers; `dial` has its own). Takes the oldest waiting frame of the module whose slot has no in-flight frame, marks it in flight, encodes → chunks → sends it **whole**, paced by a token bucket at the ~2.1 MB/s device ceiling. No lock is held across device I/O and the writer shares no lock with the state task; on shutdown it drains the queue and exits; on a device error the frame is dropped + counted and `deck/device/<serial>/disconnected` is published. |
| `telemetry.rs` | The ≤ 1 Hz sampler: writes `/telemetry/streams/<target>` (`sent, dropped, fps, bytes, queue, in_flight, last_sha256, last_seq, writer_ms_p99, sampled_at`) through `Store::set_telemetry` — non-durable, never persisted, `rev` never bumped (S1). |
| `encode.rs` / `chunk.rs` | DS-030: wire-image validation + re-encode to baseline JPEG (standard Huffman, 4:4:4, per-target quality) and the 312 B chunked image reports. The writer thread is the only caller on the send path. |

**Queue policy implementation:** the accept path (`FrameSink::on_frame` →
`FrameService::push_frame`) takes only the slots lock: budget check → evict oldest
waiting frame across targets while the 256 MiB budget would be exceeded (counted in that
target's `dropped`) → refuse the new frame with typed `E_LIMIT` when only in-flight
frames remain and the budget would still be exceeded → replace only the waiting slot
(counted) → wake the module writer. The in-flight frame is owned by the writer and never
dropped, replaced or timed out.

**Budget:** `FRAME_QUEUE_BUDGET_BYTES = 256 MiB`, counting every queued (waiting +
in-flight) frame byte across all targets. The control plane is never affected by frame
traffic: the queue is bounded (depth 2 per target), the accept path never does device I/O,
and the writer threads are structurally independent of the state task.

**Telemetry fields** (`/telemetry/streams/<target>`, `schemas/state.json`
`telemetry.streams`): `sent` (frames fully sent), `dropped` (replaced waiting frames +
budget evictions + encode/send failures), `fps` (send rate over the sampling window),
`bytes` (device-JPEG bytes sent), `queue` (waiting depth, 0/1), `in_flight` (bool),
`last_sha256`/`last_seq` (last completed frame), `writer_ms_p99` (p99 whole-frame send
time), `sampled_at` (RFC 3339). Sampled at 1 Hz; reads carry no `rev`.

**Wiring points in `daemon::run`:** the daemon opens the store as an `Arc` shared with
the state task, spawns the service with no transport, and binds the socket:
`let frames = FrameService::spawn(store.clone(), None, bus.clone(), shutdown.clone());
let (_stream_bus, stream_sock) = frames.serve_stream_socket(&data_dir,
StreamConfig::default())?;`. The device task attaches its `Arc<dyn Transport>` after the
software-mode handshake (`FrameService::set_transport`) and clears it when the device goes
away, so a frame accepted before the handshake is refused instead of being sent in
hardware mode. The stream socket then serves `DSctl stream-open`/`DSF1` with this service
as the sink.
The in-crate tests exercise exactly this stack in-process (DS-013 stream server + ds-vdev +
`Store`), including a 10-frame burst on a throttled vdev: the final device frame is the
last pushed one, `dropped > 0`, `sent + dropped == 10`, telemetry shape correct, `rev`
delta 0.

**Tests** (`cargo test -p ds-daemon frames::`): queue policy unit tests with a gated mock
transport (replace-waiting-only, in-flight never dropped, budget cap refusal `E_LIMIT` +
oldest-waiting eviction counted, dropped counters, stopped-stream `E_SCOPE` refusal and
drain, no-transport drop, one writer per module, telemetry shape + no rev) and the ds-vdev
integration test described above.
