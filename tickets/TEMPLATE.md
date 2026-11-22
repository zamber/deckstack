# TEMPLATE — copy to tickets/DS-NNN.md and fill in

The file MUST start with a fenced ```json metadata block (exactly one, first thing in the
file), then the body with all sections below as `## <Section>` headings. Only
`depends_on` is authored by hand; `blocks` and the graph are computed by
`python3 tools/tickets.py sync`.

```json
{
  "id": "DS-NNN",
  "title": "short imperative title",
  "state": "backlog",
  "phase": 0,
  "lane": "workflow",
  "size": "M",
  "owner": null,
  "depends_on": [],
  "touches": ["new:path/that/does/not/exist/yet", "existing/path/or-glob"],
  "branch": null,
  "worktree": null,
  "pr": null,
  "created": "YYYY-MM-DD",
  "updated": "YYYY-MM-DD",
  "acceptance": ["command that must succeed and the observable result"],
  "verification": ["python3 tools/tickets.py validate", "sh tests/fixtures/validate.sh"]
}
```

Field rules (enforced by `python3 tools/tickets.py validate`):

- `id`: `DS-` + three digits, must match the file name.
- `state`: backlog|ready|doing|review|done|blocked. ready/doing/review/done require all
  `depends_on` to be `done`. doing/review require `owner`. blocked requires
  `blocked_reason`.
- `phase`: integer 0..7 (0 workflow, 1-6 alpha phases, 7 post-alpha).
- `lane`: free-form string grouping parallel work (e.g. workflow, build, ipc, device).
- `size`: S|M|L (see tickets/README.md).
- `touches`: non-empty list; each entry must exist in the repo, be a glob with an
  existing base dir, have an existing parent dir, or carry the `new:` prefix for a path
  you will create. Two `doing` tickets may not overlap.
- `branch`/`worktree`/`pr`: null until claimed; claim sets `ticket/<id>`,
  `.worktrees/<id>`, `prs/<id>.md`.
- `created`/`updated`: ISO date. `acceptance`/`verification`: lists of commands.

## Context

Why this ticket exists; which doc/ADR/phase it belongs to; what is true today.

## Goal

One or two sentences: the observable end state when this ticket is done.

## Scope

What is in: exact deliverables, files, commands.

## Out of scope

What is explicitly NOT in this ticket (point at the ticket that owns it).

## Implementation notes

How to build it: read order, key docs/contracts, approach, pitfalls already known.

## Files and boundaries

The `touches` list restated as prose; which files you may edit, which you must not
(protected: BRIEF.md R1-R12, docs/discovery/, docs/reviews/).

## Acceptance

Commands that must pass and the observable result (mirror the metadata `acceptance`).

## Verification

The full gate: `sh tests/fixtures/validate.sh` → ALL CHECKS PASSED, `cargo test
--workspace`, `python tests/conformance/run.py`, `python3 tools/tickets.py validate`,
plus ticket-specific checks (hardware: `DS_HARDWARE=1 python tests/hw/smoke.py`).

## Risks

What can go wrong and how to detect/mitigate it.

## Definition of done

PR merged to main, ticket state `done`, acceptance green, docs/schemas updated in the
same commit, stale-grep clean for the area.

## Notes

Anything else a fresh worker needs (links, decisions, evidence, open questions).
