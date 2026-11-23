# Streaming conformance suite (DS-034) — `tests/conformance/streaming/`

Black-box tests of the animation/GIF pipeline (`docs/contracts/streaming.md` §2/§5/§7)
driven through the real `ds` binary on a booted ds-vdev + ds-daemon stack.

- `harness.py` — the stack (ds-vdev + `ds daemon start` on a fresh temp dir) plus a
  `StreamBridge`: the daemon side of `<data>/stream.sock` speaking `DSctl\n` control
  lines and binary DSF1 frames with the real server's gates (session/credits, 20 ms
  per-target rate limit, 16 MB wire cap before the payload, CRC32, `frames`/`fps` schema
  bounds), acking/nacking, and forwarding accepted frames to the ds-vdev as chunked
  image reports (`ds-vdevctl frames` reassembles them) — or through the daemon's own
  encoder via `ds set … --file` in `encode` mode. The phase-1 daemon does not bind
  `stream.sock` itself yet (the DS-031 service is covered by its own Rust tests), so the
  harness provides the stream plane exactly like `bins/ds/src/stream/it.rs` does
  in-process in Rust.
- `run.py` — the cases (see the module docstring): GIF `--file --loop` on the dial,
  `--dir --fps` count/pacing/meta/final-frame, final-frame == encoder output of the last
  frame (golden `tests/golden/example-key0.sha256`), 301-frame refusal + 300 accepted,
  `--fps` bounds, >16 MB refusal (E_LIMIT exit 5), loop repeat, per-target queues, and
  the raw stream-plane rate-limit/wire-cap nacks.
- Images are generated deterministically in-process (PNG via `zlib`, GIF via a
  literal-only LZW writer) — no binary fixtures.

Run standalone:

    python3 tests/conformance/streaming/run.py            # RESULT: OK, exit 0
    python3 tests/conformance/streaming/run.py -k loop    # filtered

The Rust side of the animation plane is machine-checked by
`cargo test -p ds-daemon frames::animate` (`crates/ds-daemon/src/frames/animate.rs`):
multi-frame GIF/APNG decode, the ≤300 frames / ≤30 fps / ≥0.02 s / ≤16 MB / ≤4096×4096
limits with their E_LIMIT/E_FORMAT codes, paced pushes through the DS-031 queues
(first frame immediate, latest-frame-wins preserved), and the DSF1
`frames`/`fps`/`loop` meta mapping.

Where the caps are enforced: decode-time caps (wire bytes, dimensions, frame count,
fps, frame duration, total decoded bytes) in `animate.rs`; queue caps (depth 2 per
target, 256 MiB global, latest-frame-wins) in the DS-031 `frames::service`. Every target
— `keys`, each `key:N` (own queue), `dial` — runs under the same caps values.
