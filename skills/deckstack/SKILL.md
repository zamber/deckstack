---
name: deckstack
description: Orient in the deckstack repository and change it without breaking the wire contracts.
---

# deckstack

Start at `AGENTS.md`, then `docs/00-scope.md` (scope invariants), `PLAN.md`, and
`docs/contracts/` with `schemas/` (normative wire formats).

## Ground rules

- Linux-only alpha. Empty state at first start: no presets, no sample decks, no importer.
- Contracts are normative. Change `docs/contracts/*`, `schemas/*` and `tests/fixtures/*`
  together. `sh tests/fixtures/validate.sh` must print `ALL CHECKS PASSED`.
- The gate is `sh tools/check.sh`: fixture gate, cargo fmt/clippy/test, conformance suite.
- `BRIEF.md` R1-R12, `docs/discovery/` and `docs/reviews/` record the evidence and the
  boundaries set at the start. Add a decision instead of rewriting that history.

## Where things are

| Need | Read |
|---|---|
| Process model, planes, ownership | `docs/10-architecture.md` |
| State paths, bindings, revisions | `docs/contracts/state.md` |
| CLI grammar and exit codes | `docs/contracts/cli.md` |
| Socket protocol | `docs/contracts/ipc.md` |
| Frame plane | `docs/contracts/streaming.md` |
| Plugins and capabilities | `docs/contracts/plugins.md` |
| Decisions | `docs/adr/` and `docs/decision-log.tsv` |
