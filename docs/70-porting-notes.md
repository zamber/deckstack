# 70 — Porting notes (post-alpha, ticket-driven)

Owner steering 2026-11-22 (delta 2): **no cross-implementation compatibility** — no export/
import, no migration from hubdeck (Python) or hubdeck-upstream (Rust), no `ds migrate` verb,
no config compatibility layer, no automated importer. Anything worth keeping from the old
apps is reimplemented by hand against the new contracts, one ticket at a time, after alpha.

## Candidate ports (initial list, not scheduled)

- A small set of buttons: the Salon deck's key/knob behaviour (the live
  `~/.config/hubdeck/config.yaml` uses `plugins.huemux.url: http://lights.lan`,
  `presets.default.extends: [salon-pages, salon-controls]`) — reimplemented as bindings +
  the lights plugin, not imported.
- **lights.lan support**: reimplemented as a plugin — `docs/80-integrations.md`.
- Window-focus recoloring: reimplemented as `ds-focus` + a binding — already in alpha
  (phase 6), no porting needed.
- Anything with `{python: …}` code-in-config (`hubdeck/src/hubdeck/plugins/python.py:58`)
  is **never** ported as code-in-config; it becomes an external script behind an `exec`
  binding or a plugin (R8).

## How a porting ticket looks

1. Ticket lists the exact old-app behaviour with `path:line` evidence (from
   `docs/discovery/`).
2. Reimplement against `schemas/` + `docs/contracts/*` — new state paths, new bindings,
   plugin if needed. No compatibility shims, no parallel old/new paths.
3. Acceptance runs on ds-vdev (and hardware smoke if device-touching); camera check for
   pixel claims (`docs/90-verification.md`).
4. The old app is never a runtime dependency; coexistence is not a goal (the HID device
   has one owner — `hubdeck/docs/masterhub-protocol.md:36-37`).
