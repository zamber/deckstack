# Component: ds-mcp (MCP bridge — external binary, stdio JSON-RPC 2.0)

Gives coding agents the deck as MCP tools over the same API (BRIEF goal; precedent:
`hubdeck --mcp` stdio JSON-RPC, `upstream/app/src-tauri/src/engine/mcp.rs:108-147`).

- **Language:** Rust. Why: one small binary, no UI deps; Python would also work but the
  CLI is Rust so the bridge shares `ds-client`. Rejected: stdio-only assumption folded into
  the daemon (kept external so the daemon has no agent-specific surface — R4).
- **Public surface:** `ds-mcp` on stdio; tools (thin wrappers over CLI/API calls):
  `deck_status, deck_get, deck_set, deck_bind, deck_unbind, deck_emit, deck_watch (server→client
  notifications), deck_stream_push, deck_plugin_list, deck_schema`.
  Each tool = one daemon request; auth via the same token file.
- **Invariants:** no tool mutates without the token (scope-checked daemon-side); tool
  results are JSON; `deck_watch` streams NDJSON events as MCP notifications; timeout 10 s
  per tool call.
- **Failure modes:** daemon down → tools return `E_UNREACHABLE` with the hint to run
  `ds daemon start`; malformed → JSON-RPC error -32602.
- **Dependencies:** ds-client, MCP stdio framing (JSON-RPC 2.0, hand-rolled — boring).
- **Test command:** `tests/conformance/mcp.py` spawns `ds-mcp`, performs initialize +
  `tools/list` + one `deck_set` against ds-vdev, asserts the state changed.
- **Minimum working example:**
  ```json
  {"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"deck_set",
   "arguments":{"path":"/controls/keys/0/image","value":{"kind":"color","value":"#3cc86e"}}}}
  ```

## 1. Binary, flags, transport (implemented in `bins/ds-mcp/`)

```
ds-mcp [--socket PATH] [--data-dir PATH] [--token-file PATH] [-h|--help] [--version]
```

- `--socket` wins over `--data-dir/daemon.sock`; both default to the ds-ipc default
  (`$XDG_RUNTIME_DIR/deckstack/daemon.sock`, fallback `~/.local/state/deckstack/daemon.sock`).
  The token defaults to `<socket dir>/token` (0600), overridable with `--token-file`.
- **Transport:** one JSON-RPC 2.0 message per line on stdin/stdout (NDJSON). stdout
  carries protocol messages only — no logging, ever (the conformance test fails on any
  non-JSON stdout line). Diagnostics and usage errors go to stderr.
- **Handshake:** `initialize` (request) → result with `protocolVersion`, `capabilities.tools`,
  `serverInfo {name: "ds-mcp", version}`, `instructions`; then the client sends the
  `notifications/initialized` notification (never answered). `ping` → `{}`.
- **Version assumptions (pinned):** the server always answers
  `protocolVersion: "2024-11-05"` (the stdio transport + JSON-RPC 2.0 subset this doc
  specifies: `initialize`, `ping`, `tools/list`, `tools/call`, and the `notifications/*`
  messages below). A client that does not support the answered revision must disconnect.
  Recorded per the DS-061 risk note (MCP spec drift).

## 2. Tool list (`tools/list`)

Exactly these ten tools, in this order; every tool has an `inputSchema` of
`type: "object"` so an agent can discover the whole surface from `tools/list` alone
(R1/R10):

| Tool | Daemon op (one control-socket request) | Mirrors CLI verb |
|---|---|---|
| `deck_status` | `daemon` `action=status` (+ socket/pid_file/client added locally) | `ds status` |
| `deck_get` | `get` `path` `fields?` `rev?` (rev mismatch → `E_CONFLICT` with `current_rev`) | `ds get` |
| `deck_set` | `set` with `ops:[{"op":"set","path","value"}]`, `if_rev?` | `ds set` |
| `deck_bind` | `bind` with `binding {id:"b0000", topic, steps, when?}` (steps validated against schemas/binding.json client-side) | `ds bind` |
| `deck_unbind` | `unbind` with exactly one of `binding` (bNNNN) / `topic` | `ds unbind` |
| `deck_emit` | `emit` `topic` `payload?` | `ds emit` |
| `deck_watch` | `subscribe` `path` `from_rev?` on a dedicated connection (see §4) | `ds watch` |
| `deck_stream_push` | control `stream-open` (best effort) + `DSctl stream-open` + one DSF1 frame on `stream.sock` (never a plain NDJSON op) | `ds stream push --file` |
| `deck_plugin_list` | `plugin` `action=list` | `ds plugin list` |
| `deck_schema` | `schema` `name` ∈ state, ipc, stream-frame, plugin-manifest, binding, action-metadata, preview | `ds schema` |

