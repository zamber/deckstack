# ADR-013 — Empty state at start; no presets/layouts/sample decks

Status: accepted (owner steering 2026-11-22, delta 3).

## Context

Both existing apps ship opinionated presets/profiles (upstream `engine/presets.rs`;
hubdeck `presets:` in config). The brief's unix-philosophy test says behaviour is composed
by the user/agent, not pre-baked.

## Decision

The daemon starts from an empty state document; nothing in the repo ships a preset,
layout, profile, sample deck or default fixtures (tests keep theirs under `tests/`, never
installed). Invariant lives in `docs/00-scope.md` and `docs/10-architecture.md`; conformance
asserts a fresh start yields empty `/controls`, `/bindings`, `/plugins`. Behaviour is built
from the CLI/API (`docs/contracts/cli.md`).

## Alternatives

- Ship a minimal default page — rejected: any shipped default is an opinionated layout and invites "edit the default" instead of composing.
- Examples directory — allowed only under `examples/` clearly marked non-default and never loaded automatically.

## References

BRIEF.md amendment; `docs/00-scope.md` invariant 1; decision log D33.
