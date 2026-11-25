# 2026-11-22 Change-flow review — independent adversarial audit

**Verdict**: NEEDS REVISION

An independent review of the internal change-coordination flow used during development
(a work-item board, per-change branches checked out in separate directories, change
records, and a mirror remote used to publish reviewed work). The tooling itself is not
part of this public repository; this page records what the review found, in plain terms.

---

## Findings

| Severity | ID | Claim | Why it matters | Required change |
|---|---|---|---|---|
| BLOCKER | R01 | A reviewed merge commit was not reachable on the internal mirror remote (`git ls-remote` showed the mirror behind local `main`) | The "merged and published" claim was false for that commit | Push and verify every merge; fail loudly on mismatch |
| BLOCKER | R02 | Changes could be self-approved — the review/merge path had no author check, so one person could both write and approve | Violates the second-pair-of-eyes requirement | Review/merge must be run by a different person than the author |
| BLOCKER | R03 | The local gate validated the main checkout, not the tree under review — `check.sh` always `cd`'d to its own root and ignored the change tree passed to it | Diffs were not validated against the content actually being merged; broken gate | Make the gate validate the tree it is invoked from, or an explicit tree argument |
| MAJOR | R04 | Scope patterns were matched literally, so a pattern like `src/*.rs` did not match `src/main.rs` | Files outside the declared scope could pass review | Use real glob/pattern matching for scope checks |
| MAJOR | R05 | The submit step could run from the main checkout and still pass its context check | A change record could be created with the wrong context | Verify the current directory is the change tree |
| MAJOR | R06 | Concurrent claims were not atomic: check-then-write allowed two claims to slip through | Two workers could take the same or overlapping work | Take an exclusive lock across check + write |
| MINOR | R07 | Template substitution broke when a value contained the `|` delimiter | Could corrupt a change record | Escape delimiter characters, or substitute safely |
| MINOR | R08 | Here-doc delimiters were fragile: a commit message containing the delimiter broke the heredoc | Malformed commit message | Use `git commit -F -` instead of a heredoc |
| MINOR | R09 | The list command parsed another command's text output instead of its JSON output | Fragile parsing; a format change would break it | Parse the JSON output |

---

## Checks run

| Check | Result |
|---|---|
| Board validation | OK — work items consistent, dependency graph valid, generated files in sync |
| Local gate | ALL GREEN |
| First change (DS-000) state | done |
| DS-000 change records | present |
| DS-000 branch | cleaned up |
| DS-000 checkout dir | cleaned up |
| Mirror remote sync | **MISMATCH** (mirror `main` behind local `main`) |
| DS-000 merge commit on mirror | **NOT FOUND** |
| Concurrent claim | correctly refused (one claimant wins) |
| Recovery / unclaim | behaved correctly |
| Shell syntax of all scripts | OK |

---

## Top 5 fixes before the next review round

1. **R01**: sync the mirror and verify the reviewed commit is reachable
2. **R02**: add a reviewer≠author gate on review/merge
3. **R03**: make `tools/check.sh` validate the tree it is invoked from (or the tree passed as an argument) — this fix survives in the public repo: `sh tools/check.sh [tree-root]`
4. **R04**: pattern-match scope declarations
5. **R05**: verify the current directory is the change tree before submitting

---

## Findings least certain about

1. **R06**: the claim path already used an exclusive lock; verified effective across concurrent processes.
2. **R09**: text parsing worked with the format at the time, but JSON parsing is more robust.
3. **R03**: the gate's tree was already script-relative; the gap was that it was implicit — now explicit, printed, and tested.

---

## Path

`docs/reviews/2026-11-22-workflow-review.md`

---

## Counts

BLOCKER: 3, MAJOR: 3, MINOR: 3 = 9 findings
