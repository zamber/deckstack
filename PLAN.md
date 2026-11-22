# PLAN — deckstack in one page

Status: **planning** (documents + schemas only). Goal and requirements: `BRIEF.md` (R1–R12).

## The shape

One long-lived Rust daemon (`ds-daemon`) owns all state and hardware. A stateless Rust CLI
(`ds`) does one read/mutate per invocation over a Unix socket (named pipe on Windows);
the same API is exposed over WebSocket/HTTP for UIs and agents. State is one JSON document
addressed by paths, mutated in revision-stamped transactions, subscribable. Frames travel
on a separate binary stream (latest-frame-wins, bounded queues, dedicated writer threads)
and never enter the JSON state. Bindings are data; the core action vocabulary is
`set|emit|plugin|exec|sleep` with exec argv-only and capability-gated. Plugins are separate
processes with manifests, JSON-Schema params, capability grants and rlimits; a Python
wrapper keeps the existing hubdeck plugin ecosystem alive. The device layer reuses
`mh-protocol`/`mh-device` as dependencies. Repo: umbrella monorepo, per-component
releases via `git subtree split`.

```
external senders (ds-focus, cron, webhooks, agents)
        │  ds emit / API          ┌──────────────┐   HID (mh-device)   ┌─────────┐
        ▼                         │  ds-daemon   │◄───────────────────►│ MasterHub│
   ds CLI ── NDJSON ─────────────►│ state (JSON) │   frame plane (bin) │ modules  │
   UIs/MCP ── WS/HTTP ──────────►│ router+bindings│────────────────────►│ (screens)│
                                   │ plugin host  │  newline-JSON stdio │ plugins │
                                   └──────────────┘◄────────────────────┘─────────┘
```

## Load-bearing decisions (full trail: docs/decision-log.tsv; ADRs in docs/adr/)

| # | Decision | ADR |
|---|---|---|
| 1 | Daemon in Rust, one binary; reuses mh-device/hidapi | [ADR-001](docs/adr/001-daemon-language.md), [ADR-010](docs/adr/010-device-layer-reuse.md) |
| 2 | CLI in Rust sharing the daemon's client crate; one-shot + `watch` | [ADR-002](docs/adr/002-cli-language.md) |
| 3 | IPC: Unix socket / Windows named pipe, NDJSON + binary frame stream, scoped token | [ADR-003](docs/adr/003-ipc-transport.md) |
| 4 | State: single JSON document, path addressing, revisions, transactions, subscriptions | [ADR-004](docs/adr/004-state-model.md) |
| 5 | Frames are bytes on a separate stream; latest-frame-wins 1+1 queue per module | [ADR-005](docs/adr/005-frame-transport.md) |
| 6 | Plugins: process-per-plugin, manifest, capability grants, rlimits+Landlock; Python wrapper | [ADR-006](docs/adr/006-plugin-isolation.md) |
| 7 | Actions: bindings-as-data, core vocabulary set/emit/plugin/exec/sleep, no eval, no shell, no catalog | [ADR-007](docs/adr/007-action-model.md) |
| 8 | Repo: umbrella monorepo + `git subtree split` per component; schemas/ versioned as contracts | [ADR-008](docs/adr/008-repo-topology.md) |
| 9 | Security: local-only, scoped tokens, caps, image limits, no RCE surface by default | [ADR-009](docs/adr/009-security-model.md) |
| 10 | Linux-first with ds-platform shim; watchers/senders external | [ADR-003](docs/adr/003-ipc-transport.md), [ADR-008](docs/adr/008-repo-topology.md), docs/40-platforms.md |

Design tensions 1–8 from the task resolve as: (1) Rust one-shot CLI ≈1–3 ms startup, p50 ≤ 2 ms / p99 ≤ 10 ms query budget under 32 fps frame load, proven by bench (ADR-002/004/005); (2) frame plane separate from JSON state (ADR-005); (3) bindings reach plugins/exec only via caps, argv-only (ADR-007/009); (4) daemon is a router + tiny core vocabulary, not an action catalog (ADR-007); (5) wrapper guarantees process isolation and the wire contract, not deep API compat (ADR-006); (6) reuse mh crates as deps, dual license + NOTICE (ADR-010); (7) one daemon + one CLI binary, systemd-user-or-detached start, guard child handback (ADR-001, docs/components/daemon.md); (8) core POSIX-free via ds-platform/ds-ipc shims (docs/40-platforms.md).

## Phase table (details + exit criteria: docs/60-roadmap.md)

| Phase | Slice | Observable exit criterion |
|---|---|---|
| 0 | Contracts freeze: schemas parse, CI skeleton | `schemas/*.json` validate; CI green |
| 1 | **Walking skeleton on the virtual device**: daemon + CLI + socket + state get/set + one key image via ds-vdev | `ds set /controls/keys/0/image --file k.png` then conformance test asserts the reassembled frame sha256 on ds-vdev |
| 2 | Hardware bring-up: real modules, safety, hot-plug, guard handback | hardware smoke suite green; keys back ≤ 5 s after replug |
| 3 | Streaming phase: frame plane, backpressure, stats, benchmarks | 32 fps push sustained; query p99 ≤ 10 ms under load; drops counted |
| 4 | Events/bindings/actions phase | key press → plugin → visual update chain passes on ds-vdev |
| 5 | Plugin phase: host, manifest, Python wrapper, huemux ported | existing hubdeck huemux plugin drives a fake lamp via wrapper |
| 6 | Sender phase: ds-focus (KWin/Wayland), ds-mcp | focus change recolours a key via CLI chain; MCP tool list works |
| 7 | UI phase: ds-web as pure WS client | browser sets a key image; zero logic in UI |
| 8 | Migration: import hubdeck config, coexistence, reversible cutover | `ds migrate import` from ~/.config/hubdeck produces working state; rollback = start old AppImage |

## First walking skeleton (phase 1 — a coder starts here)

1. `git clone` this repo; `cargo build --workspace` (crates: ds-protocol, ds-device over
   mh-protocol/mh-device git deps, ds-state, ds-ipc, ds-daemon, ds, ds-vdev).
2. `ds-vdev --socket /tmp/vdev.sock &` — report-level virtual MasterHub (keys+dial+M0).
3. `ds daemon start --device virtual:/tmp/vdev.sock` — binds
   `$XDG_RUNTIME_DIR/deckstack/daemon.sock`, writes 0600 token.
4. `ds set /controls/keys/0/color '#3cc86e'` → `ds get /controls/keys/0 --json` round-trips.
5. `ds set /controls/keys/0/image --file examples/key.png` → ds-vdev reassembles the
   chunked `02 36` reports; `ds test frame-sha /controls/keys/0` prints sha256; the
   conformance suite asserts it.
6. Test command: `cargo test --workspace` + `python tests/conformance/run.py` (black-box,
   drives the `ds` binary only).

Exact message shapes, paths, exit codes and limits: `docs/contracts/*` + `schemas/*`.
Component specs: `docs/components/*`. Threat model: `docs/20-security.md`.
Unverified items: `docs/open-questions.md`.