### Argument schemas (summary — the authoritative copy is each tool's `inputSchema`)

- `deck_status`, `deck_plugin_list`: `{}` (no arguments).
- `deck_get`: `{"path": string (required, starts with '/'), "fields": [string], "rev": integer≥0}`.
- `deck_set`: `{"path": string (required), "value": any JSON (required), "if_rev": integer≥0}`.
- `deck_bind`: `{"topic": string ^(deck\|app)/ (required), "do": [step object, …] ≥1 (required), "when": object}`.
- `deck_unbind`: exactly one of `{"binding": "bNNNN"}` / `{"topic": string}`.
- `deck_emit`: `{"topic": string ^(deck\|app)/ (required), "payload": any JSON}`.
- `deck_watch`: `{"path": string (required), "from_rev": integer≥0, "max_events": 1..100000}`.
- `deck_stream_push`: `{"target": keys | dial | key:0-14 (required), "file": path to a png/jpeg/gif ≤ 16 MiB (required), "credits": 1..1024}`.
- `deck_schema`: `{"name": enum (required)}`.

## 3. Results and error mapping

- **Success:** `tools/call` result is
  `{"content":[{"type":"text","text":<pretty JSON>}], "structuredContent": <the JSON result object>, "isError": false}`
  (`structuredContent` is present because every tool result is an object).
- **Daemon/client failure (tool error):** `{"content":[{"type":"text","text":"E_PATH: path not found: /x [exit 2]"}], "isError": true}`
  — the stable `E_*` code (`docs/contracts/ipc.md` §4) and the CLI exit code
  (`docs/contracts/cli.md` §3) are preserved in the text. In particular a daemon that
  is down answers `E_UNREACHABLE: daemon unreachable at <sock> … — start it with:
  ds daemon start [exit 6]` (actionable), and a per-call timeout (10 s) answers
  `E_INTERNAL: tool call … exceeded the 10-second per-call timeout [exit 8]`.
  On the phase-1 daemon, `deck_plugin_list` honestly answers `E_INTERNAL [exit 8]`
  (plugins land in a later ticket) — the bridge maps, it does not paper over.
- **Protocol errors (JSON-RPC, not tool errors):** malformed line → `-32700`
  (id null); message that is not a JSON-RPC 2.0 object → `-32600`; unknown method →
  `-32601`; unknown tool name or missing/invalid tool arguments → `-32602`.
  Unknown tools are errors, never crashes.
- **No shell, no eval:** the bridge never spawns processes; `deck_stream_push` reads
  the payload from a file path only, and binding steps are validated against the
  closed vocabulary of `schemas/binding.json`.

## 4. Streaming rule (`deck_watch`)

- `tools/call deck_watch` answers **immediately** (bounded response) with
  `structuredContent {"subscription": "wN", "path": …, "events": "notifications/deck/event",
  "closed": "notifications/deck/watch_closed"}`.
- Every daemon event on the subscription is then written as one MCP notification per
  line: `{"jsonrpc":"2.0","method":"notifications/deck/event","params":{"subscription":"wN","event":{…NDJSON event…}}}`.
- The stream ends with `{"jsonrpc":"2.0","method":"notifications/deck/watch_closed","params":{"subscription":"wN","reason":…}}`
  when: the client sends `notifications/cancelled` with the `tools/call` request id in
  `params.requestId` (the bridge sends `unsubscribe` and closes), `max_events` is
  reached, or the daemon closes/errors the subscription (reason carries the `E_*` code).
- Subscriber limits are the daemon's (`docs/contracts/ipc.md` §5: >1000 queued events →
  `E_LIMIT` close), so a notification storm cannot wedge the bridge.

## 5. Tests

- `cargo test -p ds-mcp` — framing, tool dispatch, argument validation, error mapping,
  plus an in-process mock-daemon integration suite (set/get/status/schema/watch/cancel/
  unreachable) over a real Unix socket.
- `python tests/conformance/mcp.py` — boots ds-vdev + ds-daemon, spawns the real
  `ds-mcp`, and asserts on the wire: initialize, tools/list (the ten tools),
  deck_status, deck_set/deck_get round-trip on `/controls/keys/0/image`,
  deck_schema state, unknown tool / bad params / malformed line error codes,
  deck_watch receiving an injected device event as a notification + cancel, and the
  daemon-down `E_UNREACHABLE` case. Every stdout line must parse as JSON. Prints
  `RESULT: OK`, exit 0.
