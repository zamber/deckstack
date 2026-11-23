# deckstack

A ground-up, CLI-first replacement base for the MasterHub/HubDeck controller stack
(Cooler Master MasterHub + key/dial/fader/knob/roller modules). Governing intent:
[`BRIEF.md`](BRIEF.md) (requirements R1–R12 + Scope amendment 2026-11-22). **Scope is
normative in [`docs/00-scope.md`](docs/00-scope.md)**: alpha is Linux-only, starts from an
empty state (no presets/sample decks), has no migration/import and no web UI, and treats
agent steering as first-class. Evidence about the two existing implementations:
[`docs/discovery/`](docs/discovery/) (read-only; prefixes `hubdeck/` = `~/repos/hubdeck`,
`upstream/` = `~/repos/hubdeck-upstream`).

**Status: alpha scope, working software.** The daemon, CLI, virtual device, and
conformance suites exist and run (see "Run it"). Nothing here edits the two hub repos.

## Run it

Operator guide: **[docs/runbook.md](docs/runbook.md)** — install/build, first start on the
virtual device, real hardware (udev + `hid:` URI), systemd user unit, logs, status/health,
token handling, restart/upgrade, crash recovery, backup/restore, troubleshooting.

```sh
cargo build --release --workspace                       # target/release/{ds,ds-daemon,ds-vdev,ds-vdevctl}
python3 tests/e2e/run.py --no-bench                     # end-to-end demo: boots ds-vdev + ds-daemon
                                                        # on a temp dir and drives a key image through it
# or by hand:
target/release/ds-vdev --socket /tmp/vdev.sock &
target/release/ds daemon start --device virtual:/tmp/vdev.sock
target/release/ds daemon status                         # ok running pid=… rev=… device=…
target/release/ds set /controls/keys/0/image --json '{"kind":"color","value":"#3cc86e"}'
target/release/ds daemon stop
```

Current capabilities (CLI-first, no web UI / plugin UI in the alpha — out of scope by
`docs/00-scope.md`): `ds get|set|watch|schema|describe|status`, `ds bind|unbind`,
`ds daemon start|stop|status`, `ds stream stats|stop` (frame-plane push is wired in the
CLI but the daemon does not bind `stream.sock` yet); `ds token|emit|events|plugin` are
later-phase verbs, refused with exit 1 until their tickets land. Linux only; no
`ds migrate`; empty state at first start.

## Working on this repo (agents)

All changes go through the ticket board — see [AGENTS.md](AGENTS.md) and
[docs/AGENT-WORKFLOW.md](docs/AGENT-WORKFLOW.md). Quick start:

```sh
sh tools/bootstrap.sh                     # venv + requirements + sh tools/check.sh
sh tools/setup-review-remote.sh           # once: local bare remote .review.git as `review`
python3 tools/tickets.py ready            # claimable tickets (deps done)
sh tools/wt.sh new DS-XXX --owner <name>  # claim + worktree .worktrees/DS-XXX, branch ticket/DS-XXX
# edit only the ticket's touches; commit "ticket(DS-XXX): …" (+ Vibe trailer); then:
sh tools/pr.sh submit DS-XXX              # PR record prs/DS-XXX.md, pushed to `review`
# steward on main: sh tools/pr.sh review DS-XXX --approve && sh tools/pr.sh merge DS-XXX
sh tools/check.sh                         # the gate every PR must pass
```

The flow was proven end to end on DS-000 (`prs/DS-000.md`, `prs/DS-000.review.md`,
`docs/workflow-smoke.md`).

## How to read this repo

| You are… | Start at |
|---|---|
| An operator running the daemon | [docs/runbook.md](docs/runbook.md) (start/stop, logs, tokens, recovery, troubleshooting) |
| An agent picking up a ticket | [AGENTS.md](AGENTS.md) → [docs/AGENT-WORKFLOW.md](docs/AGENT-WORKFLOW.md) → your ticket |
| A coder starting phase 1 | [PLAN.md](PLAN.md) → §Walking skeleton → [docs/60-roadmap.md](docs/60-roadmap.md) |
| Checking what is in/out of alpha | [docs/00-scope.md](docs/00-scope.md) (one hop from here and from PLAN) |
| Reviewing a load-bearing choice | PLAN.md decision table → the ADR in [docs/adr/](docs/adr/) → [docs/decision-log.tsv](docs/decision-log.tsv) |
| Implementing a component | [docs/components/](docs/components/) |
| Implementing a wire format | [docs/contracts/](docs/contracts/) + [schemas/](schemas/) + [tests/fixtures/](tests/fixtures/) |
| Verifying on hardware | [docs/90-verification.md](docs/90-verification.md) (camera oracle) |
| Auditing safety | [docs/20-security.md](docs/20-security.md) |
| Wondering what is unverified | [docs/open-questions.md](docs/open-questions.md) |

