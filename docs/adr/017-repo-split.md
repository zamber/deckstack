# ADR-017 — Repo split: umbrella for the alpha core, separate publish targets

Status: accepted (owner steering 2026-11-22, delta 8).

## Context

R12: each component releasable as its own repo/app. Previous plan: subtree split for every
crate. Owner steering: keep the umbrella for the alpha core; make boundaries explicit.

## Decision

`repos.toml` is the machine-readable boundary list: `deckstack` (umbrella = alpha core),
`deckstack-schemas` (subtree split of `schemas/`), `deckstack-verify` (camera harness,
in-tree during alpha at `tools/verify/`), `deckstack-plugin-py` (in-tree `plugins-py/`,
published to PyPI), `deckstack-lights` (post-alpha, own repo from day one). Each entry:
name, path, status (alpha/post-alpha), development location, publish command, owner. No
separate repo is claimed to exist yet — entries say "in-tree" or "to create".

## Alternatives

- Split every crate now — rejected: phase churn needs atomic contract+code commits in one repo.
- Never split — rejected: R12 independent release; subtree split keeps it one command.

## References

`repos.toml`; `docs/50-repo-topology.md` §0/§4; decision log D37.
