# ADR-014 — No web UI in alpha; WS/HTTP adapter is a generic agent/debug transport

Status: accepted (owner steering 2026-11-22, delta 4).

## Context

R1: no UI required for any capability; a GUI is only another client. Previous plan had a
ds-web UI phase.

## Decision

No UI deliverable in alpha, and none planned beyond a post-alpha note. `ds-web` is removed
from components, bins, phases and the architecture diagram. The opt-in WS/HTTP adapter
stays as a generic transport for agents/debug clients (`/api/*`, `/ws`; `/` → 404), with
bearer + Host checks (T7). Post-alpha note: a thin debug/web client could sit on the
adapter later; nothing opinionated is planned.

## Alternatives

- Thin UI in alpha — rejected by owner steering; CLI self-description (`ds schema`,
  `ds describe`, `--help`, stable exit codes) covers agent discoverability (R10).

## References

BRIEF.md amendment; `docs/00-scope.md` invariant 3; `docs/contracts/ipc.md` §1.1;
`docs/AGENT-WORKFLOW.md` §CLI self-description; decision log D34.
