# Deckstack handoff

Deckstack is a new, CLI-first replacement base for the Cooler Master MasterHub and HubDeck controller stack. One daemon owns state and HID. A stateless `ds` CLI reads and writes state over a Unix socket. Images go on a separate binary stream. State is one JSON document with path addressing, revisions, transactions, and subscriptions. Bindings are data. Plugins are separate processes. The alpha target is Linux only, empty state, no import or migration, no presets, and no web UI. Design is in `PLAN.md`. Scope is in `docs/00-scope.md`.

## Current state

| Item | Value |
|---|---|
| Repo | `/var/home/catwars/repos/deckstack` |
| Main before this file | `3644d1c` |
| Code head before the log commit | `9f7e5a8` |
| Review remote | `.review.git`, `review/main` at `3644d1c` before this file |
| Worktrees | only `/var/home/catwars/repos/deckstack` |
| Tickets | 59 total, 12 done, 5 ready, 42 backlog |
| Check | `sh tools/check.sh` prints `CHECK: ALL GREEN` |
| Board | `python3 tools/tickets.py validate` prints `OK - 59 tickets, DAG valid, generated files in sync` |
| Cargo | `cargo test --workspace`, `cargo clippy --workspace -- -D warnings`, and `cargo fmt --check` pass |

Merged tickets:

| Ticket | Result | Commit |
|---|---|---|
| DS-000, DS-001, DS-002, DS-003, DS-005, DS-006 | workflow and tooling | see `tickets/BOARD.md` |
| DS-004 | Cargo workspace skeleton | `b9bc5fe` |
| DS-010 | `ds-protocol` | `d8f4551` |
| DS-011 | `ds-state` | `d718037` |
| DS-012 | `ds-ipc` control plane and `ds-client` | `86f2850` |
| DS-014 | `ds-device` | `c9ddb37` |
| DS-090 | workflow hardening smoke | see `tickets/BOARD.md` |

Crates:

| Crate | State |
|---|---|
| `crates/ds-protocol` | merged. Pinned `mh-protocol`, sealed reports, software-mode typestate, forbidden commands, LED `save=0`, backlight 0 refused. |
| `crates/ds-state` | merged. Document, paths, revisions, transactions, persistence, telemetry split, subscriptions, schema validation. |
| `crates/ds-ipc` | merged. Unix socket, NDJSON, hello and auth, scopes, error envelope, limits, per-operation scope table, revocation. |
| `crates/ds-client` | merged. Blocking request client and watch client. |
| `crates/ds-device` | merged. Transport trait, virtual transport, report codec, chunks, rate, URI, virtual socket protocol. |
| `crates/ds-daemon` | stub. The daemon ticket is DS-015. |
| `crates/ds-vdev` | stub. The virtual device ticket is DS-017. |

Not built yet: the daemon loop, the CLI, the frame plane, the virtual device, the conformance suite, the plugin host, the focus sender, the MCP bridge, hardware bring-up, the camera test setup, and the lights integration. The ready tickets cover the first five.

## Ready work

| Ticket | Scope | Files | Note |
|---|---|---|---|
| DS-013 | Frame plane. DSF1 stream socket, credits, acknowledgements, limits. | `crates/ds-ipc/src/frames/`, `docs/contracts/streaming.md`, `schemas/stream-frame.json` | Shares `crates/ds-ipc/` with DS-012. Keep work under `src/frames/`. |
| DS-015 | Daemon skeleton. State task, IPC server, device task, lifecycle, token, logs. | `crates/ds-daemon/` | Depends on DS-011, DS-012, and DS-014, all merged. |
| DS-016 | CLI. `get`, `set`, `watch`, `schema`, `describe`, `daemon`, `status`, output modes, exit codes. | `bins/ds/`, `docs/contracts/cli.md` | Depends on DS-012. |
| DS-017 | Virtual device. Module reports, chunk reassembly, inject, throttle, frame sha, `ds-vdevctl`. | `crates/ds-vdev/`, `docs/components/virtual-device.md` | Depends on DS-014. |
| DS-018 | Conformance suite. Black-box pytest for CLI, IPC, and state. Empty-state and no-migrate invariants. | `tests/conformance/` | Depends on DS-003 and DS-012. |

The five tickets can run in parallel. Start DS-013 and DS-017 first. Add DS-015, DS-016, and DS-018 as workers free up.

## After ready

- Phase 2 hardware: DS-021 through DS-029. The camera is `/dev/video2`, an AUKEY PC-W1. `/dev/video0` is the OBS virtual camera. Camera tickets are DS-027 and DS-028.
- Post-alpha: DS-070 lights.lan plugin, DS-071 selective porting, DS-072 Windows, DS-073 macOS, DS-074 debug UI. Do not start these in alpha.

## Commands

Read Known issues before the first merge.

Run these from `/var/home/catwars/repos/deckstack`:

```sh
python3 tools/tickets.py validate
python3 tools/tickets.py ready
sh tools/check.sh
```

