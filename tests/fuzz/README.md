# tests/fuzz — DS-080 seeded fuzz suite

`python3 tests/fuzz/run.py` boots ds-vdev + ds-daemon (helpers imported from
`tests/conformance/run.py`) and runs a **deterministic, seeded** corpus of mutated
inputs against the real sockets:

- **daemon.sock (NDJSON control plane)** — seeds from `tests/fixtures/ipc-examples.json`
  (the checked-in example corpus), mutated JSON-aware (deleted keys, unknown ops,
  `ok` in a request, wrong id/protocol/path/op types, extra keys) and byte-level
  (bit flips, truncation, splices, inserted garbage, duplicate lines, partial line +
  disconnect, non-UTF-8, rare standalone > 1 MiB line for the `E_LIMIT` + close path).
- **daemon.sock (DSF1 bytes)** — valid `DSF1` frames per `schemas/stream-frame.json`
  (magic | u32 meta_len | meta | u64 payload_len | payload, CRC32 via zlib) with
  header lies (meta_len/payload_len over the caps), meta lies (`panel` target,
  `frames: 301`, `fps: 30.5`, `w: 4097`, wrong `max_payload_bytes`, extra keys, dropped
  required keys), CRC mismatch, bad magic, truncation, bit flips. On the control socket
  these must get a typed `E_SCHEMA` (binary is not one JSON object) or the id-less
  `E_LIMIT` + close when over the 1 MiB line cap.
- **vdev.sock (binary device framing)** — `u32 body_len | kind | pid | payload`
  (cap 4096, `docs/components/virtual-device.md`): body_len lies over the cap, unknown
  kinds, pid lies, truncation, payload flips/extensions, raw garbage, valid CLOSE.
  Must get a clean close or a typed reply frame (ack 0x82 / answer 0x83 / error 0x84),
  never a crash or hang.
- **vdev.sock.ctl (ds-vdevctl line protocol)** — mutated command lines; must get
  `OK`…`.` / `ERR` / EOF.

## Verdicts and findings

Every case must end in a **typed verdict**: an `E_*` envelope from the schema enum, an
`ok` reply, a reply frame, or a clean close (`eof`/`reset`). A first-read timeout is a
**hang finding**: the run fails, the case stays in the corpus, nothing is retried or
masked. After every 200 cases and at the end the daemon and vdev are health-checked
(process alive, ping answered, no `panicked`/`abort` in either log).

## Determinism and the corpus

The generator is `random.Random(0xD5F080)` — same seed, same corpus, same verdicts
(verified: two runs produced identical verdict histograms). Each run rewrites
`tests/fuzz/corpus/cases.jsonl` (case kind + hex payload, capped at 8 KiB per case, +
the recorded verdict). Replay:

```sh
python3 tests/fuzz/run.py --replay     # re-runs the corpus from the same seed and
                                       # asserts every recorded verdict reproduces
```

## Usage / bounds

```sh
python3 tests/fuzz/run.py               # 2000 cases, ~15 s, exit 0, FUZZ RESULT: OK
python3 tests/fuzz/run.py --cases 500   # fewer cases (env-free knob)
```

A 60 s SIGALRM watchdog bounds the whole run (boot + cases), plus per-read timeouts
(2 s first read = hang detection, 0.3 s drain). Teardown always kills daemon + vdev and
removes the temp dir, so no processes/sockets/temp dirs leak.

## Reachability note

The running daemon binds only `daemon.sock` in this tree (`stream.sock` is not wired
into `daemon::run` yet — see `tests/limits/README.md`), so the DSF1 *decoder* itself is
not socket-reachable here; DSF1 mutants are driven against the real framings that do
exist (the control NDJSON framing and the vdev binary framing), and the DSF1 meta caps
are covered by the `ds stream push` caps in `tests/limits/` plus the schema fixture
gate and the ds-ipc cargo tests.
