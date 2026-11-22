# tests/state — ds-state test fixtures (ticket DS-011)

- `empty-state.json` — the canonical **empty state document** (scope invariant, ADR-013):
  what `ds_state::Store::fresh()` / `ds_state::empty_document()` produces on a fresh data
  dir. The Rust integration test `crates/ds-state/tests/state.rs` asserts the crate's
  fresh document is exactly this fixture.
- `validate_empty_state.js` — standalone schema check of the fixture against
  `schemas/state.json` (external `$ref`s resolve against the local `schemas/` by `$id`),
  plus the negative cases the schema genuinely rejects:

  ```sh
  node tests/state/validate_empty_state.js   # prints ALL CHECKS PASSED
  ```

  Known contract gap (reported, not worked around here): `schemas/state.json` uses
  `patternProperties` without `additionalProperties: false` under `/controls` and
  `/streams`, so pure JSON-Schema validation admits e.g. `/controls/keys/99` although the
  schema description says indices `15`/`99` must not match (S8). The index/target
  vocabulary is enforced by the ds-state path layer (`crates/ds-state/src/path.rs`);
  the schema itself is unchanged in this ticket.

The behavioral tests (rev semantics, transaction rollback, telemetry split, persistence,
crash recovery) live in `crates/ds-state/tests/state.rs` and run with
`cargo test -p ds-state`.
