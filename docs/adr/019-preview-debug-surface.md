# ADR-019 — Opt-in read-only localhost `/preview` debug surface + action metadata (DS-113)

Status: accepted (ticket DS-113, 2026-11-25).

## Context

Two consumers want to see "what can this deck do" without touching it: the
salon-deck map (human-readable `/bindings` + plugin manifests) and a browser virtual
deck with hover docs. The daemon had no queryable action-metadata document — action
docs lived only inside plugin manifests and the closed step vocabulary
(`docs/contracts/actions.md` §3) — and no renderable deck view. Alpha has no UI
(`docs/00-scope.md` delta 4, ADR-014): the WS/HTTP adapter is a generic transport for
agents/debug clients only, with `/` → 404.

## Decision

1. **Action-metadata aggregate** (`schemas/action-metadata.json/v1`): a derived,
   read-only document combining the closed core step vocabulary (`set`, `emit`,
   `plugin`, `exec`, `sleep` with args/capabilities) with every registered plugin's
   manifest actions (`id`/`name`/`params` schema/capabilities). Served by the daemon at
   `GET /preview/api/action-metadata`, embedded in the `/preview` page and the WS
   snapshot, and fetchable as a schema via `ds schema action-metadata`. Additive `/v1`
   only; never stored, never bumps `rev`.
2. **`/preview` debug surface** on the loopback WS/HTTP adapter
   (`docs/contracts/preview.md`, `schemas/preview.json/v1`): binds `127.0.0.1` only,
   bearer token required on every request (WS upgrade also accepts `?token=`), refuses
   non-loopback `Host` with 421 (T7), port published in `<data-dir>/http.port`.
   Serves `GET /preview` (HTML virtual deck with hover action docs),
   `GET /preview/api/action-metadata`, `GET /preview/api/state`, and
   `GET /preview/ws` (live state over WS). **Read-only**: non-GET → 405, unknown →
   404, WS accepts only `ping`/`get`/`subscribe`/`unsubscribe` (mutating ops get
   `E_SCOPE` and never run), and the surface never issues `set`/`emit`/`plugin`/`exec`
   or HID writes.
3. **Default-off, opt-in**: started only by the `ds-daemon run --preview` flag or
   `/policy/preview=true` at boot; `/policy/preview=false` disables it live (404).
   Production profiles ship with it off.

## Why this does not amend delta 4 (no-UI scope)

Delta 4 / ADR-014 forbids a **UI deliverable** — a shipped, planned web UI component.
This is a debug endpoint on the existing agent/debug transport: localhost-only,
token-gated, read-only, opt-in, default-off, disableable via `/policy`, serving HTML
to a bearer-authenticated loopback client for inspection. It adds no UI component, no
bin, no shipped default page, and no daemon behaviour change for any capability
reachable through `ds`; every `/preview` request is a read. Hence the no-UI scope
stands unamended; this ADR records the justification as the ticket requires.

## Alternatives

- Serve the metadata only as an IPC op — rejected: the ticket's consumers (browser
  hover docs, salon-deck map) need it over the loopback HTTP/WS adapter, and a new
  control-plane op would change the `/v1` op set for what is a debug read.
- Always-on listener that 404s when disabled — rejected: default-off means **no
  listener** in production profiles, so there is nothing to probe (T7 stays
  meaningfully "no listener" when off).
- WS auth via header only — rejected: browsers cannot set headers on a WebSocket
  handshake, so the upgrade additionally accepts `?token=` (loopback-only, still
  constant-time checked).

## References

Ticket `docs/tickets/2026-11-25-action-metadata-virtual-deck.md`; `docs/contracts/preview.md`;
`docs/contracts/ipc.md` §1.1/§2; `docs/contracts/actions.md` §3; `docs/contracts/plugins.md` §3/§9;
`docs/00-scope.md` delta 4; ADR-014; `docs/20-security.md` T7; decision log D40;
`tests/conformance/preview/`; `schemas/preview.json`, `schemas/action-metadata.json`.
