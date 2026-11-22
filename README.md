# deckstack

A ground-up, CLI-first replacement base for the MasterHub/HubDeck controller stack
(Cooler Master MasterHub + key/dial/fader/knob/roller modules). Governing intent:
[`BRIEF.md`](BRIEF.md) (requirements R1–R12). Evidence about the two existing
implementations: [`docs/discovery/`](docs/discovery/) (read-only dossier; path prefixes
`hubdeck/` = `~/repos/hubdeck`, `upstream/` = `~/repos/hubdeck-upstream`).

**Status: planning.** This repo contains documents, schemas and examples only — no
working software. Nothing here edits the two hub repos.

## How to read this repo

| You are… | Start at |
|---|---|
| A coder starting phase 1 | [PLAN.md](PLAN.md) → §Walking skeleton → [docs/60-roadmap.md](docs/60-roadmap.md) |
| Reviewing a load-bearing choice | [PLAN.md](PLAN.md) decision table → the ADR it names in [docs/adr/](docs/adr/) → [docs/decision-log.tsv](docs/decision-log.tsv) |
| Implementing a component | [docs/components/](docs/components/) (one file per component) |
| Implementing a wire format | [docs/contracts/](docs/contracts/) + [schemas/](schemas/) (the schemas are the contract) |
| Auditing safety | [docs/20-security.md](docs/20-security.md) |
| Wondering what is unverified | [docs/open-questions.md](docs/open-questions.md) |

Reading order for full context: `BRIEF.md` → `docs/discovery/10..50` → `PLAN.md` →
`docs/10-architecture.md` → contracts → components → ADRs.

## Repo topology (summary — details in docs/50-repo-topology.md)

```
~/repos/deckstack/            umbrella monorepo; components released per-crate via git subtree split
├── BRIEF.md                  governing intent (R1-R12) — do not edit
├── README.md                 this file (index)
├── PLAN.md                   one-page entry point
├── docs/
│   ├── discovery/            cited facts about hubdeck + hubdeck-upstream — do not edit
│   ├── decision-log.tsv      one row per material decision (id, decision, alternatives, why, evidence)
│   ├── open-questions.md     OPEN items + contradictions, each with a resolving experiment
│   ├── 10-architecture.md    process model, planes, ownership, recovery
│   ├── components/*.md       one doc per component
│   ├── contracts/*.md        state / ipc / cli / streaming / actions / plugins
│   ├── 20-security.md  30-testing.md  40-platforms.md
│   ├── 50-repo-topology.md  60-roadmap.md  70-migration.md
│   └── adr/NNN-*.md          architecture decision records (referenced from PLAN.md)
└── schemas/                  JSON Schemas: state.json, ipc-message.json, plugin-manifest.json, binding.json
```

Planned runtime layout (phase 1 onward, not present yet): crates `ds-protocol`,
`ds-device`, `ds-state`, `ds-ipc`, `ds-daemon`, `ds` (CLI), `ds-vdev`, plus sender
binaries `ds-focus`, `ds-mcp`, `ds-web`. See `docs/50-repo-topology.md`.

## Glossary

- **daemon (`ds-daemon`)** — the single long-lived process owning all runtime state and
  hardware I/O (R1).
- **control plane** — the NDJSON request/response + subscription channel (JSON state).
- **frame plane** — the binary stream channel carrying encoded frames to screens; never
  part of JSON state (R5).
- **state** — the single JSON document addressed by paths like `/controls/keys/0`
  (R2); every mutation bumps a global `rev`.
- **binding** — data at `/bindings/<control>/<gesture>` mapping an input event to a
  list of steps (R3/R4).
- **virtual device (`ds-vdev`)** — a software HID-level fake of the MasterHub used by CI
  (R10), modelled on `hubdeck/tests/fakes/virtual_hub.py`.
- **sender** — any ordinary external program that emits events into the daemon via the
  CLI/API (window watcher, cron, webhooks) (R4).
- **plugin** — a separate OS process declaring actions in a manifest, talking newline-JSON
  to the daemon's plugin host (R6).
- **capability token** — per-install token with scopes `read|mutate|admin`; per-plugin
  grants are a separate, smaller set (R8).
- Citations like `upstream/PROTOCOL.md:244` refer to `~/repos/hubdeck-upstream/PROTOCOL.md:244`.
