# Workflow smoke evidence (DS-000)

This file is the evidence that the local ticket → worktree → PR → review → merge
flow works end to end. It is intentionally content-free apart from this record.

- Ticket: DS-000 (claimed by `autolith/run-4004342966`)
- Branch: `ticket/DS-000`, worktree `.worktrees/DS-000`, created from `main` by
  `sh tools/wt.sh new DS-000 --owner autolith/run-4004342966`
- Commit: `ticket(DS-000): add workflow smoke doc`
- PR record: `prs/DS-000.md` (submitted with `sh tools/pr.sh submit DS-000`),
  steward verdict in `prs/DS-000.review.md` (`sh tools/pr.sh review DS-000 --approve`),
  merged with `sh tools/pr.sh merge DS-000` (merge commit recorded in `prs/DS-000.md`)
- Gate at every step: `sh tools/check.sh` (fixture gate ALL CHECKS PASSED +
  `python3 tools/tickets.py validate` OK)

Date: 2026-11-22. Run: run-4004342966-95265.
