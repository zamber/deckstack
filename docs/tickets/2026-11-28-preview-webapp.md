# Ticket DS-121 — static preview webapp consuming the /preview API (deck map + plugin action mappings)

Date: 2026-11-28. Type: feature ticket (client app + conformance smoke test; filed with
the implementation on branch `vibe/impl-ds-121-preview-webapp`).
Scope guard: **separate client app only.** Nothing in `crates/` serves HTML and no
daemon code, schema, contract, or test expectation changes with this ticket — the
no-UI scope of `docs/00-scope.md` delta 4 / ADR-014 stays unamended (same reasoning
as ADR-019: delta 4 forbids a UI *deliverable shipped in the daemon*; a standalone
static client under `clients/`, like `clients/ds-py`, is a consumer of the debug
transport, not a daemon component). No ADR/decision-log row: no daemon behaviour
changes (precedent: DS-063 `clients/ds-py`).

## Motivation

- The salon-deck owner wants "the app to preview the orientation and plugin
  mappings" — a human-checkable view of *where each module sits* (connector
  position + base orientation, DS-112) and *which plugin actions are bound to each
  control* (SALON-001 capture session needs exactly this: `ds get /devices` plus
  `/bindings` + plugin manifests read by hand today, `docs/porting/salon-key.md`).
- DS-113 already merged the read-only localhost `/preview` debug surface
  (`docs/contracts/preview.md`): `GET /preview/api/state`,
  `GET /preview/api/action-metadata`, and the `GET /preview/ws` live stream on the
  loopback adapter (`docs/contracts/ipc.md` §1.1: `127.0.0.1` only, bearer token,
  loopback `Host` check T7). The daemon's own `GET /preview` page is a debug
  surface, not this deliverable; DS-121 is a **standalone static webapp** that
  consumes those endpoints.

## Requirements

1. **Location/shape:** `clients/preview-web/` — a single static `index.html` +
   `app.js` + `style.css` (+ optional `config.js`), stdlib-only vanilla JS + SVG, no
   dependencies, no build step. Served by any static server
   (`python3 -m http.server`) or opened as a file; configuration via `?token=…&port=…`
   query params or `config.js`.
2. **Transport:** connect to the daemon's loopback adapter at `127.0.0.1:<port>`
   with `Authorization: Bearer <token>` (HTTP) and `?token=<token>` on the WS
   upgrade (browsers cannot set handshake headers — the reason the daemon accepts
   the query form, ADR-019). Consume `GET /preview/api/state` and
   `GET /preview/api/action-metadata`, plus the `/preview/ws` stream
   (`preview.hello`/`preview.snapshot`/`event`). Because the daemon sends no CORS
   headers, the app must fall back to the WS snapshot (`{rev, state, actions}` — the
   same data as the two GET endpoints) when the HTTP fetches are blocked by the
   browser's same-origin policy, which is the normal case when the app is served
   from a separate static server.
3. **Deck map:** render the base connector grid — flat = 6×4, vertical = 4×6
   (`docs/discovery/40-hardware-and-protocol.md` §9, DS-112) — and place each module
   from `/devices/<serial>` at its connector `position` with its `slot` and
   `orientation` shown. **Defensive reads:** DS-112 (`position`/`orientation`
   fields) is landing on a separate branch, so read them only if present and fall
   back to serial-keyed placement when absent.
4. **Controls + action mappings:** render the control surface from `/controls`
   (keys 0–14, dial, panel, knobs 0–2, faders 0–4, rollers 0–1, base). Hover/click a
   control to show its bound plugin actions: bindings from `/bindings` whose topic
   matches the control (`deck/control/…`, `docs/contracts/actions.md` §2 segment
   grammar), each step resolved against the action-metadata aggregate — plugin
   steps show the action `name`, `params` schema and the plugin's `capabilities`;
   core steps (`set`/`emit`/`plugin`/`exec`/`sleep`) show their args/capability
   docs (`schemas/action-metadata.json/v1`).
5. **Read-only:** the app never sends a mutating op — GET/WS read-only ops only
   (`ping`/`get`/`subscribe`), matching the `/preview` surface's own guarantee.
6. **Smoke test:** `tests/conformance/preview_web/run.py`, registered in
   `tests/conformance/run.py` (`-k preview_web`): boots ds-vdev + `ds-daemon
   --preview`, serves `clients/preview-web/` with `python3 -m http.server`, fetches
   `index.html`/`app.js`/`style.css` (200), and asserts the endpoints the app
   needs: `/preview/api/state` → 200 with `{rev, controls, bindings, plugins,
   devices}` and non-empty `devices`; `/preview/api/action-metadata` → 200 with
   `protocol:1` and core verbs exactly `set, emit, plugin, exec, sleep`; `/preview/ws`
   → 101 + `preview.hello` + `preview.snapshot`; missing token → 401. No browser
   automation.

## Acceptance

- `python3 tests/conformance/run.py -k preview_web` green; the full
  `python3 tests/conformance/run.py` shows no new failures (the two pre-existing
  safety_070/T8 failures aside).
- Manual: against a ds-vdev + `ds-daemon --preview` stack the app loads from a
  static server and renders the deck map (base grid + modules + controls), and
  hovering a control lists its bound actions from the action-metadata document.
- `sh tests/fixtures/validate.sh` prints ALL CHECKS PASSED; `cargo test --workspace`
  green; `sh tools/check.sh` green; committed on `vibe/impl-ds-121-preview-webapp`.

## References

- `docs/contracts/preview.md` (DS-113 surface), `docs/contracts/ipc.md` §1.1 (loopback
  adapter, bearer + T7 Host), `schemas/preview.json`, `schemas/action-metadata.json`.
- `docs/tickets/2026-11-25-module-position-orientation.md` (DS-112 — `position`/
  `orientation` fields, 4×6 vs 6×4 grid), `docs/tickets/salon/SALON-001.md`.
- `docs/contracts/actions.md` §1–3 (topics, binding lookup grammar, core verbs),
  `schemas/binding.json`, `schemas/state.json` (`/devices`, `/controls`).
- `docs/adr/014-no-web-ui.md`, `docs/adr/019-preview-debug-surface.md`,
  `docs/discovery/40-hardware-and-protocol.md` §9.
- `clients/ds-py/` (precedent for a separate client under `clients/`).
