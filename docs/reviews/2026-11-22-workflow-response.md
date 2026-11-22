# 2026-11-22 Workflow Review — Response

Response to `docs/reviews/2026-11-22-workflow-review.md` (findings R01–R09). All fixes land
in the tool that owns the behavior: `tools/tickets.py` owns touch semantics, `tools/pr.sh` owns
review/merge, `tools/wt.sh` owns worktrees, `tools/steward.sh` launches the steward with
`DS_STEWARD=1`. Tool fixes are committed on `main` as `da401d5`; the second end-to-end smoke
run is ticket DS-090 (`prs/DS-090.md`, `prs/DS-090.review.md`, artifact
`docs/workflow-hardening-smoke.md`).

| ID | Disposition | Fix (files) | Test / proof |
|---|---|---|---|
| R01 | fixed | `tools/pr.sh` (`push_main_to_review`: push + `git ls-remote` sha verification, dies loudly on failure), `tools/setup-review-remote.sh` used to sync; main pushed to `review` | `git ls-remote review` = local `main`; `git merge-base --is-ancestor f6e4bd9 review/main` OK; DS-090 merge re-verified the push |
| R02 | fixed | `tools/pr.sh` (`require_steward` on review/merge: requires `DS_STEWARD=1`, refuses reviewer == ticket owner, error tells workers to hand off to `tools/steward.sh`); `tools/steward.sh` exports `DS_STEWARD=1`; `agents/deckstack-steward.md`, `docs/AGENT-WORKFLOW.md`, `AGENTS.md` document it | without `DS_STEWARD=1`: rc=1 hand-off error; with `DS_STEWARD=1` and owner==reviewer: rc=1 "self-approval is forbidden"; DS-090 review/merge ran with `DS_STEWARD=1` |
| R03 | fixed | `tools/check.sh` validates the tree it is invoked from, or the explicit tree argument (`sh tools/check.sh [tree-root]`, prints `validating tree: …`); `tools/pr.sh run_checks`/merge pass the worktree path | temp worktree with `MARKER-BROKEN` in its `tickets/BOARD.md`: `sh <wt>/tools/check.sh <wt>` from main FAILs on the worktree; `sh tools/check.sh` in main ALL GREEN; no-arg run inside the worktree validates the worktree; probe removed afterwards |
| R04 | fixed | `tools/tickets.py check-touches <id> --diff <base>..<head>` (fnmatch globs, directory-prefix rules, `new:` strips to its declared path); `tools/pr.sh review` calls it and refuses `--approve` on violations | `src/*.rs` matched `src/main.rs`; `src/` matched `src/nested/deep/x.rs`; `new:docs/new-thing.md` matched; `README.md` reported OUTSIDE with rc=1; `..` treated as `...` |
| R05 | fixed | `tools/pr.sh submit` resolves `git rev-parse --git-common-dir`, requires `pwd == <main>/.worktrees/<id>` plus branch `ticket/<id>` | from main: rc=1 "submit must run inside …/.worktrees/DS-004", board untouched; positive case proven by DS-090 submit inside `.worktrees/DS-090` |
| R06 | fixed (already atomic; proven) | `tools/tickets.py cmd_claim` holds `Lock(exclusive=True)` (flock on `tickets/.lock`) across the collision check, write and sync — no code change needed | two concurrent `claim DS-902` attempts: one rc=0, the other rc=1 ("is doing, must be ready to claim"); ticket file has exactly one `owner`, state `doing`, `tickets.py validate` OK |
| R07 | fixed | `tools/pr.sh fill_line` replaced sed substitution with a Python `str.replace`, value passed via environment (safe for `|`, `&`, `\`, newlines) | DS-090 owner `worker-smoke\|R07&R08\CMT` flows through `fill_line {{OWNER}}` into `prs/DS-090.md` verbatim; review `--request-changes` reason containing a newline + `|`/`&`/`\` written verbatim into `prs/DS-090.review.md` |
| R08 | fixed | `tools/pr.sh` and `tools/wt.sh` `vibe_commit` and the merge message now pipe via `git commit -F -` / `git merge -F -` (no here-doc delimiter) | DS-090 claim coordination commit message contains `| & \ CMT` (via the owner) and lands intact; worker commit body contains `CMT`/`EOF` lines; DS-090 merge message built with `printf … | git merge -F -` |
| R09 | fixed | `tools/pr.sh list` uses `tickets.py list --state review --json true` parsed with `python3 json` | with DS-902 in `review`: `pr.sh list` printed the DS-902 row (owner/branch/verdict); also exercised during the DS-090 smoke |

## Notes

- The review's R03 mechanism ("check.sh always cds to main") did not reproduce as stated —
  `ROOT` was already `$0`-based — but the tree was only implicit; it is now explicit, printed,
  and tested (see R03 row).
- R06 needed no code change: the lock already covers check+write; the concurrent-claim test
  proves it.
- Steward-only enforcement is `DS_STEWARD=1` (declaration exported by `tools/steward.sh`) plus
  the reviewer≠owner check; a worker following the docs cannot approve its own PR and is told
  to hand off to the steward.

## Path

`docs/reviews/2026-11-22-workflow-response.md`
