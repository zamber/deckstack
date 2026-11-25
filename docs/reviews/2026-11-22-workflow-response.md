# 2026-11-22 Change-flow review — response

Response to `docs/reviews/2026-11-22-workflow-review.md` (findings R01–R09). The fixes
landed in the internal change-coordination tooling during development (commit `da401d5`);
that tooling is not part of this public repository. The one fix with lasting public
relevance is R03: `tools/check.sh` validates the tree it is invoked from, or the explicit
tree passed as an argument (`sh tools/check.sh [tree-root]`, printing
`validating tree: …`).

| ID | Disposition | Fix | Proof |
|---|---|---|---|
| R01 | fixed | push + `git ls-remote` sha verification on every merge; fails loudly on failure; mirror synced | mirror `main` equals local `main`; `git merge-base --is-ancestor f6e4bd9` OK; re-verified on the DS-090 merge |
| R02 | fixed | review/merge require a different person than the author; self-approval refused with a clear error | self-approval attempt → rc=1 "self-approval is forbidden" |
| R03 | fixed | `tools/check.sh` takes an optional tree-root argument and validates that tree (kept in the public repo) | a deliberately broken tree FAILs when passed explicitly; the main tree run stays ALL GREEN |
| R04 | fixed | scope checks use real glob matching (fnmatch, directory-prefix rules, `new:` paths) | `src/*.rs` matched `src/main.rs`; out-of-scope `README.md` reported with rc=1 |
| R05 | fixed | submit resolves the common git dir and requires the current directory to be the change tree on its branch | run from main → rc=1 with a clear message; positive case proven on DS-090 |
| R06 | fixed (already atomic; proven) | the claim path holds an exclusive `flock` across check + write + sync — no code change needed | two concurrent claims: one rc=0, one rc=1; exactly one owner recorded |
| R07 | fixed | template substitution uses a safe string replace with the value passed via the environment (safe for `|`, `&`, `\`, newlines) | a value containing all of those landed verbatim in the change record |
| R08 | fixed | commits use `git commit -F -` (no heredoc delimiter); the merge message is built in a variable and passed with `git merge -m` (fixed in `bc6ff8b`) | messages containing delimiters landed intact; merge commit `fa37db3` carries the generated message |
| R09 | fixed | the list command parses the board's JSON output with `python3 json` | exercised during the DS-090 end-to-end run |

## Notes

- The review's R03 mechanism ("the gate always cds to main") did not reproduce as stated —
  the tree was already script-relative — but it was only implicit; it is now explicit,
  printed, and tested (see R03).
- R06 needed no code change: the lock already covered check + write; the concurrent-claim
  test proves it.
- Reviewer≠author enforcement plus a hand-off error meant a worker following the docs could
  not approve its own change.

## Post-review fixes found by the end-to-end run

The first DS-090 merge exposed two script bugs the review could not see; both were fixed
on `main`:

- the gate call now returns the gate status (`2871137`) and declares its locals (`81339f8`);
  without them it clobbered the caller's variables and merge cleanup looked in the wrong
  path. Review and merge now abort on a failed gate and leave the change tree intact.
- `git merge` has no `-F`; the merge message is built in a variable and passed with `-m`
  (`bc6ff8b`).

DS-090 proof: merge commit `fa37db3`, work item `done`. The local gate reported ALL GREEN
and the tree was clean afterwards.

## Path

`docs/reviews/2026-11-22-workflow-response.md`
