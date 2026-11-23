# 70 — Porting notes (post-alpha, ticket-driven)

Owner steering 2026-11-22 (delta 2): **no cross-implementation compatibility** — no export/
import, no migration from hubdeck (Python) or hubdeck-upstream (Rust), no `ds migrate` verb,
no config compatibility layer, no automated importer. Anything worth keeping from the old
apps is reimplemented by hand against the new contracts, one ticket at a time, after alpha.

## Landed status (2026-11-23, ticket-driven)

- **lights.lan support — landed as a plugin (DS-070)**: the sibling repo
  `deckstack-lights` (`../deckstack-lights`, own repo per `repos.toml`), plugin
  `lights.set {room, light, on, brightness, color}`, capability `net:lights.lan` only,
  config as data, fake HueMux + end-to-end tests (`tests/run.py`, exit 0). Spec:
  `docs/80-integrations.md`.
- **Salon key — ported end to end (DS-071)**: `docs/porting/salon-key.md` — a key press
  (or a focus event) recolours the key and drives the Salon lights via
  `ds bind … --do 'set …' --do 'plugin lights.set {…}'`, proven on ds-vdev + the fake
  HueMux with a repeatable recorded run (rc=0).
- **Porting inventory — landed (DS-071)**: `docs/porting/inventory.md` — every candidate
  old-app behaviour from `docs/discovery/` with `path:line` evidence and a
  port / not-needed / post-alpha decision; the single source for future porting tickets.
- **Window-focus recoloring**: already in alpha (phase 6) as `ds-focus` + an
  `app/focus/changed` binding — no porting needed (`docs/components/focus-sender.md`).
- Anything with `{python: …}` code-in-config (`hubdeck/src/hubdeck/plugins/python.py:58`)
  is **never** ported as code-in-config; it becomes an external script behind an `exec`
  binding or a plugin (R8).

The rule stays: **reimplement, never import** — anything worth keeping from the old apps is
reimplemented by hand against the new contracts, one ticket at a time; the old app is never
a runtime dependency.

## How a porting ticket looks

1. Ticket lists the exact old-app behaviour with `path:line` evidence (from
   `docs/discovery/`).
2. Reimplement against `schemas/` + `docs/contracts/*` — new state paths, new bindings,
   plugin if needed. No compatibility shims, no parallel old/new paths.
3. Acceptance runs on ds-vdev (and hardware smoke if device-touching); camera check for
   pixel claims (`docs/90-verification.md`).
4. The old app is never a runtime dependency; coexistence is not a goal (the HID device
   has one owner — `hubdeck/docs/masterhub-protocol.md:36-37`).
