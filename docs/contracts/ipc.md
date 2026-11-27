# Contract: daemon IPC API

Normative schemas: `schemas/ipc-message.json` (NDJSON control), `schemas/stream-frame.json`
(binary DSF1 frames). Example corpus validated in CI: `tests/fixtures/ipc-examples.json`
via `sh tests/fixtures/validate.sh`. Design: ADR-003, D04. R1/R7/R8. Fixes B1/B2/S14.

## 1. Transports

### 1.1 Endpoints

| Transport | Where | Used for |
|---|---|---|
| Unix domain socket | `$XDG_RUNTIME_DIR/deckstack/daemon.sock` (fallback `~/.local/state/deckstack/daemon.sock`); dir 0700, socket 0600 | all NDJSON control ops |
| Frame stream socket | `<dir>/stream.sock` | DSF1 binary frames + `DSctl\n` NDJSON stream control |
| Windows named pipe | `\\.\pipe\deckstack-<user>` (+ `…-stream`), ACL below | same two planes |
| WebSocket (opt-in adapter) | `ws://127.0.0.1:<port>/ws`, port in `<dir>/http.port`; `Authorization: Bearer <token>` + `Host: 127.0.0.1\|localhost` (T7) | JSON text frames = control; binary frames = DSF1. **Generic adapter for agents/debug clients only — no UI in alpha** (scope delta 4; post-alpha note: a thin debug/web client could use this later) |
| HTTP (opt-in adapter, same port) | `GET/POST /api/<op>` only; `/` returns 404 | simple request/response, 1 MiB body cap |

When the DS-113 debug surface is enabled (`ds-daemon --preview` or `/policy/preview=true`
at boot; default-off), the same loopback adapter additionally serves the read-only
`/preview` debug routes (`GET /preview` HTML, `GET /preview/api/*` JSON,
`GET /preview/ws` WebSocket) — contract `docs/contracts/preview.md`, schemas
`schemas/preview.json` + `schemas/action-metadata.json`. `/preview` accepts **no
writes** (non-GET → 405) and never issues device/HID writes.

Windows pipe ACL (S5): the pipe is created with an explicit security descriptor
`D:P(A;;GA;;;SY)(A;;GA;;;<current-user-sid>)` (SYSTEM + owning user only, via
`CreateNamedPipe` `SECURITY_ATTRIBUTES` / `SetNamedSecurityInfo`); the token dir and
`token`/`tokens.json` files get the equivalent file ACL (user + SYSTEM, no inheritance of
broad ACEs). Test: a second local user's connect attempt fails with access denied
(tests/hw or a Windows CI job with two accounts).

### 1.2 Which verb uses which transport (B1/S14)

| CLI verb | Transport | Wire |
|---|---|---|
| `get set bind unbind emit watch events plugin daemon token schema` | control socket | NDJSON request/response (no `migrate` op exists — scope delta 2) |
| `stream stats` / `stream stop` | control socket | NDJSON ops `stream-stats` / `stream-stop` |
| `stream push` | control socket **then** stream socket | `stream-open {target, credits}` (NDJSON) authorizes; frames are DSF1 binary on stream.sock; acks are `DSctl\n` NDJSON `stream-ack`/`stream-nack`/`stream-credit` |

`ds stream push` is therefore **not** an NDJSON op — a coder must not implement it as one.

## 2. Framing

- **Control:** newline-delimited JSON, one request line (`id` + `op`) → one response line
  with the same `id`. Reusable connection; CLI one-shot by default, `ds watch` persistent.
  Max line 1 MiB. A line that exceeds the cap is answered with an id-less
  `{"ok":false,"error":{"code":"E_LIMIT",…}}` and the connection is closed (the framing
  can no longer be trusted); a line that is not exactly one schema-valid request object is
  answered with an id-less (or id-carrying, if recoverable) `E_SCHEMA` envelope and the
  connection stays open. The server also caps responses at 4 MiB (a larger handler reply
  is replaced by `E_LIMIT`).
- **DSF1 binary frame** (B2, schema `stream-frame.json`): `"DSF1" | u32 LE meta_len |
  UTF-8 JSON meta | u64 LE payload_len | payload`. Meta: `{target, seq, ts_ms, format:
  png|jpeg|gif, w, h, frames, fps, loop, crc32, max_payload_bytes: 16777216}`.
  Max payload 16 MB (`MAX_BODY` precedent `upstream/.../plugins.rs:266`). Stream-socket
  control lines are prefixed `DSctl\n` so the two framings never mix.
- **Versioning:** first message `{"op":"hello","protocol":1,…}` →
  `{"op":"welcome","protocol":1,"rev":N,…}`; mismatch → `E_VERSION`. Additive-only
  changes; breaking changes bump `protocol`; schemas carry `/vN` `$id`s. The schema types
  `hello.protocol` as a plain integer (not `const 1`) so a mismatched hello can be parsed
  and answered with `E_VERSION` rather than a schema error.

## 3. Auth and scopes

- Per-install token file `<dir>/token`, 0600, generated at first start (per-run tokens
  rejected — they break stateless CLI across restarts, D14). `auth` checks the token
  constant-time (`hmac.compare_digest` precedent, `hubdeck/.../control.py:67`) and grants
  min(requested, granted). Install token = `admin`; `ds token issue --scope read|mutate`
  mints scoped tokens in `tokens.json` (0600).
- `E_AUTH` = bad/missing token (exit 4). `E_SCOPE` (m4) = token valid but scope below the
  request's requirement (exit 4). `E_CAP` = capability grant missing (plugins/exec/net/fs).
