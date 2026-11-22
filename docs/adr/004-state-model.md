# ADR-004 — State model: one JSON document, path-addressed, revisions

Status: accepted (planning).

## Context

R2: state is the product — stable paths, cheap queries (single key, subtree, filtered),
machine-readable; fast while frames stream. R3: every observable attribute settable. The
current apps keep config in YAML files with mtime hot-reload (`hubdeck/.../engine.py:469-472`,
`config/loader.py:282-311`) or JSON profiles (`upstream/.../engine/profile.rs:882-909`).

## Decision

One JSON document owned by the daemon. Addressing: `/meta /devices/<serial> /controls/…
/bindings/… /plugins/… /streams/… /settings /policy /focus` (docs/contracts/state.md §1).
Modules keyed by USB **serial** so hot-plug restores state. Every mutation is an atomic
transaction with a global `rev`; optimistic concurrency via `if_rev` (`E_CONFLICT` with
`current_rev`). Subscriptions per path/wildcard with drop-counting. Persisted atomically
(temp+rename, debounced 250 ms). Query semantics: exact / subtree / one-segment `*` /
field projection; filtering client-side (jq). Budget: p50 ≤ 2 ms, p99 ≤ 10 ms under
32 fps frame load; the state lock is never held across I/O.

## Alternatives

- **Per-entity files** — rejected: no cross-entity transactions, rename races.
- **SQLite** — rejected: heavier dependency, second format to reconcile; the document is
  small and JSON is the R2-native format.
- **Config-files-as-state (status quo)** — rejected: mtime polling, broken-file semantics,
  no transactions.

## Consequences

Queries are trivially fast (in-memory RwLock) and the whole deck is `ds get / --json`.
Conflict rules in docs/contracts/state.md §6. Negative: one big document — mitigated by
field projection and wildcard subscriptions.

## References

BRIEF.md R2/R3; docs/contracts/state.md; schemas/state.json; decision log D05, D13.
