# 30 — Testing

R5/R10/R11. Design: D13/D15/D16. Everything runs in CI without hardware via ds-vdev.

## 1. Layers

| Layer | What | Command | Hardware? |
|---|---|---|---|
| Unit | crate tests; protocol safety (no builder output is forbidden — precedent `upstream/crates/mh-protocol/src/safety.rs:40-79`) | `cargo test --workspace` | no |
| Contract | every IPC message validated against `schemas/*.json`; CLI conformance suite drives the **real `ds` binary** black-box (Python pytest, deterministic fixtures) | `python tests/conformance/run.py` | no (ds-vdev) |
| Acceptance | full daemon + real drivers + ds-vdev at report level; key turn → LED + frame writes asserted on the virtual hub (port of `hubdeck/tests/test_acceptance.py`, model `tests/fakes/virtual_hub.py:17-130,196-246`) | `python tests/acceptance/run.py` | no |
| Golden frames | sha256 of reassembled frames on ds-vdev vs `tests/golden/*.sha256` | part of conformance | no |
| Bench | latency/throughput budgets (below) | `cargo bench -p ds-bench` | no (throttled vdev) |
| Hardware smoke | real deck: enumerate, software-mode, one frame per target, feature-read liveness, replug ≤ 5 s | `DS_HARDWARE=1 python tests/hw/smoke.py` | yes, manual/CI-device |

Rule carried over from `hubdeck/CLAUDE.md:101-103`: if real hardware and ds-vdev disagree,
the virtual model is wrong — fix the model first.

## 2. Virtual device at report level (ds-vdev)

Implements the HID report protocol in memory (docs/components/virtual-device.md):
software-mode features, chunked `02 36/35/45` reassembly into `frames[target]`, LED cmds,
input injection, feature reads (serial/faders/orientation), forbidden-command refusal, plus
knobs `--throttle-kbps` (simulates the ~2.1 MB/s cap, `upstream/PROTOCOL.md:265`),
`--drop-after` (hot-unplug), `--inject` (scripted input). Tests assert on **device-level
traffic**: exact report sequences, reassembled frame sha256, per-write return counts.

## 3. Black-box CLI conformance suite

`tests/conformance/` (Python, pytest): starts ds-vdev + ds-daemon, drives only the `ds`
binary and raw sockets. Covers: every grammar rule in `docs/contracts/cli.md` (exit codes
0-8 with the exact stderr shapes), every `set`/`bind` example, transactions + `E_CONFLICT`,
subscriptions + `dropped` counting, auth scopes, plugin abuse cases (docs/20-security.md
T3/T5), frame push + `ds test frame-sha` vs golden, hot-unplug/replug report sequence
(discovery/50 P6). Fixtures: `tests/fixtures/*.json` (deterministic state seeds).

## 4. Contract tests

`tests/contract/`: feeds recorded message corpora (valid + fuzzed) at the socket and
asserts every message validates against `schemas/ipc-message.json` / `state.json` /
`binding.json` / `plugin-manifest.json`, and that the daemon rejects invalid ones with the
documented `E_*` code. Schemas are compiled once in CI (`jsonschema` CLI) — "schemas parse
as JSON" is a CI gate.

## 5. Load / latency benchmarks (budgets, D13, R11)

`cargo bench -p ds-bench` against ds-vdev:

| Bench | Budget (fails CI beyond) |
|---|---|
| `query-under-load`: `get /controls/keys/0` loop while pushing 32 fps panel frames (throttled vdev) | p50 ≤ 2 ms, p99 ≤ 10 ms |
| `event-latency`: inject key events while streaming; event→binding dispatch | p99 ≤ 5 ms |
| `stream-throughput`: frames accepted per s per target on vdev; drops counted | ≥ 32 fps accepted, drops = sent − 32×s |
| `frame-encode`: JPEG q80 panel encode | ≤ 5 ms/frame (upstream measured 2.7–4.1 ms q98, `upstream/docs/PERF-AUDIT-0.2.md:33-41`) |
| `memory`: 10 min soak at max frame rate | RSS growth < 100 MB, no leak |
| Harness sensitivity proof: reproduce the Python stall — throttle vdev writes to 40 ms and assert the bench detects p99 blowout on a synchronous-write build (the anti-pattern), proving the harness would catch a regression to `hubdeck/.../engine.py:1325-1341` behaviour |

## 6. R5's video-test question — the exact answer

**The device cannot report what it displays.** Writes have no ack (byte count only);
feature reads return saved config, not live state; input reports carry only control
events, never screen content (`upstream/PROTOCOL.md:19-23,35,141-167,219-220`;
discovery/40 §6; discovery/50 T3). So tests capture the strongest available proxy:

1. host-side sent log: every report's bytes/chunk/timestamp/per-write return count,
   reassembled into whole frames with sha256 (golden-frame oracle), via ds-vdev;
2. liveness: feature reads answering in 45–48 ms (`upstream/PROTOCOL.md:141-143`) and HID
   read errors as disconnect signals;
3. device-originated state: fader values (`0x33`) and orientation (`0x35`);
4. optional camera oracle for pixel-level golden facts, never in CI
   (`upstream/PROTOCOL.md:70-87,243-246`).

Limits of the proxy: cannot see displayed pixels, cannot detect device-side frame drops,
byte-count ack ≠ display confirmation. Recorded as contradiction C3 with the USBPcap
experiment that would settle whether any feedback report exists (docs/open-questions.md).

## 7. Hardware smoke suite

`DS_HARDWARE=1 python tests/hw/smoke.py`: enumerate modules, software-mode handshake,
one frame per target + `GET`-feature liveness, LED write, replug timing ≤ 5 s
(`upstream/PROTOCOL.md:172-177`), forbidden-command refusal, guard handback
(`03 3A 00`/`03 49 00` on SIGTERM). Runs on a dedicated CI device, never gating PRs.

## 8. CI matrix

| Job | OS | Runs |
|---|---|---|
| test | Linux (ubuntu-latest) | unit + contract + conformance + acceptance + bench gates |
| schema | Linux | `jsonschema` validation of `schemas/*.json` + examples in docs parse |
| windows | windows-latest | unit + conformance (named pipe path) — no hardware |
| macos | macos-latest | unit + conformance (no hardware; hidapi exclusive-open noted, OPEN O8) |
| hardware | self-hosted Bazzite + real deck | smoke suite, nightly + on tag |
