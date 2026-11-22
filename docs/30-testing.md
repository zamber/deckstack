# 30 — Testing

R5/R10/R11. Design: D13/D15/D16. Everything runs in CI without hardware via ds-vdev.

## 1. Layers

| Layer | What | Command | Hardware? |
|---|---|---|---|
| Schema/fixtures | schemas parse, $refs resolve, example corpus validates, regex regressions | `sh tests/fixtures/validate.sh` (node; `tests/fixtures/validate.py` with jsonschema in CI) | no |
| Unit | crate tests; protocol safety (precedent `upstream/crates/mh-protocol/src/safety.rs:40-79`) | `cargo test --workspace` | no |
| Contract | IPC messages validated against `schemas/ipc-message.json`; CLI conformance drives the **real `ds` binary** black-box (pytest, deterministic fixtures) | `python tests/conformance/run.py` | no (ds-vdev) |
| Acceptance | full daemon + real drivers + ds-vdev at report level (port of `hubdeck/tests/test_acceptance.py`, model `tests/fakes/virtual_hub.py:17-130,196-246`) | `python tests/acceptance/run.py` | no |
| Golden frames | sha256 of reassembled frames on ds-vdev vs `tests/golden/*.sha256`; inspect via `ds-vdevctl frames <target>` (S10) | part of conformance | no |
| Bench | budgets below | `cargo bench -p ds-bench` | no (throttled vdev) |
| Hardware smoke | real deck suite | `DS_HARDWARE=1 python tests/hw/smoke.py` | yes |

Rule carried over (`hubdeck/CLAUDE.md:101-103`): if real hardware and ds-vdev disagree, the
virtual model is wrong — fix the model first.

## 2. Virtual device at report level (ds-vdev)

ds-vdev implements the HID report protocol in memory (docs/components/virtual-device.md):
**all module kinds incl. rollers M032LE (`0x0204`, input `01 2A … 01 02 r1 r2`)** (m7),
software-mode features, chunked `02 36/35/45` reassembly into `frames[target]`, LED cmds,
input injection, feature reads (serial/faders/orientation), forbidden-command refusal, plus
`--throttle-kbps` (simulates ~2.1 MB/s, `upstream/PROTOCOL.md:265`), `--drop-after`
(hot-unplug), `--inject`. Tests assert on device-level traffic: exact report sequences,
reassembled sha256, per-write return counts. Inspection: `ds-vdevctl` (m10, S10).

## 3. Black-box CLI conformance suite

`tests/conformance/` (pytest): starts ds-vdev + ds-daemon, drives only the `ds` binary and
raw sockets. Covers the full cli.md grammar (exit codes 0–8 with exact stderr shapes),
`--json` on streaming verbs → exit 1 (S9), transactions + `E_CONFLICT`, subscriptions +
`dropped`, auth scopes + `E_SCOPE` (m4), token rotation (S12/T9), plugin abuse (T3/T5/T6/
T11: fork-bomb, `/bin/sh -c` exec refusal, traversal, net glob edges), focus-chain atomicity
(S7), frame push + `ds-vdevctl frames` sha vs golden, hot-unplug/replug sequence (P6).

## 4. Contract tests

`tests/contract/`: fuzzed + recorded corpora validated against the schemas; invalid
messages must get the documented `E_*`. The checked-in corpus is
`tests/fixtures/ipc-examples.json` — the REVIEW command is now:
`sh tests/fixtures/validate.sh` (must print ALL CHECKS PASSED).

## 5. Load / latency benchmarks (budgets, D13, R11) — incl. S1 assertions

`cargo bench -p ds-bench` against ds-vdev:

| Bench | Budget (CI fails beyond) |
|---|---|
| `query-under-load`: `get /controls/keys/0` loop while pushing 32 fps frames | p50 ≤ 2 ms, p99 ≤ 10 ms |
| `frame-push-no-state-churn` (S1): 32 fps × 60 s | `rev` delta == 0; `/controls` subscription events == 0; telemetry sampled ≤ 1 Hz |
| `event-latency`: inject key events while streaming | p99 ≤ 5 ms |
| `stream-throughput`: throttled vdev | ≥ 32 fps accepted; `dropped` == sent − accepted |
| `frame-encode`: JPEG q80 panel | ≤ 5 ms/frame (upstream 2.7–4.1 ms q98, `upstream/docs/PERF-AUDIT-0.2.md:33-41`) |
| `memory`: 10 min soak at max rate | RSS growth < 100 MB |
| harness sensitivity: throttled vdev (40 ms/writes) must blow up p99 on a synchronous-write build (reproduces the `hubdeck/.../engine.py:1325-1341` stall) — proves the harness detects regressions | p99 > 100 ms detected |

## 6. R5's video-test question — exact answer

**The device cannot report what it displays** (no per-write ack; feature reads return
saved config; input reports carry control events only — `upstream/PROTOCOL.md:19-23,35,
141-167,219-220`; discovery/40 §6, 50 T3). Tests capture the strongest proxy: host-side
sent log + reassembled-frame sha256 (golden) + per-write byte counts + feature-read
liveness (45–48 ms) + fader/orientation readback + optional camera oracle (never CI).
Limits: cannot see displayed pixels or device-side drops (open-questions C3; USBPcap
experiment would settle it).

## 7. Hardware smoke suite

`DS_HARDWARE=1 python tests/hw/smoke.py`: enumerate, software-mode handshake, one frame
per target + liveness, LED write, replug ≤ 5 s (`upstream/PROTOCOL.md:172-177`),
forbidden refusal, guard handback. Self-hosted Bazzite device, nightly + on tag.

## 8. CI matrix (alpha = Linux only; S15/M6/M7 honesty)

| Job | OS | Runs | Claims |
|---|---|---|---|
| test | Linux ubuntu-latest | schema/fixtures + unit + contract + conformance + acceptance + bench gates | Linux alpha fully tested (virtual) |
| hardware | self-hosted Bazzite + deck + webcam | smoke suite + `deckstack-verify check` on ≥1 key and the panel (`docs/90-verification.md`) | Linux hardware, camera-verified |

No Windows/macOS CI gate exists in alpha (scope delta 1): those platforms are TODO
sections in `docs/40-platforms.md` §2–§3 — "do not start until Linux alpha is stable".
When a port starts, its gate is: Windows — conformance over the named pipe incl. p99
budget + second-account ACL test (O5/T12), then a VM with a deck; macOS — build+unit
runner, then a runner with a deck (O8).

m11: LED rate cap default 30/s is interim; soak (OPEN O2) must land **before phase 3
exit** (docs/60-roadmap.md).
