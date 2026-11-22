# PLAN — deckstack in one page

Status: **planning, alpha scope** (`docs/00-scope.md` is normative; BRIEF.md R1–R12 +
2026-11-22 amendment). Goal: `BRIEF.md`.

## The shape

One long-lived Rust daemon (`ds-daemon`) owns all state and hardware on **Linux**
(Bazzite/KDE/Wayland first). A stateless Rust CLI (`ds`) does one read/mutate per
invocation over a Unix socket; the same API is reachable via a generic WS/HTTP adapter for
agents/debug clients (no UI in alpha). State is one JSON document, path-addressed,
revision-stamped, starting **empty** — no presets or sample decks. Frames travel on a
separate binary stream (latest-frame-wins, bounded queues, dedicated writer threads) and
never enter JSON state or bump `rev`. Bindings are data in a `/bindings` array; the core
action vocabulary is `set|emit|plugin|exec|sleep` with exec argv-only and
allowlist−denylist enforced. Plugins are separate processes with manifests, JSON-Schema
params, capability grants, rlimits; a Python wrapper keeps the plugin ecosystem usable.
The device layer reuses `mh-protocol`/`mh-device` as dependencies. Camera verification of
the live deck is part of the plan (`docs/90-verification.md`).

```
external senders (ds-focus, cron, webhooks, agents)
        │  ds emit / API          ┌──────────────┐   HID (mh-device)   ┌─────────┐
        ▼                         │  ds-daemon   │◄───────────────────►│ MasterHub│
   ds CLI ── NDJSON ─────────────►│ state (JSON) │   frame plane (bin) │ modules  │
   agents ── WS/HTTP adapter ───►│ router+bindings│───────────────────►│ (screens)│
   (debug clients only)          │ plugin host  │  newline-JSON stdio │ plugins │
                                   └──────────────┘◄────────────────────┘─────────┘
```

## Load-bearing decisions (full trail: docs/decision-log.tsv; ADRs in docs/adr/)

| # | Decision | ADR |
|---|---|---|
| 1 | Daemon in Rust, one binary; reuses mh-device/hidapi | [ADR-001](docs/adr/001-daemon-language.md), [ADR-010](docs/adr/010-device-layer-reuse.md) |
| 2 | CLI in Rust sharing the daemon's client crate; one-shot + `watch`; self-describing (`schema`/`describe`/`--help`) | [ADR-002](docs/adr/002-cli-language.md) |
| 3 | IPC: Unix socket, NDJSON + binary DSF1 stream; WS/HTTP as generic agent adapter; scoped token | [ADR-003](docs/adr/003-ipc-transport.md) |
| 4 | State: single JSON document, path addressing, revisions, transactions, subscriptions, `/bindings` array | [ADR-004](docs/adr/004-state-model.md) |
| 5 | Frames are bytes on a separate stream; latest-frame-wins 1+1; no per-frame rev | [ADR-005](docs/adr/005-frame-transport.md) |
| 6 | Plugins: process-per-plugin, manifest, capability grants, rlimits+Landlock; Python wrapper | [ADR-006](docs/adr/006-plugin-isolation.md) |
| 7 | Actions: bindings-as-data, core vocabulary set/emit/plugin/exec/sleep, no eval, no shell, no catalog | [ADR-007](docs/adr/007-action-model.md) |
| 8 | Repo: umbrella monorepo for alpha core + `repos.toml` boundaries + `git subtree split` publishing | [ADR-008](docs/adr/008-repo-topology.md), [ADR-017](docs/adr/017-repo-split.md) |
| 9 | Security: local-only, scoped tokens, caps, image limits, no RCE surface by default | [ADR-009](docs/adr/009-security-model.md) |
| 10 | mh-protocol/mh-device reused as git deps, dual license + NOTICE | [ADR-010](docs/adr/010-device-layer-reuse.md) |
| 11 | Alpha is Linux-only; Win/macOS are design TODOs, not deliverables | [ADR-011](docs/adr/011-linux-alpha-only.md) |
| 12 | No import/migration/compat with existing apps; ticket-driven porting later | [ADR-012](docs/adr/012-no-import.md) |
| 13 | Empty state at start; no presets/layouts/sample decks shipped | [ADR-013](docs/adr/013-empty-state.md) |
| 14 | No web UI in alpha; WS/HTTP adapter is for agents/debug clients only | [ADR-014](docs/adr/014-no-web-ui.md) |
| 15 | Camera oracle verifies pixels on hardware; vdev in CI | [ADR-015](docs/adr/015-camera-oracle.md) |
| 16 | Ticket/worktree/PR workflow governs all changes | [ADR-016](docs/adr/016-ticket-workflow.md) |