Reading order for full context: `BRIEF.md` → `docs/00-scope.md` → `docs/discovery/10..50` →
`PLAN.md` → `docs/10-architecture.md` → contracts → components → ADRs.

## Repo topology (summary — details + `repos.toml` in docs/50-repo-topology.md)

```
~/repos/deckstack/            umbrella monorepo = the alpha core (contracts + crates + CLI + vdev + plugin host + conformance + docs)
├── BRIEF.md                  governing intent (R1-R12) + scope amendment — protected
├── AGENTS.md                 agent operational entry point
├── README.md  PLAN.md        this index / one-page plan
├── repos.toml                explicit repo boundaries (umbrella + separate publish targets)
├── skills/deckstack/SKILL.md agent orientation skill
├── docs/
│   ├── 00-scope.md           normative alpha scope + invariants — read first
│   ├── discovery/            cited facts about hubdeck + hubdeck-upstream — protected
│   ├── reviews/              adversarial review + response — protected
│   ├── AGENT-WORKFLOW.md     tickets/worktrees/PRs long protocol
│   ├── decision-log.tsv      one row per material decision
│   ├── open-questions.md     OPEN items + contradictions, each with a resolving experiment
│   ├── 10-architecture.md    process model, planes, ownership, recovery
│   ├── components/*.md       one doc per component (no web UI in alpha)
│   ├── contracts/*.md        state / ipc / cli / streaming / actions / plugins
│   ├── 20-security.md  30-testing.md  40-platforms.md (Linux alpha; Win/macOS TODO)
│   ├── 50-repo-topology.md  60-roadmap.md (alpha phases 0-6 + backlog)
│   ├── 70-porting-notes.md   post-alpha manual porting (no importer)
│   ├── 80-integrations.md    lights.lan as a plugin (post-alpha)
│   ├── 90-verification.md    camera verification harness (deckstack-verify)
│   └── adr/NNN-*.md          decision records (referenced from PLAN.md)
├── schemas/                  JSON Schemas: state, ipc-message, stream-frame, plugin-manifest, binding
├── tests/fixtures/           validate.sh/validate.js/validate.py + ipc-examples.json + regex-checks.js
└── packaging/                bootstrap.sh, release.sh, systemd unit, udev rules, NOTICE.md
```

Separate publish targets (none exists as a separate repo yet — `repos.toml` marks status):
`deckstack-schemas` (subtree split of `schemas/`), `deckstack-verify` (camera harness,
`tools/verify/`), `deckstack-plugin-py` (PyPI, `plugins-py/`), `deckstack-lights`
(post-alpha, own repo).

Planned runtime crates: `ds-protocol`, `ds-device`, `ds-state`, `ds-ipc`, `ds-daemon`,
`ds` (CLI), `ds-vdev` (+`ds-vdevctl`), `ds-focus`, `ds-mcp`, `ds-bench`, `ds-plugin-sdk`.
No `ds-web` in alpha.

## Glossary

- **daemon (`ds-daemon`)** — the single long-lived process owning all runtime state and
  hardware I/O (R1).
- **control plane / frame plane** — NDJSON JSON state channel vs binary DSF1 frame stream;
  frames never enter state or bump `rev`.
- **state** — the single JSON document addressed by paths like `/controls/keys/0` (R2).
- **binding** — an element of the `/bindings` array mapping a topic to steps (R3/R4).
- **virtual device (`ds-vdev`)** — HID-report-level fake of the MasterHub for CI (R10).
- **sender** — an external program emitting events via the CLI (window watcher, cron) (R4).
- **plugin** — a separate OS process with a manifest, talking newline-JSON to the daemon.
- **capability token** — per-install token with scopes `read|mutate|admin`; plugins get
  smaller capability grants (R8).
- **camera oracle** — webcam verification of pixels on the live deck (`docs/90-verification.md`).
- Citations like `upstream/PROTOCOL.md:244` refer to `~/repos/hubdeck-upstream/PROTOCOL.md:244`.
