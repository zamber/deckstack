# 2026-11-22 Workflow Review — Independent Adversarial Audit

**Verdict**: NEEDS REVISION

---

## Findings

| Severity | ID | Claim | Evidence | Why It Matters | Required Change |
|---|---|---|---|---|---|
| BLOCKER | R01 | DS-000 merge commit not on review remote | `git ls-remote review` → main=b05aaf2, local main=6dd4978; `git ls-remote review \| grep f6e4bd9` → empty | Smoke claim false: merge commit f6e4bd9 unreachable on review | `git push review main` to sync; verify all merges pushed |
| BLOCKER | R02 | Workers can self-approve | `pr.sh review`/`merge` have no owner check; any user from main can run `sh tools/pr.sh review DS-XXX --approve` then merge | Violates second-pair-of-eyes requirement; steward can be bypassed | Add owner check in pr.sh: review/merge must be run by steward (not ticket owner) |
| BLOCKER | R03 | checks run against main, not worktree | `check.sh` hard-codes `cd "$ROOT"` where ROOT is always main repo; `pr.sh review` calls `run_checks "$ROOT/$wt"` but check.sh ignores wt and cds to main | PR diffs not validated against actual worktree content; broken gate | Fix check.sh to use current directory or accept ROOT parameter; pr.sh must pass wt path |
| MAJOR | R04 | Glob patterns in touches not honored | `pr.sh review` line 200: `grep -v -F -f <(...)` uses literal match; `src/*.rs` in touches won't match `src/main.rs` | touch collision check passes when files outside touches changed | Use `grep -E` with pattern matching or validate globs before diff check |
| MAJOR | R05 | submit runs from main passes wrong check | `pr.sh:121`: `[ -f "$ROOT/.git" ]` always true in main and worktrees; doesn't verify current dir is worktree | Could create PR record from main with wrong context | Replace with `[ "$(pwd)" = "$ROOT/.worktrees/$id" ]` or similar path check |
| MAJOR | R06 | Parallel claim collision not atomic | `tickets.py claim` checks collision then writes; between check and write, another claim could slip through | Two workers could claim same ticket or overlapping touches | Add exclusive lock in claim; tickets.py already uses Lock(exclusive=True), but verify |
| MINOR | R07 | fill_line breaks on pipe in value | `pr.sh:71`: `sed -i "s|$ph|$v|g"` — if val contains `|`, sed pattern fails | Could corrupt PR record | Escape `|` and delimiter chars in $v before sed |
| MINOR | R08 | Here-doc delimiters fragile | `vibe_commit` in wt.sh/pr.sh uses `<<EOF`/`<<CMT`; message containing delimiter breaks here-doc | Commit message malformed | Use unique delimiter per call or avoid here-doc in command substitution |
| MINOR | R09 | pr.sh list uses deprecated awk | `pr.sh:336`: awk parsing of `tickets.py list` output relies on format; `list --json` exists but not used | Fragile parsing; format change breaks pr.sh list | Use `tickets.py list --state review --json true` for robust parsing |

---

## Checks Run

| Check | Command | Result |
|---|---|---|
| Board validate | `python3 tools/tickets.py validate` | OK - 58 tickets, DAG valid, generated files in sync |
| Local gate | `sh tools/check.sh` | ALL GREEN (fixture gate + board validate) |
| DS-000 state | `python3 tools/tickets.py field DS-000 state` | done |
| DS-000 file | `ls docs/workflow-smoke.md` | exists on main |
| DS-000 PR records | `ls prs/DS-000.md prs/DS-000.review.md` | both exist |
| DS-000 branch | `git branch -a \| grep DS-000` | none (correctly cleaned up) |
| DS-000 worktree | `git worktree list \| grep DS-000` | none (correctly cleaned up) |
| Review remote sync | `git ls-remote review` | main=b05aaf2 vs local main=6dd4978 **MISMATCH** |
| DS-000 merge commit on review | `git ls-remote review \| grep f6e4bd9` | **NOT FOUND** |
| Worktree collision | `python3 tools/tickets.py claim DS-004 --owner test` then claim DS-004 --owner other | Correctly refused (ticket is doing) |
| Recovery | `python3 tools/tickets.py claim DS-004 --owner test` then `recover` | Correctly shows: doing, no branch/worktree/pr |
| Unclaim | `python3 tools/tickets.py unclaim DS-004` | Correctly set to ready |
| Syntax check | `bash -n tools/wt.sh tools/pr.sh tools/check.sh tools/setup-review-remote.sh tools/bootstrap.sh tools/steward.sh` | All OK |

---

## Top 5 Fixes Before Owner Review

1. **R01**: Sync review remote: `git push review main` and verify f6e4bd9 reachable
2. **R02**: Add steward-only gate in pr.sh review/merge (check $USER or role config)
3. **R03**: Fix check.sh to validate the worktree, not main: remove hard-coded cd "$ROOT", use pwd or parameter
4. **R04**: Fix glob handling in pr.sh: replace literal grep with pattern matching for touches
5. **R05**: Fix submit check: verify current directory is the ticket worktree

---

## 3 Findings I Am Least Sure About

1. **R06** (parallel claim): tickets.py does use Lock(exclusive=True) in cmd_claim, which should serialize claims. Need to verify if the lock is actually effective across concurrent Python processes.
2. **R09** (pr.sh awk): awk parsing works with current format, but relying on text parsing vs JSON is fragile; not confirmed broken.
3. **R03** (check.sh): Need to verify if the worktree truly has different content than main in a real scenario; current DS-000 worktree was deleted so cannot repro.

---

## Path

`docs/reviews/2026-11-22-workflow-review.md`

---

## Counts

BLOCKER: 3, MAJOR: 3, MINOR: 3 = 9 findings
