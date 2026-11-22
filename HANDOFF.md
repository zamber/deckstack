# Deckstack handoff

Deckstack is a new, CLI-first replacement base for the Cooler Master MasterHub and HubDeck controller stack. One daemon owns state and HID. A stateless `ds` CLI reads and writes state over a Unix socket. Images go on a separate binary stream. State is one JSON document with path addressing, revisions, transactions, and subscriptions. Bindings are data. Plugins are separate processes. The alpha target is Linux only, empty state, no import or migration, no presets, and no web UI. Design is in `PLAN.md`. Scope is in `docs/00-scope.md`.

## Current state

| Item | Value |
|---|---|
| Repo | `/var/home/catwars/repos/deckstack` |
| Main | see `git log --oneline -1` (this file is committed on top) |
| Review remote | `.review.git`, `review/main` synced and sha-verified on every merge |
| Worktrees | in flight only; `sh tools/wt.sh list` |
| Tickets | 65 total, 24 done, 2 doing, 6 ready, 33 backlog |
| Check | `sh tools/check.sh` prints `CHECK: ALL GREEN` (fixtures + board + cargo + conformance) |
| Cargo | `cargo test --workspace`, `cargo clippy --workspace -- -D warnings`, `cargo fmt --check` |
| E2E | `python tests/e2e/run.py` prints `RESULT: OK`; bench p99 ~6.7 ms |

Phase 1 core is complete: protocol, state, IPC control plane + frame plane, device transport, daemon, CLI, virtual device, conformance harness, systemd unit/runbook, and the walking-skeleton e2e (`ds set --file` -> vdev reassembled sha, watch, no-migrate, query p99 <= 10 ms).

Merged: DS-000..DS-006 (workflow), DS-090, DS-004, DS-010, DS-011, DS-012, DS-013, DS-014, DS-015, DS-016, DS-017, DS-018, DS-019, DS-020, DS-091, DS-092, DS-093, DS-094, DS-095. Details in `tickets/BOARD.md`.

## In flight and ready

| Ticket | Scope | Note |
|---|---|---|
| DS-021 | Real HID transport on Linux (hidraw/libusb enumeration, open, reader, safe reads) | in flight; hardware is connected |
| DS-040 | Event bus + router (topics, wildcards, ordering, backpressure) | in flight |
| DS-041 | Bindings: `/bindings` ids, when/payload conditions, `ds bind/unbind`, validation | after DS-040 |
| DS-030 | JPEG baseline 4:4:4 encoder + chunking, golden-frame tests | frames lane |
| DS-031 | Frame service: latest-frame-wins queues, writer threads, telemetry, 256 MiB cap | after DS-030 |
| DS-032 | `ds stream` CLI (push/stats/stop, --dir/--fps, --loop, ffmpeg decode) | after DS-013/DS-016 |
| DS-025 | udev rules, device permissions, install docs | phase 2 packaging |
| DS-096 | State schema index limits at object level (S8 gap found by DS-093) | small, contract-only |

Phase 2 hardware is available on this machine: five Cooler Master HID devices (2516:0202, 0204, 01fe, 0206, 01f6) and the AUKEY camera at `/dev/video2` (`/dev/video0` is the OBS virtual camera). Software-mode handshake and module init are DS-022; hot-plug DS-023; guard/handback DS-024; hardware smoke DS-026; camera harness DS-027/028; safety conformance DS-029.

## Workflow

```sh
python3 tools/tickets.py validate            # board gate
python3 tools/tickets.py ready               # ready set + next up
python3 tools/tickets.py add-touch ID --add PATH   # extend scope (lock + validate + sync)
sh tools/check.sh                            # single local gate
sh tools/wt.sh new DS-XXX --owner vibe/DS-XXX      # claim + worktree + branch
sh tools/pr.sh submit DS-XXX                 # in the worktree: gate, PR record, push to review
DS_STEWARD=1 sh tools/pr.sh review DS-XXX --approve
DS_STEWARD=1 sh tools/pr.sh merge DS-XXX
```

`pr.sh merge` rebases the branch; whitelisted rebase conflicts (`tickets/BOARD.md`, `GRAPH.md`, `index.json`, `tickets/DS-*.md`, `Cargo.lock`) are resolved by `tools/resolve-rebase.sh`, which also runs `tickets.py sync` afterwards. Source, contract, docs and run-log conflicts stay manual; the merge stops with the worktree intact. Regression test: `sh tests/tools/test-resolve-rebase.sh`.

Every merge: `sh tools/check.sh`, `python3 tools/tickets.py validate`, log a `docs/run-log.tsv` row, `git push review main` (the merge already verifies the sha).

## Vibe workers

Launch headless runs with `agent: auto-approve`. `smart-approve` still blocks legitimate work inside an authorized worktree: writing new files, standalone `python3`, and `sh tools/pr.sh submit` (evidence in the hook audit). Judge a run by its branch and PR, not by the job status: runs can end with the sandbox verify failure (`bwrap: ... .git: Not a directory` for linked worktrees) and an intermittent `NIL is not of type NUMBER` job error, while the worker committed and submitted correctly. Use `vibe.resume` when a run stops before committing.

## Known issues

1. Verify/acceptance in `vibe.run` runs through the command sandbox, which cannot bind a linked worktree's `.git` file; verification reports fail with a bwrap message. Verify worker artifacts by hand with `cargo test` and `sh tools/check.sh`.
2. Intermittent `NIL is not of type NUMBER` after a headless run (post-run bookkeeping). The run record and artifacts are written; the job status is wrong. Reproducer not yet found.
3. Build environment: `crates/ds-device` pulls `hidapi`/`mh-device` with `linux-static-libusb`; this machine gets libusb from Homebrew and `~/.cargo/config.toml` sets `PKG_CONFIG_PATH`. CI needs a libusb dev package or the same path.
4. `docs/run-log.tsv` rows up to 2026-11-22T16:59:11Z carry backfilled evidence (DS-094); keep new rows concrete.

## Read first

- `PLAN.md`, `docs/00-scope.md`, `docs/10-architecture.md`
- `docs/contracts/state.md`, `ipc.md`, `cli.md`, `streaming.md`, `actions.md`
- `tickets/BOARD.md`, `tickets/GRAPH.md`
- `AGENTS.md`, `docs/AGENT-WORKFLOW.md`, `agents/deckstack-steward.md`
- `docs/runbook.md`, `docs/run-log.tsv`

## Next work

- Land DS-021 and DS-040; then DS-041, DS-030, DS-031, DS-032, DS-025, DS-096.
- Phase 2 on the connected deck: DS-022 -> DS-023/024 -> DS-026 -> DS-027/028 -> DS-029.
- Do not self-merge; keep main green; log every decision.