## Phase table (alpha 0–6; details + exit criteria: docs/60-roadmap.md)

| Phase | Slice | Observable exit criterion |
|---|---|---|
| 0 | Contracts freeze + workspace + ticket/workflow scaffolding | `sh tests/fixtures/validate.sh` → ALL CHECKS PASSED; CI skeleton green |
| 1 | **Walking skeleton on the virtual device**: daemon + CLI + socket + state + one key image via ds-vdev; empty-state invariant | `ds set /controls/keys/0/image --file k.png` → golden sha256 via `ds-vdevctl frames key:0`; fresh start has empty `/controls` |
| 2 | Linux hardware bring-up + **camera verification** | hardware smoke green incl. `deckstack-verify check` on ≥1 key + panel; replug ≤ 5 s; handback on SIGTERM |
| 3 | Streaming: frame plane, backpressure, telemetry, benchmarks | 32 fps push sustained, rev delta 0, query p99 ≤ 10 ms, drops counted |
| 4 | Events/bindings/actions | key→plugin→visual chain and focus→recolor chain pass on vdev; `/bin/sh -c` exec refused |
| 5 | Plugin host + Python wrapper | example plugin runs isolated; ds-plugin-py published to PyPI |
| 6 | Senders + agent surfaces: ds-focus (KWin/wlr), ds-mcp, self-describing CLI | focus change recolours a key ≤ 500 ms on Bazzite; MCP `tools/list` + `deck_set` works |

**Post-alpha backlog** (documented, not scheduled): lights.lan plugin
(`docs/80-integrations.md`), Windows, macOS (`docs/40-platforms.md` TODOs), web/debug UI
over the adapter, selective porting (`docs/70-porting-notes.md`).

## First walking skeleton (phase 1 — a coder starts here)

1. `git clone` this repo; `sh packaging/bootstrap.sh` (cargo build --workspace + fixture
   gate; crates: ds-protocol, ds-device over mh-protocol/mh-device git deps, ds-state,
   ds-ipc, ds-daemon, ds, ds-vdev).
2. `sh tests/fixtures/validate.sh` → ALL CHECKS PASSED.
3. `ds-vdev --socket /tmp/vdev.sock &` — report-level virtual MasterHub.
4. `ds daemon start --device virtual:/tmp/vdev.sock` — binds
   `$XDG_RUNTIME_DIR/deckstack/daemon.sock`, writes 0600 token, starts from **empty state**.
5. `ds set /controls/keys/0/color '#3cc86e'` → `ds get /controls/keys/0 --json` round-trips.
6. `ds set /controls/keys/0/image --file examples/key.png` → `ds-vdevctl --socket
   /tmp/vdev.sock frames key:0` prints the sha256; conformance asserts it.
7. Tests: `cargo test --workspace` + `python tests/conformance/run.py` + validate.sh.

Exact message shapes, paths, exit codes, limits: `docs/contracts/*` + `schemas/*` + the
example corpus `tests/fixtures/ipc-examples.json`. Component specs: `docs/components/*`.
Threat model: `docs/20-security.md`. Alpha scope: `docs/00-scope.md`. Agent protocol:
`AGENTS.md` + `docs/AGENT-WORKFLOW.md`. Unverified items: `docs/open-questions.md`.
Review disposition: `docs/reviews/2026-11-22-response.md`.
