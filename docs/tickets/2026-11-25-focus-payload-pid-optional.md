# Ticket DS-114 — accept app/focus/changed without a pid

Date: 2026-11-25. Type: bug ticket (docs-only at filing).
Scope guard: documentation-only; no daemon code, schemas, contracts, or tests change with this file.

## Symptom

During the 2026-11-25 ambilight soak the daemon rejected a focus event from ds-focus:

```
ingest of "app/focus/changed" rejected (depth 0): E_SCHEMA app/focus/changed payload needs an integer 'pid', got {"app":"steam","title":"Steam"} — no state change
```

ds-focus documents that `pid` is included only when the backend knows it. The
event is dropped, so `/focus` and the Salon app's focus label stay stale and the
focused-player media selection falls back to another player.

## Root cause

The router/state validation of `app/focus/changed` requires an integer `pid`,
contradicting `docs/contracts/actions.md` §1 ("pid is included when the backend
knows it").

## Requirements

1. Accept `pid` absent or null; validate it as an integer when present.
2. Keep the `app`/`title` string validation and the `/focus` mirror.
3. Add a router unit test for a payload without pid; a non-integer pid is still
   rejected.
4. Update the contract wording if necessary; no breaking schema bump.

## Acceptance

`ds emit app/focus/changed '{"app":"steam","title":"Steam"}'` updates `/focus`
and the bus; `{"pid":"x"}` is still rejected; the deck's focus label updates for
Steam.

## References

The journal line at 2026-11-25 23:55:12; `docs/contracts/actions.md` §1;
`crates/ds-daemon/src/router`; `bins/ds-focus` help.
