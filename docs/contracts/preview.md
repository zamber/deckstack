# Contract: the `/preview` debug surface + action metadata (DS-113)

Normative schemas: `schemas/preview.json` (routes + WS messages) and
`schemas/action-metadata.json` (the aggregate document). Example corpus:
`tests/fixtures/preview-examples.json` (validated by `sh tests/fixtures/validate.sh`).
Transport parent contract: `docs/contracts/ipc.md` §1.1. Design: ADR-019. Scope:
debug-only endpoint inside the no-UI scope of `docs/00-scope.md` delta 4 / ADR-014 —
it is **not** a UI deliverable and does not amend delta 4.

## 1. What this is

An **opt-in, read-only, localhost-only debug endpoint** on the loopback WS/HTTP
adapter. It exists so a human or agent can see "what can this deck do" and the live
deck state without any risk of mutation:

- `GET /preview` — HTML page rendering the current deck (controls from `/controls`,
  bindings from `/bindings`, plugin lifecycle from `/plugins`); hovering a control
  shows its bound actions and the action docs (name, params schema, capabilities).
- `GET /preview/api/action-metadata` — the **action-metadata aggregate**
  (`schemas/action-metadata.json/v1`): the closed core step vocabulary
  (`docs/contracts/actions.md` §3: `set`, `emit`, `plugin`, `exec`, `sleep` with args
  and capabilities) plus every registered plugin's manifest actions
  (`id`/`name`/`params` JSON Schema/capabilities, `docs/contracts/plugins.md` §3/§9).
  The aggregate is derived on demand — never stored, never mutates state, never bumps
  `rev`.
- `GET /preview/api/state` — `{rev, controls, bindings, plugins, devices}` snapshot.
- `GET /preview/ws` — WebSocket: on connect the server sends `preview.hello` then
  `preview.snapshot` (`{rev, state, actions}`); the client may then `subscribe` /
  `unsubscribe` / `get` / `ping` (see §4) and receives live bus `event` frames.

## 2. Localhost-only and auth (T7)

- The listener binds **`127.0.0.1` only** (ephemeral port). The port is published in
  `<data-dir>/http.port` (0600) while the surface runs; the file is removed on
  shutdown. When the surface is off there is **no listener** and no `http.port`.
- Every request — including the WS upgrade — requires a valid token:
  `Authorization: Bearer <token>` (install token or a scoped token from
  `tokens.json`; checked constant-time against the current on-disk set, so revocation
  is instant, S12). On the WS upgrade only, `?token=<token>` in the query is also
  accepted (browsers cannot set headers on a WebSocket handshake). Missing/bad token →
  **401**.
- Every request must carry a loopback `Host` header: `127.0.0.1` or `localhost`
  (any port suffix). Any other `Host` → **421** (T7 DNS-rebinding protection, same
  check as the `/api/*` adapter of `docs/contracts/ipc.md` §1.1).

## 3. Read-only guarantee

- HTTP: **GET only**. Any non-GET request on `/preview*` → **405** (`Allow: GET`).
  Unknown paths (including `/` and `/api/*`) → **404**.
- WS: only `ping`, `get`, `subscribe`, `unsubscribe` are accepted. Any other op —
  in particular every mutating op (`set`, `emit`, `bind`, `unbind`, `plugin`, `exec`,
  `stream-*`, `daemon`, `token`, `auth`) — is answered with an
  `{"op":"error","error":{"code":"E_SCOPE",…}}` frame and is **never executed**.
  Unknown op → `E_SCHEMA` error frame.
- The surface never issues `set`/`emit`/`plugin`/`exec` itself and never sends HID
  writes: a preview session performs **zero state mutations** (`rev` unchanged) and
  `ds-vdevctl writes` shows no device-bound reports during it (conformance:
  `tests/conformance/preview/`).
- Request head ≤ 16 KiB; one WS frame ≤ 1 MiB; responses close the HTTP connection
  (`Connection: close`).

## 4. WS protocol (`schemas/preview.json` `$defs`)

Client→server (one JSON object per text frame):
`{"op":"ping","id"?}`, `{"op":"get","id"?,"path":"/…"}`,
`{"op":"subscribe","id"?,"path":"/…"}`, `{"op":"unsubscribe","id"?,"path":"/…"}`.
`path` uses the ordinary subscribe grammar (`docs/contracts/ipc.md` §2): a state-path
pattern (`/controls/*`) or a topic pattern (`/deck/…`, `/app/…`, …).

Server→client: `preview.hello {op, protocol:1, rev, server, read_only:true}`,
`preview.snapshot {op, rev, state, actions}`, `preview.result {op, id?, …}`
(pong/value/subscribed/unsubscribed), `event {op:"event", path, rev?, value?|topic?,
payload?}` (the historical event shape, `docs/contracts/ipc.md` §2), and
`error {op:"error", id?, error:{code, message}}`.

## 5. Enablement (default-off)

- **Default-off.** No listener and no `http.port` unless enabled.
- Enable at boot with the `ds-daemon run --preview` flag **or** by setting
  `/policy/preview=true` before the daemon starts (admin scope, like all of
  `/policy`; `schemas/state.json`).
- Disable live at any time: `ds set /policy/preview false` — every subsequent
  `/preview` request answers **404** and open preview WS connections are refused on
  their next message and closed.
- Production profiles ship with the surface off (no flag, `/policy/preview` absent or
  `false`).

## 6. Errors

HTTP status codes: `200` served, `400` malformed WS upgrade, `401` bad/missing token,
`404` unknown path or surface disabled, `405` non-GET on the surface, `421`
non-loopback Host. WS errors use the control-plane codes
(`E_AUTH`/`E_SCOPE`/`E_PATH`/`E_SCHEMA`/`E_LIMIT`/`E_INTERNAL`, `ipc.md` §4) in the
`error` frame.

## 7. Tests

- Unit: `cargo test -p ds-daemon` (`preview::tests`, `actions_meta::tests`).
- Black-box on vdev (no hardware): `python3 tests/conformance/run.py -k preview`
  (`tests/conformance/preview/`): default-off, HTML + bearer + T7 Host refusal,
  action metadata listing the demo plugin's actions after install, WS live state
  streaming, POST → 405, mutating WS op → `E_SCOPE` with `rev` unchanged, and
  `ds-vdevctl writes` unchanged across a preview session.
- Security mapping: `docs/20-security.md` row T7 points at this suite.
