# ADR-008 — Repo topology: umbrella monorepo + git subtree split publishing

Status: accepted (planning).

## Context

R12 + tension 7: the project lives in `<deckstack>`; each component must be
releasable as its own repo/app; clean versioned contracts between components; no submodule
hand-waving — pick one and show the commands.

## Decision

Umbrella monorepo, cargo workspace, components as crates/bins under `crates/` and `bins/`
(docs/50-repo-topology.md §1). Contracts versioned in `schemas/` (bumped in the same commit
as the code that speaks them; wire `protocol: N`). Publishing per component via
`git subtree split --prefix=<dir>` pushed to `github.com/deckstack/<component>` — exact
commands in docs/50-repo-topology.md §4, driven by tags (`ds-daemon-vX.Y.Z`). Bootstrap:
`git clone && cargo build --workspace && python tests/conformance/run.py` (§5).

## Alternatives

- **Git submodules** — rejected: pointer drift, clone footguns, contract edits split across
  repos during phase 1.
- **Sibling repos from day one** — rejected: phase-1 contract churn across PR stacks;
  revisit after phase 4 when contracts stabilize (subtree split makes extraction trivial).
- **One crate** — rejected: violates independent release (R12).

## Consequences

One PR can change schema + impl atomically; CI tests both the workspace and
`cargo publish --dry-run` per crate. Negative: umbrella history grows; subtree split
rewrites it per component (by design).

## References

BRIEF.md R12; docs/50-repo-topology.md; decision log D10.