- **Per-op scope requirement** (enforced by the server before dispatch; `hello`/`auth`
  themselves need none):

  | Scope | Ops |
  |---|---|
  | `read` | `ping`, `get`, `subscribe`, `unsubscribe`, `stream-stats`, `schema`, `plugin list`, `daemon status`, `token list` |
  | `mutate` | `set`, `emit`, `bind`, `unbind`, `stream-open`, `stream-stop`, `plugin invoke|install|remove` |
  | `admin` | `daemon stop|reload`, `token issue|revoke` (and any op unknown to the scope table — fail closed) |

- **Field names:** the envelope's `id` is always the integer correlation id, so ops that
  reference another object by id use their own field: `unbind` takes `binding` (or
  `topic`), `plugin` takes `plugin`, `token revoke` takes `token_id`
  (`schemas/ipc-message.json`).
- **Rotation (S12/M4):** `ds token revoke <id>` / rotate is **instant for new connections**:
  the auth check reads the current token set per connection. A short grace (≤ 5 s) applies
  only to requests already in flight on an established connection, tracked by a connection
  generation counter bumped on rotation. No 60 s window for new requests. Test:
  rotate, then a NEW connection with the old token → `E_AUTH`; an in-flight long
  subscription on the old connection completes its current event batch then closes.

## 4. Operations and error model

Ops: `ping, hello, auth, get, set, subscribe, unsubscribe, emit, bind, unbind, plugin,
stream-open, stream-stats, stream-stop, daemon, token, schema`. Response envelope:
`{"id":…,"ok":true,"rev":N,"result":…}` or `{"id":…,"ok":false,"error":{…}}`.

| Code | Meaning | CLI exit |
|---|---|---|
| `E_AUTH` | bad/missing token | 4 |
| `E_SCOPE` | valid token, insufficient scope | 4 |
| `E_PATH` | path not found / bad syntax | 2 |
| `E_SCHEMA` | value violates schema | 1 |
| `E_CONFLICT` | `if_rev` mismatch (`current_rev` in payload) | 3 |
| `E_LIMIT` | size/rate/queue limit | 5 |
| `E_CAP` | capability not granted | 4 |
| `E_UNREACHABLE` | daemon not running (client-side) | 6 |
| `E_DEVICE` | hardware error (incl. `Forbidden`) | 7 |
| `E_VERSION` | protocol mismatch | 8 |
| `E_INTERNAL` | bug | 8 |

The request/response split is schema-enforced: requests never carry `ok`, responses always
do (m3 discriminator in `ipc-message.json`). Every line in both directions is validated
against `schemas/ipc-message.json` before dispatch/return; an unknown op therefore fails
validation and is answered with `E_SCHEMA` — the usage error (exit 1); the CLI exit-code
mapping lands with DS-016.

## 5. Backpressure and limits

- Control: ≤ 64 in-flight requests/connection; request line ≤ 1 MiB; response ≤ 4 MiB;
  idle connection closed after 300 s (subscriptions exempt; > 1000 queued events →
  `E_LIMIT` close).
- Stream: per-target queue 2 (1 in flight + 1 waiting); producer must respect
  `stream-credit` budgets; excess → `stream-nack` + `E_LIMIT`. Device ceilings
  (32 fps accepted / ~24 displayed / ~2.1 MB/s, `upstream/PROTOCOL.md:91-97,244,265`)
  are enforced by writer-thread pacing, never by blocking producers or the control plane
  (S16/M8: in-flight frames are never dropped mid-send — see ADR-005).
- Global: ≤ 64 connections, ≤ 32 plugins, frame-queue memory ≤ 256 MiB, telemetry
  sampling ≤ 1 Hz (S1).

## 6. Full example session (Unix socket, NDJSON)

```sh
SOCK=$XDG_RUNTIME_DIR/deckstack/daemon.sock
TOKEN=$(cat ~/.local/state/deckstack/token)
{ printf '%s\n' \
  '{"id":0,"op":"hello","protocol":1,"client":"manual/1.0"}' \
  "{\"id\":1,\"op\":\"auth\",\"token\":\"$TOKEN\",\"scope\":\"admin\"}" \
  '{"id":2,"op":"get","path":"/meta"}' \
  '{"id":3,"op":"set","ops":[{"op":"set","path":"/controls/keys/0/image","value":{"kind":"color","value":"#3cc86e"}}],"if_rev":1}' \
  '{"id":4,"op":"subscribe","path":"/controls/keys/*"}' \
  '{"id":5,"op":"stream-open","target":"keys","credits":8}' \
  '{"id":6,"op":"stream-stats","target":"keys"}' \
  ; sleep 0.2; } | socat - UNIX-CONNECT:$SOCK
# then on stream.sock: DSF1 frame with meta {"target":"keys","seq":7,"format":"png","w":480,"h":272,"crc32":…} + PNG payload
```

Replies (abridged; full corpus in `tests/fixtures/ipc-examples.json`):

```json
{"id":0,"ok":true,"result":{"op":"welcome","protocol":1,"rev":1,"server":"ds-daemon/0.1.0"}}
{"id":1,"ok":true,"result":{"scope":"admin"}}
{"id":3,"ok":true,"rev":2}
{"id":5,"ok":true,"result":{"target":"keys","credits":8}}
{"op":"event","path":"/controls/keys/0/image","rev":2,"value":{"kind":"color","value":"#3cc86e"}}
{"op":"stream-ack","target":"keys","seq":7}
```
