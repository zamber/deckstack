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

`cargo bench -p ds-bench` against ds-vdev. Since DS-082 these budgets are **CI
gates** on the Linux job (`.github/workflows/ci.yml`, §8): the job fails beyond any
of them. The CI runs the short configuration (`--seconds 10`); the full 60 s run
stays available locally (`cargo bench -p ds-bench`).

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
| test (`check`) | Linux ubuntu-latest | schema/fixtures + unit + contract + conformance + acceptance + bench gates (§5, via `ds-bench --ci` + the actions bench — DS-082) | Linux alpha fully tested (virtual) incl. perf budgets |
| hardware | self-hosted Bazzite + deck + webcam | smoke suite + `deckstack-verify check` on ≥1 key and the panel (`docs/90-verification.md`) | Linux hardware, camera-verified |

**CI perf gates (DS-082), exact commands** — all run in the `check` job of
`.github/workflows/ci.yml` after one `cargo build --workspace` and `sh tools/check.sh`:

```sh
cargo run -p ds-bench -- --seconds 10 --ci     # streaming gate, JSON -> ds-bench-ci.json, exit 1 beyond budget
python3 tests/conformance/actions/run.py       # event-latency gate on the vdev (asserts the ≤ 2 s budget)
```

Gates (exit nonzero beyond budget): query-under-load p50 ≤ 2 ms / p99 ≤ 10 ms;
S1 no-churn (`rev` delta == 0, `/controls` events == 0 during 32 fps × 10 s);
stream throughput ≥ 0.95 × 32 fps accepted on the 16800 kbps-throttled vdev;
event latency: key input → visual ≤ 2 s end to end on the vdev (actions.md §5,
asserted by the actions conformance suite; the DS-045 in-process dispatch-p99
bench `tests/bench/actions.rs` — event → dispatch p99 ≤ 5 ms under 32 fps stream
load — is env-gated and not yet a cargo target: wiring it from `ds-bench` would
make `cargo fmt --check` format that DS-045-owned file, which this ticket may
not edit, so it stays out-of-tree until its owning ticket wires it);
memory bound: peak RSS (VmHWM) ≤ 256 MiB per run (the 10 min soak with RSS
growth < 100 MB, §5, stays a long-running/nightly job, not the per-PR gate).
Bench results upload as the `ds-bench-ci` artifact (`ds-bench-ci.json`).

**Margin policy** (shared runners): budgets carry headroom — measured on a dev
box the 10 s CI run reports p99 ≈ 0.15 ms against the 10 ms budget and 32.0 fps
accepted against the 30.4 fps floor — and the workflow sets
`concurrency: {group: <workflow>-<ref>, cancel-in-progress: true}` so two bench
runs never share a ref's runner. A flaky gate is retried once with a recorded
flake note in the job summary, escalated to a maintainer, never silently re-run.

No Windows/macOS CI gate exists in alpha (scope delta 1): those platforms are TODO
sections in `docs/40-platforms.md` §2–§3 — "do not start until Linux alpha is stable".
When a port starts, its gate is: Windows — conformance over the named pipe incl. p99
budget + second-account ACL test (O5/T12), then a VM with a deck; macOS — build+unit
runner, then a runner with a deck (O8).

m11: LED rate cap default 30/s is interim; soak (OPEN O2) must land **before phase 3
exit** (docs/60-roadmap.md).

## 9. Streaming bench harness (DS-033) — `crates/ds-bench`

`ds-bench` runs the phase-3 streaming bench against a **throttled ds-vdev + the daemon
stack assembled in-process** on a tmp dir — the same library wiring the DS-031
in-crate integration test uses (`VdevServer`, a `ds-ipc` control server over the
shared `ds-state` `Store`, and the `FrameService` serving `<dir>/stream.sock`;
`daemon::run` does not serve `stream.sock` yet, docs/components/frame-service.md
§wiring). No hardware and no prebuilt binaries are needed.

One run (default: 32 fps × 60 s to `key:0`, vdev throttled to `--throttle-kbps
16800` ≈ 2.1 MB/s, the simulated device write cap):

