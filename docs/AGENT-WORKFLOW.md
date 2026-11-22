# AGENT-WORKFLOW — tickets, worktrees, PRs, roles

Long form of `AGENTS.md` §Work protocol. The ticket tooling and scripts land in the next
run; this document fixes the protocol they must implement. Intended commands are shown so
the tooling has a spec.

## Roles

- **Owner/steward** (human or supervising agent): triages tickets, assigns, reviews PRs,
  runs acceptance, merges. Only role that merges to `main`.
- **Worker** (coding agent): claims one ticket, works in one worktree, opens one PR.
  Never commits to `main`, never force-pushes, never merges.

## Ticket states

`backlog → claimed → in-progress → in-review → merged → done` (`rejected`/`blocked` with a
reason). One ticket = one PR = one worktree. Tickets carry: id, title, scope (files it may
touch — the ownership list), acceptance (commands + observable result), phase.

## Worktree creation (intended commands)

```sh
git worktree add .worktrees/<id> -b ticket/<id> main     # or: git worktree add --detach + branch
# local bare review remote (once per repo):
git init --bare .review.git
git remote add review .review.git
# work, commit (heredoc format with Mistral Vibe co-author trailer), then:
git push review ticket/<id>
# PR record: prs/<id>.md  {ticket, branch, summary, files, acceptance output, reviewer, state}
```

## PR submit / review / merge

1. Worker pushes `ticket/<id>` to the `review` remote and writes `prs/<id>.md`.
2. Steward checks out the branch in a scratch worktree, runs acceptance:
   `sh tests/fixtures/validate.sh && cargo test --workspace && python tests/conformance/run.py`
   (+ hardware smoke if the ticket touches the device layer).
3. Review gates: contracts and schemas changed together; docs updated in the same commit;
   protected files untouched (`git diff main -- BRIEF.md docs/discovery docs/reviews` empty
   except the BRIEF amendment); stale-grep clean; commit messages in format.
4. Steward merges to `main` (squash or rebase per ticket size), marks ticket `merged`,
   deletes the worktree (`git worktree remove .worktrees/<id>`).

## Conflict avoidance

- File-ownership map lives in tickets: before claiming, check open tickets' file lists for
  overlap; if overlap, split the ticket or sequence it.
- Contracts (`schemas/`, `docs/contracts/`) are high-contention: one ticket at a time;
  everyone else branches after it merges.
- `.worktrees/`, `.review.git/`, `prs/` are gitignored except `prs/<id>.md` records.

## Verification (every PR)

`sh tests/fixtures/validate.sh` (ALL CHECKS PASSED) → `cargo test --workspace` →
`python tests/conformance/run.py` → ticket-specific acceptance → stale grep for the
ticket's area. Hardware tickets additionally: `DS_HARDWARE=1 python tests/hw/smoke.py` and,
for pixel claims, the camera harness (`docs/90-verification.md`, `deckstack-verify`).

## Escalation

Stop and escalate to the steward when: a ticket needs a contract change another open ticket
also touches; a test is flaky on real hardware (file it, don't retry-loop); a scope rule in
`docs/00-scope.md` seems wrong (propose an amendment to BRIEF.md, don't silently relax);
hardware behaves differently from ds-vdev (fix the virtual model first —
`hubdeck/CLAUDE.md:101-103` rule carried over).

## CLI self-description contract (referenced by all steering docs)

Normative in `docs/contracts/cli.md`: `ds --help` / `ds <group> --help` /
`ds <group> <verb> --help` always present; `ds schema <name>` prints the JSON Schema from
`schemas/`; `ds describe <path>` prints schema + current value + allowed verbs; output is
`--json` (one object) or `--ndjson` (streaming verbs only; `--json` there is exit 1);
exit codes 0–8 are stable and documented; errors are actionable (`ds: path not found: …
[exit 2]`). Agents must be able to discover every capability from the CLI alone — no UI
required (R1/R10).
