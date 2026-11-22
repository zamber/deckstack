---
name: deckstack
description: Orient in the deckstack repo (CLI-first deck daemon plan + alpha build) and work a ticket per AGENTS.md / docs/AGENT-WORKFLOW.md.
---

# deckstack

Start at `AGENTS.md`, then `docs/00-scope.md` (alpha scope invariants), `PLAN.md`, and
`docs/contracts/` + `schemas/` (normative contracts). Read order, invariants, commands,
worktree/PR protocol, definition of done: `AGENTS.md` and `docs/AGENT-WORKFLOW.md`.

Rules of thumb: Linux-only alpha; empty state, no presets; no `ds migrate`, no web UI;
contracts change together with schemas (`sh tests/fixtures/validate.sh` must print ALL
CHECKS PASSED); never commit to `main` — one worktree per ticket under `.worktrees/<id>`,
branch `ticket/<id>`, PR record `prs/<id>.md`; never edit `BRIEF.md` R1–R12,
`docs/discovery/`, `docs/reviews/`.
