# prs/ — durable PR and review records

Every ticket PR leaves a committed record here. `prs/` is **not** gitignored
(`.worktrees/` and `.review.git/` are; see `.gitignore`).

| File | Written by | When | Content |
|---|---|---|---|
| `prs/<id>.md` | `sh tools/pr.sh submit <id>` (in the ticket worktree, committed on `ticket/<id>`) | PR submit | PR record from `prs/TEMPLATE.md`: ticket, owner, base/head, commits, diffstat, checks + results, acceptance, risks. After merge, `pr.sh merge` adds the verdict + merge commit. |
| `prs/<id>.review.md` | `sh tools/pr.sh review <id> [--approve|--request-changes REASON]` (on main, coordination commit) | steward review | verdict (approved / changes-requested + reason), reviewer, checks run in the worktree, findings (protected files, touches, diffstat). |
| `TEMPLATE.md` | — | — | the template both commands fill (placeholders `{{…}}`). |
| `README.md` | — | — | this file. |

Flow: `sh tools/pr.sh submit DS-XXX` → steward `sh tools/pr.sh review DS-XXX --approve`
→ `sh tools/pr.sh merge DS-XXX` → `prs/DS-XXX.md` on main carries the approved
verdict and merge commit; `prs/DS-XXX.review.md` carries the review detail.
`sh tools/pr.sh list` shows open PRs with verdict and ahead/behind.
