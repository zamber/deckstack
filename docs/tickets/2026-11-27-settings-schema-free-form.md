# Ticket DS-119 — mark `/settings` explicitly free-form in schema and contract

Date: 2026-11-27. Type: docs+schema-description ticket (spec filed from
`docs/reviews/2026-11-27-opportunities.md` section 2 item 3 / P1-3).
Scope guard: a `description` string on one schema node plus contract wording.
**No hardware needed — vdev/local tests only** (`sh tests/fixtures/validate.sh`
validates the schema change locally; no deck, no camera, no network).

## Motivation

`schemas/state.json:113` declares `"settings": {"type": "object"}` with no
`properties` and no `additionalProperties` restriction, while
`docs/contracts/state.md:40` documents concrete `/settings/...` keys (sender
backends, log level, exec_path removed → `/policy/exec`). A reader cannot tell
whether `/settings` is closed (schema-validated) or intentionally free-form.
Closing the object now would risk rejecting existing state documents, so the
minimal, safe alignment is to declare the free-form intent in both places.

## Requirements

1. **`schemas/state.json:113`**: keep `{"type": "object"}` exactly, add a
   `"description"` stating that `/settings` is intentionally free-form
   (consumer/app-defined keys; the keys named in `docs/contracts/state.md` are
   the currently documented ones, not an exhaustive schema). No
   `properties`/`additionalProperties` are added.
2. **`docs/contracts/state.md:40`**: reword the `/settings/...` line to say
   the object is free-form by design — the listed keys (sender backends, log
   level; exec_path removed → see `/policy/exec`) are the documented subset,
   and arbitrary additional keys are permitted and ignored by the daemon.
3. **Test**: no fixture changes are needed (description-only schema edit is
   additive and example validation is unaffected). Add a local assertion
   (docs-marker check alongside DS-117/DS-118) that `docs/contracts/state.md`
   contains the free-form wording; `sh tests/fixtures/validate.sh` must still
   validate the schema and all example corpora.

## Acceptance

- `schemas/state.json` still parses and every example corpus validates:
  `sh tests/fixtures/validate.sh` prints ALL CHECKS PASSED.
- `docs/contracts/state.md:40` states the free-form rule; schema description
  and contract agree.
- A `ds set /settings/<arbitrary-key> …` against a vdev daemon is accepted
  exactly as before (no behavior change; conformance `set` cases unchanged).

## References

- `docs/reviews/2026-11-27-opportunities.md` §2 item 3, §5 P1-3.
- `schemas/state.json:113` (`"settings": {"type": "object"}`).
- `docs/contracts/state.md:40` (`/settings/...` documented keys).
- `tests/fixtures/validate.sh` (schema + example corpus validation).
