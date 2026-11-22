# ADR-005 — Frame plane: bytes on a separate binary stream, latest-frame-wins

Status: accepted (planning).

## Context

Tension 2: state is data, frames are bytes. Large frames must stay out of the JSON state
path; local video must not be slow or non-portable; control must not starve under frame
load (R2/R5/R11). Device ceilings: ~32 fps accepted, ~24 fps displayed, ~2.1 MB/s
(`upstream/PROTOCOL.md:91-97,244,265`).

## Decision

Frames travel as binary on a dedicated stream endpoint (`DSF1` framing, ipc.md §2), never
inside the state document — state holds only refs (`{kind:"frame", target, seq, sha256}`).
Per target: one writer thread, queue = 1 in flight + 1 waiting, newer replaces waiting
(latest-frame-wins, `upstream/.../engine/mod.rs:1303-1335`), drops counted in
`/streams/<target>/stats`. Producer side: stream credits. Encoding: daemon re-encodes any
input (PNG/JPEG/GIF ≤ 16 MB, ≤ 4096×4096, ≤ 300 frames) to baseline JPEG, standard
Huffman, 4:4:4, chunked 312 B reports. No shared memory (not portable to named pipes);
local socket throughput dwarfs the 2.1 MB/s device cap, so bytes-over-socket is not a
video bottleneck.

## Alternatives

- **base64 frames in JSON state** — rejected: 33% bloat, state-lock pressure, blocks
  queries.
- **shm ring buffers** — rejected: lifecycle complexity, not portable to Windows pipes;
  premature until benchmarks show socket overhead (none expected at 2.1 MB/s).
- **Synchronous writes on the control thread (Python engine)** — rejected: observed to
  starve control (`hubdeck/.../engine.py:1325-1341`).

## Consequences

Query budgets hold under frame load (bench gate). Frame identity = (target, seq, sha256);
tests capture reassembled frames on ds-vdev (docs/contracts/streaming.md §7).

## References

BRIEF.md R2/R5/R11; docs/contracts/streaming.md; decision log D06.
