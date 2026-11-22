# Component: ds-mcp (MCP bridge — external binary, stdio JSON-RPC 2.0)

Gives coding agents the deck as MCP tools over the same API (BRIEF goal; precedent:
`hubdeck --mcp` stdio JSON-RPC, `upstream/app/src-tauri/src/engine/mcp.rs:108-147`).

- **Language:** Rust. Why: one small binary, no UI deps; Python would also work but the
  CLI is Rust so the bridge shares `ds-client`. Rejected: stdio-only assumption folded into
  the daemon (kept external so the daemon has no agent-specific surface — R4).
- **Public surface:** `ds-mcp` on stdio; tools (thin wrappers over CLI/API calls):
  `deck_status, deck_get, deck_set, deck_bind, deck_emit, deck_watch (server→client
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
   "arguments":{"path":"/controls/keys/0/color","value":"#3cc86e"}}}
  ```
