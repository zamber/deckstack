# Workflow hardening smoke — DS-090 (second end-to-end run)

Date: 2026-11-22. Ticket: DS-090. Purpose: prove the R01–R09 fixes from
`docs/reviews/2026-11-22-workflow-review.md` (response:
`docs/reviews/2026-11-22-workflow-response.md`) with a full second run of the
claim → worktree → commit → submit → steward review → merge loop, including the
verified push of `main` to the `review` remote.

## Commands run

```sh
sh tools/wt.sh new DS-090 --owner 'worker-smoke|R07&R08\CMT'   # claim + worktree (R08: nasty owner in the coordination commit message landed verbatim)
# in .worktrees/DS-090:
git add -A && git commit -m "ticket(DS-090): …" …               # body contains CMT/EOF lines on purpose
sh tools/pr.sh submit DS-090                                    # R05: pwd verified as .worktrees/DS-090; R07: owner with | & \ written verbatim into prs/DS-090.md
# steward, on main:
DS_STEWARD=1 sh tools/pr.sh list                                # R09: JSON-parsed open PRs
DS_STEWARD=1 sh tools/pr.sh review DS-090 --approve             # R02: steward-only + no self-approval; R03: check.sh run in the worktree; R04: check-touches gate
DS_STEWARD=1 sh tools/pr.sh merge DS-090                        # R01: pushes main to review and verifies the remote sha
```

## R01–R09 dispositions (evidence in the response doc and the tests it lists)

- R01 fixed: `pr.sh merge` pushes `main` to `review` and verifies `git ls-remote` sha, failing loudly.
- R02 fixed: `review`/`merge` require `DS_STEWARD=1` (exported by `tools/steward.sh`) and refuse reviewer == owner.
- R03 fixed: `check.sh [tree-root]` validates the invoked/passed tree; proven with a broken-marker worktree.
- R04 fixed: `tickets.py check-touches` (fnmatch + dir prefix + `new:`) gates `--approve`.
- R05 fixed: `submit` verifies it runs inside `<main>/.worktrees/<id>` on `ticket/<id>`.
- R06 proven: exclusive flock covers claim check+write (two concurrent claims: one wins, one fails).
- R07 fixed: `fill_line` is a safe Python replace (value via env); owner with `| & \` landed verbatim in `prs/DS-090.md`.
- R08 fixed: commit helpers use `git commit -F -`; the claim coordination commit contains `| & \ CMT` and landed intact.
- R09 fixed: `pr.sh list` parses `tickets.py list --state review --json true`.

## Result

Filled in by the merge: merge commit, ticket state, review record — see `prs/DS-090.md` and
`prs/DS-090.review.md` on main.
