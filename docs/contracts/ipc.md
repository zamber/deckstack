# Contract: daemon IPC API

Normative schema: `schemas/ipc-message.json`. Design: ADR-003, D04. R1/R7/R8.

## 1. Transports

| Transport | Where | Framing |
|---|---|---|
| Unix domain socket | `$XDG_RUNTIME_DIR/deckstack/daemon.sock` (fallback `~/.local/state/deckstack/daemon.sock`); dir 0700 | NDJSON control on the main socket; binary frames on `<dir>/stream.sock` |
| Windows named pipe | `\\.\pipe\deckstack-<username>` (+ `…-stream`) | same two framings |
| WebSocket (opt-in adapter) | `ws://127.0.0.1:<port>/ws`, port in `~/.local/state/deckstack/http.port`; requires `Authorization: Bearer <token>` and `Host: 127.0.0.1\|localhost` (DNS-rebinding guard, precedent `hubdeck/src/hubdeck/gui/server.py:601`) | one JSON text frame per message; binary frames = stream |
| HTTP (opt-in adapter, same port) | `GET/POST /api/<op>` for simple request/response | JSON body, cap 1 MiB control |

The socket pair is created at daemon start; the CLI never guesses ports (both current apps
discover ephemeral ports via files — racy, `hubdeck/src/hubdeck/control.py:81-90`,
`upstream/.../engine/plugins.rs:336-358`; rejected in D04).

## 2. Framing

- **Control:** newline-delimited JSON. Client sends one line `hello`, one `auth`, then
  requests; each request has an `id` (client-chosen int) and gets exactly one response
  line with the same `id`. A connection may be reused (CLI uses one-shot by default;
  `ds watch` keeps it open). Max line: **1 MiB** (`MAX_REQUEST` precedent 64 KiB at
  `hubdeck/.../control.py:35` is too small for `set` with an inline image ref — 1 MiB).
- **Stream (binary):** `magic "DSF1" | u32 meta_len | meta JSON | u64 payload_len | payload`.
  Meta: `{target, seq, ts_ms, format: png|jpeg|gif, w, h, flags: [loop], crc32}`.
  Max payload 16 MB (`MAX_BODY` precedent `upstream/.../plugins.rs:266`).

## 3. Versioning

First message must be `{"op":"hello","protocol":1,"client":"ds/0.1.0"}`; daemon replies
`{"op":"welcome","protocol":1,"rev":N,"server":"ds-daemon/0.1.0","features":[...]}`.
Mismatch → `E_VERSION` and close. New optional fields are additive; removed fields are a
protocol bump. `protocol` is bumped only for breaking changes; schemas in `schemas/` are
versioned alongside (`"$id": "…/ipc-message.json/v1"`).

## 4. Auth

- Token file `~/.local/state/deckstack/token`, mode 0600, per-install, generated at first
  start (precedent: per-install UUID token `upstream/.../plugins.rs:341-344`; per-RUN
  tokens rejected — they break stateless CLI across restarts, D14).
- Client sends `{"op":"auth","token":"…","scope":"read|mutate|admin"}`; the daemon
  checks the token with a constant-time compare (precedent
  `hmac.compare_digest`, `hubdeck/.../control.py:67`) and grants the MINIMUM of
  requested and token scopes. Tokens: the install token has scope `admin`; `ds token
  issue --scope read` creates scoped tokens stored in `tokens.json` (0600).
- Failures: `E_AUTH` (exit 4). No token → only `hello`/`auth`/`ping` answer.

## 5. Operations and error model

Ops: `ping, get, set (transaction), subscribe, unsubscribe, emit, bind, unbind,
plugin {list|invoke|install|remove}, stream-open, daemon {status|stop|reload}, token
{list|issue|revoke}, schema {name}, test {frame-sha}`.

Response envelope: `{"id":…,"ok":true,"rev":N,"result":…}` or
`{"id":…,"ok":false,"error":{"code":"E_…","message":"…","path":…?}}`.

| Code | Meaning | CLI exit |
|---|---|---|
| `E_AUTH` | bad/missing token or scope | 4 |
| `E_PATH` | path does not exist / bad syntax | 2 |
| `E_SCHEMA` | value violates `schemas/state.json` | 1 |
| `E_CONFLICT` | `if_rev` mismatch (payload has `current_rev`) | 3 |
| `E_LIMIT` | size/rate/queue limit hit | 5 |
| `E_CAP` | capability not granted (plugin/exec/net/fs) | 4 |
| `E_UNREACHABLE` | daemon not running (client-side) | 6 |
| `E_DEVICE` | hardware error (incl. `Forbidden`) | 7 |
| `E_VERSION` | protocol mismatch | 8 |
| `E_INTERNAL` | bug | 8 |

## 6. Backpressure and limits

- Control: per-connection in-flight requests ≤ 64; request line ≤ 1 MiB; response ≤ 4 MiB
  (else paginate with `get --after`); idle connection closed after 300 s (except
  subscriptions, kept until client closes or `dropped` overflow > 1000 events → close with
  `E_LIMIT`).
- Stream: per-target queue 2 (1 in flight + 1 waiting); producer credits: daemon sends
  `{"op":"stream-credit","target":…,"credits":n}`; producer must not exceed credits +
  queue depth; excess → `E_LIMIT` close of the stream. Device ceilings (32 fps accepted /
  ~24 displayed / ~2.1 MB/s, `upstream/PROTOCOL.md:91-97,244,265`) are enforced by the
  writer thread pacing, not by blocking producers.
- Global: max 64 client connections; max 32 plugins; memory cap for frame queues
  256 MiB total (drop oldest waiting frames beyond it).

## 7. Full example session (Unix socket, NDJSON)

```sh
SOCK=$XDG_RUNTIME_DIR/deckstack/daemon.sock
TOKEN=$(cat ~/.local/state/deckstack/token)
{ printf '%s\n' \
  '{"id":0,"op":"hello","protocol":1,"client":"manual/1.0"}' \
  "{\"id\":1,\"op\":\"auth\",\"token\":\"$TOKEN\",\"scope\":\"admin\"}" \
  '{"id":2,"op":"get","path":"/meta"}' \
  '{"id":3,"op":"set","ops":[{"op":"set","path":"/controls/keys/0/image","value":{"kind":"color","value":"#3cc86e"}}],"if_rev":1}' \
  '{"id":4,"op":"subscribe","path":"/controls/keys/*"}' \
  '{"id":5,"op":"emit","topic":"app/demo/hello","payload":{"n":1}}' \
  ; sleep 0.2; } | socat - UNIX-CONNECT:$SOCK
```

Replies (abridged):

```json
{"id":0,"ok":true,"result":{"op":"welcome","protocol":1,"rev":1,"server":"ds-daemon/0.1.0"}}
{"id":1,"ok":true,"result":{"scope":"admin"}}
{"id":2,"ok":true,"rev":1,"result":{"version":1,"rev":1,"protocol":1}}
{"id":3,"ok":true,"rev":2}
{"id":4,"ok":true,"rev":2}
{"id":5,"ok":true,"rev":2}
{"op":"event","path":"/controls/keys/0/image","rev":2,"value":{"kind":"color","value":"#3cc86e"}}
```

Frame push over the stream socket (binary): meta
`{"target":"keys","seq":7,"ts_ms":1732000000000,"format":"png","w":480,"h":272,"crc32":…}`
+ PNG payload → daemon re-encodes to JPEG q80, chunks into `02 35` reports; stats at
`/streams/keys/stats`.