Claim a ticket:

```sh
sh tools/wt.sh new DS-013 --owner vibe/DS-013
```

Worker setup uses `agent: auto-approve` in `/var/home/catwars/repos/deckstack/.worktrees/DS-013`. The worker reads `tickets/DS-013.md` and `AGENTS.md`, edits only the ticket touches, and runs the ticket acceptance. It commits `ticket(DS-013): ...` and runs `sh tools/pr.sh submit DS-013`. The worker does not merge. If log writes fail, the worker skips them. The repo, commits, and PR are the recoverable state.

Review and merge from main:

```sh
DS_STEWARD=1 sh tools/pr.sh review DS-013 --approve
DS_STEWARD=1 sh tools/pr.sh merge DS-013
```

After each merge:

```sh
sh tools/check.sh
python3 tools/tickets.py validate
git push review main
```

Add a decision log row:

```sh
sh /home/catwars/.config/autolith/skills/show-me-your-work/scripts/log.sh docs/run-log.tsv <phase> "<decision>" "<why>" "<evidence>" "<result>"
```

Recover an abandoned worker:

```sh
python3 tools/tickets.py recover
sh tools/wt.sh list
sh tools/pr.sh list
```

## Known issues

1. Rebase conflicts. Parallel tickets change `Cargo.lock` and the generated files `tickets/BOARD.md`, `tickets/GRAPH.md`, `tickets/index.json`, and `tickets/<id>.md`. `tools/pr.sh merge` stops on those conflicts. No resolver is in `tools/pr.sh`. Fix this first. Create a ticket for it before a parallel run with several tickets. The manual resolution that worked: take main's ticket files with `git checkout HEAD -- tickets/BOARD.md tickets/GRAPH.md tickets/index.json tickets/DS-XXX.md`. Take main's `Cargo.lock` and run `cargo generate-lockfile --offline`. Add the files. Run `GIT_EDITOR=true git rebase --continue`. Re-run the merge.
2. Ticket touches. `tickets.py` has no add-touch command. DS-012 needed `new:crates/ds-client/` added to `tickets/DS-012.md` on main by hand, then `python3 tools/tickets.py sync`. Add a command for this or record it as a ticket rule.
3. Vibe worker reliability. Workers hit turn limits and log permission errors. DS-012 stopped with uncommitted code and was finished by hand. Verify the artifact with `cargo test` and `sh tools/check.sh`. Do not trust a worker report. Use `vibe.resume` when a run stops short. Auto-approve is temporary.
4. Build environment. `crates/ds-device` pulls `mh-device` and `hidapi` with `linux-static-libusb`. This machine gets libusb from Homebrew. `~/.cargo/config.toml` sets `PKG_CONFIG_PATH=/var/home/linuxbrew/.linuxbrew/lib/pkgconfig`. That file is outside the repo. CI needs a libusb development package or the same path.
5. `tests/fixtures/validate.js`. DS-012 reported a pre-existing `not` short-circuit that lets an async message without `ok` pass. Confirm it before changing. It is outside current ticket touches.
6. Phase 1 is partial. DS-012 and DS-014 are verified. DS-013 is open. Do not call phase 1 complete.

## Review flags

A `deepseek-v4-pro` review of `docs/run-log.tsv` raised these evidence issues. The findings concern evidence, not correctness. The log needs better pointers.

| Row | Flag |
|---|---|
| `start` | `git log b9bc5fe..44b87bd` shows prior work, not the user authorization to continue. |
| `policy` | Auto-approve is risky. The row records a bare user instruction and no instruction text or hash. |
| `pivot` at 16:58:24Z | The DS-012 takeover has a run id and commit, but no test output or diff review in the row. |
| `pivot` at 16:59:11Z | The abandoned `pr.sh` resolver edit has no captured tool error. |
| `merge` at 16:23:03Z | The DS-011 conflict resolution is claimed, but the commit pointer only records the merge. |

The reviewer subagent did not return a result. The review used a bounded cross-model inference.

## Read first

- `PLAN.md`
- `docs/00-scope.md`
- `docs/10-architecture.md`
- `docs/contracts/state.md`, `docs/contracts/ipc.md`, `docs/contracts/cli.md`, `docs/contracts/streaming.md`
- `tickets/BOARD.md` and `tickets/GRAPH.md`
- `AGENTS.md`, `docs/AGENT-WORKFLOW.md`, and `agents/deckstack-steward.md`
- `docs/run-log.tsv`
- `docs/reviews/2026-11-22-independent-review.md` and `docs/reviews/2026-11-22-workflow-review.md`

## Next work

- Fix the rebase conflict resolver or create a ticket for it. Parallel merges will keep hitting it.
- Start DS-013 and DS-017.
- Add DS-015, DS-016, and DS-018 as workers free up.
- Review and merge each. Log each. Do not self-merge.
- Stop if `sh tools/check.sh` fails.
