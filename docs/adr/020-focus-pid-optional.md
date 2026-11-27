# ADR-020 — `app/focus/changed` accepts an absent or null `pid`

Status: accepted (ticket DS-114, 2026-11-25 ambilight soak regression).

## Context

During the 2026-11-25 ambilight soak the daemon rejected a focus event from ds-focus:

```
ingest of "app/focus/changed" rejected (depth 0): E_SCHEMA app/focus/changed payload needs an integer 'pid', got {"app":"steam","title":"Steam"} — no state change
```

The router's `validate_mirror_payload` (`crates/ds-daemon/src/router/ingest.rs`) required
an integer `pid`, contradicting the producer contract: ds-focus documents that `pid` is
included only when the backend knows it (KWin does, wlr does not —
`docs/components/focus-sender.md`). The event was dropped, so `/focus` and focus-based
bindings stayed stale.

## Decision

- The `app/focus/changed` payload is `{app: string, title: string, pid?: integer}`.
  `pid` **absent or null** is accepted and omitted from the `/focus` mirror; a **present
  non-integer `pid`** (string, float, bool, …) is still rejected with `E_SCHEMA` before
  any state change.
- `app`/`title` string validation, the unknown-field rejection, and the atomic `/focus`
  mirror (S7) are unchanged. `schemas/state.json` already did not require `pid` in
  `/focus`, so no schema `$id` bump — this is a non-breaking widening of the accepted
  event payloads.
- Contract wording updated in the same commit: `docs/contracts/actions.md` §2/§6,
  `docs/contracts/state.md` §7/§state tree, `docs/components/focus-sender.md`, and the
  `/focus` description in `schemas/state.json`.

## Alternatives

- Mirror `pid: null` into `/focus` when absent — rejected: `schemas/state.json` types
  `pid` as integer, so a null would be schema-invalid; omitting the key is the clean
  representation and `/focus` never required it.
- Keep requiring `pid` and make ds-focus always send one — rejected: the wlr backend
  genuinely has no pid; the contract already says "when the backend knows it".

## References

Ticket `docs/tickets/2026-11-25-focus-payload-pid-optional.md`; decision log D40;
`crates/ds-daemon/src/router/ingest.rs` (`validate_mirror_payload`, unit tests);
`tests/conformance/focus_ingest/run.py`.
