# Ticket DS-113 — action metadata surface + read-only local /preview virtual deck

Date: 2026-11-25. Type: feature ticket (docs-only at filing; implementation later).
Scope guard: documentation-only ticket — no daemon code, schemas, contracts, or tests
change with this file.

## Motivation

Two consumers want to see "what can this deck do" without touching it:

- the **salon-deck map**: which control does what, which binding/plugin action is
  attached to it — the human-readable form of `/bindings` + `/plugins/<id>/manifest`
  (`docs/porting/salon-key.md` is the worked example of such a map);
- a **browser virtual deck** with hover docs: render the live deck layout and show the
  action docs (name, params schema, capabilities) on hover.

The daemon currently has no action-metadata surface: action docs exist only inside
plugin manifests (`schemas/plugin-manifest.json`, `docs/contracts/plugins.md` §3) and
the closed step vocabulary (`docs/contracts/actions.md` §3), neither of which is
queryable as one document, and there is no renderable deck view (alpha has no UI —
`docs/00-scope.md` delta 4, ADR-014).

## Requirements

1. **Registered action metadata.** A read-only aggregate the daemon can serve, built
   from (a) the closed step vocabulary in `docs/contracts/actions.md` §3 (`set`, `emit`,
   `plugin`, `exec`, `sleep` with their args/capabilities) and (b) every registered
   plugin's manifest actions (`/plugins/<id>/manifest.actions[]` with `id`, `name`,
   `params` JSON Schema, capabilities — `docs/contracts/plugins.md` §3, §9). New wire
   shape goes in `schemas/` + `docs/contracts/` in the same commit (AGENTS.md invariant);
   additive-only per the `protocol` versioning rule in `docs/contracts/ipc.md` §2.
2. **Read-only local `/preview` surface.** An opt-in debug endpoint on the existing
   loopback adapter (`docs/contracts/ipc.md` §1.1: WS/HTTP on `127.0.0.1` only, bearer
   token + `Host: 127.0.0.1|localhost` check, T7) serving HTML over HTTP and live state
   over WS: it renders the current deck (controls from `/controls`, layout from
   `/devices/*/slot` once DS-112 lands) and the action docs from (1) on hover.
   **No HID writes, no state mutation** — every `/preview` request is a read; the
   surface never issues `set`/`emit`/`plugin`/`exec`.
3. **Localhost-only.** Bind `127.0.0.1`, refuse non-loopback `Host` headers (existing
   T7 check), bearer token required, same as the `/api/*` adapter; `/` stays 404 for
   anything but the debug surface when explicitly enabled (alpha scope: the WS/HTTP
   adapter is for agents/debug clients only — this is a debug surface, not a UI
   deliverable, so it stays inside the no-UI scope of `docs/00-scope.md` delta 4 and
   ADR-014; it must be disableable via `/policy` or a flag and default-off in
   production profiles).
4. **Schema / ADR impact.** New schema for the action-metadata document and the
   `/preview` routes (schema `$id` bump only if breaking — expected additive `/v1`),
   contract section in `docs/contracts/ipc.md` (or a new `docs/contracts/preview.md`),
   and an ADR in `docs/adr/` + a row in `docs/decision-log.tsv` per AGENTS.md, since the
   surface touches the no-UI scope (justify: debug-only, localhost-only, read-only,
   opt-in — does not amend delta 4, which forbids a UI deliverable, not a debug
   endpoint).
5. **vdev test plan.** With `ds-vdev` + daemon on `virtual:` (the
   `docs/components/virtual-device.md` harness): (a) action-metadata doc lists the
   demo plugin's actions with params schemas after `examples/plugins/run_demo.py`
   installs it; (b) `GET /preview` on 127.0.0.1 with the token returns 200 HTML, and a
   non-loopback Host or missing token is refused; (c) a WS subscription on `/preview`
   streams control-state changes; (d) **negative test**: no write op is accepted on
   `/preview` (POST → 404/405) and no HID write appears in `ds-vdevctl writes` during
   a preview session; (e) conformance entry in `tests/conformance/` so the surface
   stays read-only.

## Acceptance

With the daemon on vdev (or hardware), browse `http://127.0.0.1:<port>/preview` with
the install token: the current deck layout renders, hovering a control shows its
bound actions with param docs from the registered manifests, and the whole session
performs zero state mutations (`rev` unchanged, `ds-vdevctl writes` shows no
device-bound reports).

## References

- `docs/contracts/actions.md` §3 (closed step vocabulary), §1 (event topics incl.
  `deck/device/<serial>/orientation`).
- `docs/contracts/plugins.md` §3 (manifest actions + params schema), §9
  (`/plugins/<id>` lifecycle state incl. manifest).
- `docs/contracts/ipc.md` §1.1 (loopback WS/HTTP adapter, T7 Host check, `/` = 404),
  §2 (additive versioning).
- `docs/00-scope.md` delta 4 + `docs/adr/014-no-web-ui.md` (no-UI scope; debug-surface
  justification), `docs/10-architecture.md` §1 (no privileged UI state).
- `docs/porting/salon-key.md` (the salon-deck map motivation),
  `docs/components/virtual-device.md` + `examples/plugins/run_demo.py` (vdev test
  harness), `docs/90-verification.md` (CI uses vdev only; camera only for pixels).
