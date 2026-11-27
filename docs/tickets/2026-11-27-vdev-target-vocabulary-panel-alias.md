# Ticket DS-118 — document the ds-vdevctl `panel` alias for the S6 `keys` target

Date: 2026-11-27. Type: docs ticket (spec filed from
`docs/reviews/2026-11-27-opportunities.md` section 2 item 2 / P1-3).
Scope guard: documentation-only alignment decision. **No hardware needed —
vdev/local tests only** (doc-marker assertions run in the local conformance
suite; no deck, no camera, no network).

## Motivation

Contract S6 fixes the target vocabulary as `keys | key:<0-14> | dial` and says
"`panel`/`screen` are not targets" (`docs/contracts/streaming.md:6,14`;
`docs/contracts/state.md:46-47`), enforced by `bins/ds/src/stream/mod.rs:45-58`
(`parse_target` refuses `panel`). But the vdev test tooling speaks `panel`:
`crates/ds-vdev/src/target.rs:33` maps `"panel"` to `Target::Panel` (with the
inverse at `:46` and the deliberate test at `:86` asserting
`Target::parse("keys") == None`), surfaced as `ds-vdevctl frames panel|key:N|dial`
(`bins/ds-vdevctl/src/main.rs:13,27`) and documented in
`docs/components/virtual-device.md:29,81`. Salon-deck session scripts mix
`ds stream push keys …` with `ds-vdevctl frames panel …`, so the two spellings
need one explicit mapping note.

## Alignment decision (minimal diff)

**Keep the vdev tooling spelling `panel`; document it as a vdev-only alias of
the S6 `keys` target. Do not change `crates/ds-vdev/src/target.rs` or
`bins/ds-vdevctl`.** Justification: S6 governs the `ds` CLI and the state/IPC
surfaces, where `parse_target` (`bins/ds/src/stream/mod.rs:45-58`) already
enforces it; `ds-vdevctl` is a test/CI tool (`docs/contracts/cli.md:33-36`)
whose `frames` command names the physical panel on the vdev control socket —
the two surfaces never exchange target strings (the daemon stream path is
`stream.sock`, the vdev control path is the vdev socket). Renaming the vdev
spelling would churn `target.rs`, its tests, and every script that already
says `frames panel`, while a two-line doc note costs nothing and cannot break
a test. The contract therefore gains an explicit alias sentence instead of the
tooling gaining a second spelling.

## Requirements

1. **`docs/components/virtual-device.md:29` and `:81`**: state explicitly that
   `panel` in `ds-vdevctl frames panel` is the ds-vdevctl-local spelling of the
   contract-S6 target `keys` (the 15-key M4315 panel, wire `02 35` idx 0), i.e.
   `ds-vdevctl frames panel` ≡ `ds stream push keys`.
2. **`docs/contracts/streaming.md:14`**: after the sentence
   "`panel`/`screen` are not targets; `ds stream push keys …` means the panel",
   add one sentence: the single exception is the ds-vdevctl test tooling, which
   spells the same target `panel` (see `docs/components/virtual-device.md`);
   the `ds` CLI and all state paths keep the S6 vocabulary only.
3. **`docs/contracts/state.md:46-47`**: add the same one-line pointer next to
   "no `panel`/`screen` synonyms".
4. **`crates/ds-vdev/src/target.rs:30`**: extend the doc comment (comment-only,
   no behavior change) to reference the alias note in the contract; the test at
   `:86` (`parse("keys") == None`) stays and is now documented as intentional.
5. **Doc-marker test**: add a local conformance/docs assertion (alongside the
   cli.md marker check from DS-117) that `docs/components/virtual-device.md`,
   `docs/contracts/streaming.md`, and `docs/contracts/state.md` all carry the
   `panel` ≡ `keys` alias note.

## Acceptance

- The three docs name the `panel`→`keys` mapping in one sentence each; a
  reader of a session script sees the equivalence without reading source.
- `cargo test -p ds-vdev` passes unchanged (including `target.rs:86`);
  `python tests/conformance/run.py -k streaming` passes unchanged — no behavior
  changed anywhere.
- `sh tests/fixtures/validate.sh` prints ALL CHECKS PASSED.

## References

- `docs/reviews/2026-11-27-opportunities.md` §2 item 2, §5 P1-3.
- `docs/contracts/streaming.md:6,14`; `docs/contracts/state.md:46-47`;
  `docs/contracts/cli.md:33-36` (ds-vdevctl = test/CI tool).
- `bins/ds/src/stream/mod.rs:45-58` (`parse_target` refuses `panel`).
- `crates/ds-vdev/src/target.rs:3,30,33,46,86`; `bins/ds-vdevctl/src/main.rs:13,27`;
  `docs/components/virtual-device.md:29,81`.
