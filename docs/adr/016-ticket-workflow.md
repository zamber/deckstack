# ADR-016 — Ticket/worktree/PR workflow governs all changes

Status: accepted (owner steering 2026-11-22, delta 9).

## Context

R10 makes agents the main developers; uncoordinated agent edits collide on contracts.

## Decision

All changes go through tickets: one worktree per ticket under `.worktrees/<id>` from
`main`, branch `ticket/<id>`, local bare review remote `.review.git`, PR record
`prs/<id>.md`, steward reviews + runs acceptance + merges. No worker commits to `main`.
File-ownership collisions are checked against open tickets before claiming; `schemas/` +
`docs/contracts/` are single-ticket-at-a-time. Protocol: `docs/AGENT-WORKFLOW.md`;
summary in `docs/50-repo-topology.md` §6 and `AGENTS.md`. Ticket tooling/scripts land in
the next run — the intended commands are documented now.

## Alternatives

- Direct commits to main by agents — rejected: contract collisions, no review gate.
- External issue tracker only — rejected for alpha: local `prs/` + review remote is enough and reviewable offline.

## References

`docs/AGENT-WORKFLOW.md`; `docs/50-repo-topology.md` §6; `AGENTS.md`; decision log D36.
