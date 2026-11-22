# 00 — Scope (normative, amended 2026-11-22)

BRIEF.md R1–R12 remain the governing requirements. The owner's 2026-11-22 steering
(BRIEF.md "Scope amendment 2026-11-22") narrows scope as follows. Where any other doc
conflicts with this file, this file wins for alpha.

## In alpha

- **Linux only**: Bazzite/Fedora, KDE Plasma on Wayland first (R9's primary target). No
  Windows, no macOS implementation in alpha — they are design TODOs in
  `docs/40-platforms.md` ("do not start until Linux alpha is stable"). The
  `ds-platform`/`ds-ipc` shims remain so ports are possible later.
- **CLI-first, daemon + stateless CLI** (R1). No UI deliverable: the WS/HTTP adapter is a
  generic transport for agents/debug clients only (R7); a thin debug/web client is a
  post-alpha note, not a plan item.
- **Empty state at start**: no pre-baked presets, layouts, profiles, sample deck, or
  shipped default fixtures. The daemon starts from an empty state document
  (`schemas/state.json` with zero controls/bindings/plugins); agents and users build all
  behaviour through the CLI/API. Test fixtures live under `tests/` and are never installed
  as defaults.
- **No cross-implementation compatibility**: no export/import, no migration from hubdeck
  (Python) or hubdeck-upstream (Rust), no `ds migrate`. Selective manual porting later,
  ticket-driven — `docs/70-porting-notes.md`.
- **Streaming, events/bindings/actions, plugin host + Python wrapper, senders (focus,
  MCP), agent surfaces** — alpha phases 0–6 (`docs/60-roadmap.md`).
- **Camera verification on hardware** — `docs/90-verification.md`; CI uses ds-vdev only.
- **Agent steering is first-class**: `AGENTS.md`, `docs/AGENT-WORKFLOW.md`,
  `skills/deckstack/SKILL.md`, and the CLI self-description contract (`ds schema`,
  `ds describe`, `--help` everywhere, stable exit codes, `--json`/`--ndjson`).

## Post-alpha backlog (documented, not scheduled)

lights.lan integration as a plugin (`docs/80-integrations.md`) · Windows · macOS ·
web/debug UI over the WS/HTTP adapter · selective porting of existing hubdeck plugins
(`docs/70-porting-notes.md`).

## Invariants (checked by conformance tests)

1. `ds daemon start` on a fresh data dir yields a state document with empty
   `/controls`, `/bindings`, `/plugins` — conformance asserts it.
2. No `migrate` verb exists: `ds migrate …` exits 1 (usage) — conformance asserts it.
3. No `ds-web`/UI component exists in the alpha tree; the HTTP adapter serves only
   `/api/*` and `/ws` for agents/debug clients, with a `404` for `/`.
4. CI gates run on Linux only for alpha; Windows/macOS jobs are build-only or absent.
5. Linux-only claims in `docs/40-platforms.md` §0; no alpha exit criterion mentions
   Windows or macOS.
