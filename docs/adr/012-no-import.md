# ADR-012 — No import/migration/compatibility with the existing apps

Status: accepted (owner steering 2026-11-22, delta 2).

## Context

The previous plan had `ds migrate import` from `~/.config/hubdeck` and a migration phase.
The old configs are code-adjacent (`{python: …}` exec, `hubdeck/.../plugins/python.py:58`)
and YAML-coupled; an importer would either launder that or need a validation layer as big
as a port.

## Decision

No export/import, no `ds migrate` verb (conformance: `ds migrate …` exits 1), no config
compatibility layer, no automated importer. `docs/70-migration.md` replaced by
`docs/70-porting-notes.md`: selective manual porting, ticket-driven, post-alpha; candidates
are a small set of buttons and lights.lan support (`docs/80-integrations.md`); everything
is reimplemented against the new contracts. The old apps are never a runtime dependency
(HID has one owner, `hubdeck/docs/masterhub-protocol.md:36-37`).

## Alternatives

- Automated importer — rejected: imports code-adjacent config (R8 risk), and the mapping is small enough to do by hand per ticket.
- Coexistence/cutover tooling — rejected: single HID owner makes concurrent use impossible anyway.

## References

BRIEF.md amendment; `docs/70-porting-notes.md`; `docs/contracts/cli.md` §1; decision log D32.
