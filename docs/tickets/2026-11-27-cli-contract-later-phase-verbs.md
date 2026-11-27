# Ticket DS-117 — mark later-phase verbs as refused in the CLI contract

Date: 2026-11-27. Type: docs+test ticket (spec filed from
`docs/reviews/2026-11-27-opportunities.md` section 2 item 1 / P1-3).
Scope guard: one contract table edit plus a conformance assertion that the
contract carries the marker. **No hardware needed — vdev/local tests only**
(the conformance suite runs against the real binary on vdev/loopback; no deck,
no camera, no network).

## Motivation

`docs/contracts/cli.md:22-28` documents `ds emit`, `ds events`, `ds plugin …`,
`ds token …`, and `ds daemon … restart` as plain verbs with no "refused"
marker — a grep for `phase`/`later` in that file finds nothing. The binary
refuses all of them today: `bins/ds/src/cli.rs:124-125` defines
`LATER_PHASE = ["emit","events","plugin","token"]`, enforced at `:610-614`
(parse path) and `:723-726` (help path) with exit 1 and
"verb '{v}' is not implemented in phase 1 (later ticket)"; `restart` is not in
`DAEMON_SUBS` (`cli.rs:127`) and is rejected as "unknown daemon subcommand"
(`cli.rs:439-442`, asserted at `:1020`). A reader of the contract alone would
expect these verbs to work. This is a docs-only honesty fix.

## Requirements

1. **Mark the rows in `docs/contracts/cli.md:22-28`.** Add a visible
   "recognized but refused — exit 1, `not implemented in phase 1` until its
   phase ticket lands" note to the `emit` (:22), `events` (:24), `plugin`
   (:26), and `token` (:28) rows, and to the `daemon` row (:27) note that
   `restart` is not an implemented subcommand (only `start|stop|status`).
2. **Add one summary line under the table** (next to the existing `migrate`
   note at `docs/contracts/cli.md:30`): the four later-phase verbs are parsed
   and then refused with exit 1; when a verb's ticket lands, the marker moves
   from "refused" to the real grammar and the conformance tripwire
   (`tests/conformance/cli/run.py:615-627`) fails until the test lists move.
3. **Lock the wording with a test.** Extend the CLI conformance suite
   (`tests/conformance/cli/run.py`) with an assertion that
   `docs/contracts/cli.md` rows for the `LATER_PHASE_VERBS`
   (`run.py:98-99`) and for `daemon … restart` contain the refusal marker —
   so the contract cannot silently drift from the binary again. The existing
   behavior tests stay as the enforcement lock: `run.py:615-627`
   (`test_cli_030_later_phase_verbs_refused_until_landed`, asserts exit 1 +
   "not implemented" + "[exit 1]"), `bins/ds/src/cli.rs:924` (unit: `emit`
   → "not implemented in phase 1"), `cli.rs:1020` (unit: `daemon restart` →
   "unknown daemon subcommand").
4. **No binary behavior change.** `ds --help`, per-verb `--help`, and the
   exit codes are exactly as today; only the contract text and the new
   doc-marker assertion change.

## Acceptance

- `docs/contracts/cli.md:22-28` shows the refusal marker on all five rows and
  the summary line under the table cites exit 1 + "not implemented in phase 1".
- `python tests/conformance/run.py -k cli_010 -k cli_030` passes (help walk
  still pins the advertised verb set; later-phase tripwire still green).
- `ds emit app/x '{}'`, `ds events`, `ds plugin list`, `ds token list`,
  `ds daemon restart` each exit 1 with the documented message.
- `sh tests/fixtures/validate.sh` prints ALL CHECKS PASSED.

## References

- `docs/reviews/2026-11-27-opportunities.md` §2 item 1, §5 P1-3.
- `docs/contracts/cli.md:22-30` (verb table + migrate precedent), `:81-87`
  (exit-code examples).
- `bins/ds/src/cli.rs:124-127` (`LATER_PHASE`, `DAEMON_SUBS`),
  `:439-442` (unknown daemon subcommand), `:610-614`, `:723-726`
  (refusal sites), `:924`, `:1020` (unit tests).
- `tests/conformance/cli/run.py:98-99` (`LATER_PHASE_VERBS`),
  `:615-627` (`test_cli_030_later_phase_verbs_refused_until_landed`).