1. paces DSF1 frames on `stream.sock` at the target rate (credits/acks honoured,
   `stream-stop` at the end, queue drained);
2. while a query loop times `get /controls/keys/0` over `daemon.sock` (p50/p95/p99);
3. then reads `stream-stats` (the `stream-stats` op; `/telemetry` paths with `:` are
   not addressable by `get` — ipc-message.json's path pattern excludes `:`), checks
   `rev` delta and `/controls` subscription events across the window (S1), reads peak
   RSS from `/proc/<pid>/status` `VmHWM` (the bench process hosts the daemon tasks
   in-process, so the daemon pid is the bench pid), and verifies via the vdev control
   socket that the last pushed frame is the device's final reassembled frame.

Budgets (the §5 budgets above, enforced by the bench; exit 1 on any violation):

| Metric | Budget |
|---|---|
| query p50 / p99 under load | ≤ 2 ms / ≤ 10 ms |
| push rate and device-accepted rate | ≥ 0.95 × `--fps` (default 32 fps) |
| frame conservation | sent + dropped == pushed (drops counted in telemetry) |
| `rev` delta, `/controls` events (S1) | == 0 |
| peak RSS of the daemon (VmHWM) | ≤ 256 MiB |
| last frame on device | the last pushed frame (latest-frame-wins) |

Commands:

| Command | What |
|---|---|
| `cargo test -p ds-bench` | unit tests (percentile math, budget evaluation on a synthetic series, the `--ci` preset) + a 2 s end-to-end integration run — **the CI-suitable gate** |
| `cargo run -p ds-bench -- --seconds 5` | short run, prints the compact table (exit 0 = PASS) |
| `cargo run -p ds-bench -- --json` | one JSON object with every measurement + violations |
| `cargo run -p ds-bench -- --seconds 10 --ci` | **the CI gate (DS-082)**: deterministic, bounded 10 s run; JSON on stdout **and** written to `ds-bench-ci.json` (the CI artifact, §8) |
| `cargo run -p ds-bench` | full default run: 32 fps × 60 s |
| `cargo bench -p ds-bench` | the full 60 s bench target (harness = false, stable Rust) |
| `python3 tests/conformance/actions/run.py` | the event-latency gate on real binaries + vdev: key input → visual ≤ 2 s (actions.md §5) plus the single-event-path S1 assertions |

Knobs: `--seconds N`, `--fps N`, `--throttle-kbps N|off`, `--target keys|key:0-14|dial`,
`--json`, `--ci` (preset: implies `--json`, defaults `--seconds` to 10, writes the JSON
report to `ds-bench-ci.json`), `--out PATH` (with `--ci`: artifact path override).
Exit codes: 0 = all budgets met, 1 = budget violation, 2 = usage/harness error. Cleanup
is deterministic: shutdown → writer drain → vdev stop → tmp dir removed.

**Reproduce the CI gate locally** (same flags as `.github/workflows/ci.yml`):

```sh
cargo build --workspace                                   # once; provides the binaries the actions suite needs
sh tools/check.sh                                         # the full local gate
cargo run -p ds-bench -- --seconds 10 --ci                # writes ./ds-bench-ci.json, exit 0/1 on budgets
python3 tests/conformance/actions/run.py                  # the event-latency gate (≤ 2 s key input -> visual)
```

Recorded local run of `cargo run -p ds-bench -- --seconds 10 --ci` (dev box, exit 0,
`ds-bench-ci.json`): 320 pushed / 320 sent / 0 dropped, push = accepted = 32.04 fps,
query n=135021 p50 0.065 ms / p99 0.144 ms, `rev` delta 0, `/controls` events 0,
peak RSS 19.7 MiB, last frame on device, `ok: true`. Negative control
(`--seconds 3 --throttle-kbps 100`): exit 1 with
`accepted rate: 13.13 fps < 30.40 fps (target 32); sent=39 dropped=57` — the gate
fails beyond budget.
