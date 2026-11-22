# Golden frames (ticket DS-030)

Golden vectors for the device frame wire format (`docs/contracts/streaming.md` §2/§8):
a fixed input image is re-encoded by `ds-daemon`'s frame plane to **baseline JPEG,
standard Huffman tables, 4:4:4** and chunked into 320 B image reports; the exact bytes
are checked in here and asserted by `cargo test -p ds-daemon frames::golden` and by
`tests/e2e/run.py` step 3 (through ds-vdev reassembly).

These are test fixtures only — never installed as deck defaults (empty-state invariant).

## Encoder pin (golden sha drift guard)

- Encoder crate: **`jpeg-encoder` `=0.6.1`** (pinned in `crates/ds-daemon/Cargo.toml`;
  chosen because it was the small, pure-Rust baseline encoder with explicit standard
  Huffman tables and 4:4:4 sampling control, and it is vendored in the local cargo
  cache; recorded here per the ticket).
- Decoder crates for the wire payloads: `png 0.18`, `zune-jpeg 0.5`, `gif 0.14`.
- Settings: `SamplingFactor::F_1_1` (4:4:4), `optimized_huffman_tables(false)` (standard
  Annex K tables), `progressive(false)` (baseline SOF0), quality per target:
  `keys` panel q80, `key:N` q90, `dial` q85.

The encoder is deterministic for a pinned version (scalar path; the `simd` feature is
not enabled), so these bytes are stable across debug/release builds on one toolchain.
If the pin is ever bumped, regenerate the fixtures **deliberately** and note the new
version here:

```sh
DS_GOLDEN_REGEN=1 cargo test -p ds-daemon frames::golden
```

## Files

| File | Meaning |
|---|---|
| `input.png` | The fixed deterministic input: 72×72 RGB PNG, pixel `(x,y) = ((x*255)//71, (y*255)//71, (x*7 + y*13) % 256)`. sha256 `edbae06fe607792cadcfa0f570f70dd41a6e0a6babc9554a2ab68bd1051496ae`. |
| `key0.jpg` | Exact JPEG bytes of `encode_frame(key:0, input.png)` — baseline, standard Huffman, 4:4:4, q90. |
| `key0.sha256` | sha256 hex of `key0.jpg` (the golden-frame oracle value for `key:0`). |
| `key0.chunks.bin` | Exact chunk sequence for `key0.jpg` on `key:0`: the concatenated 320 B reports `02 36 <chunk u16 LE> <len u16 LE> 00 <last> <≤312 B payload>`. |
| `example-key0.sha256` | sha256 of `encode_frame(key:0, examples/key.png)` — what `tests/e2e/run.py` step 3 compares the ds-vdev-reassembled frame against (the e2e pushes `examples/key.png` to `key:0`). |

Regenerate all of them with the command above (run from the repo root, in the ticket
worktree); the normal (non-regen) test run only asserts equality.
