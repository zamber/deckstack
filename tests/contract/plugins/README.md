# tests/contract/plugins — plugin wire-protocol contract tests

Ticket DS-051. These tests run the SDK's example plugin binary
(`crates/ds-plugin-sdk/src/bin/demo_plugin.rs`, built as `demo-plugin`) against a **mock host**
(`mock_host.rs`) that speaks the newline-JSON plugin protocol of `docs/contracts/plugins.md`
§1 over the child's real stdin/stdout — no daemon needed — and assert the wire messages
**byte-for-byte**:

- `demo_plugin_session_byte_for_byte` — handshake (`hello` → `register` with the manifest),
  event dispatch, invoke → `effects` (result), invoke → `request kind:"net"` → `reply` →
  `effects`, invoke → `error` (unknown action, params violation), malformed-input tolerance,
  explicit `{"op":"shutdown"}` → exit 0.
- `eof_on_stdin_is_clean_shutdown` — EOF on stdin → exit 0.
- `version_mismatch_exits_cleanly_without_register` — `protocol:99` → no output, exit 8.
- `first_message_must_be_hello` — non-hello first message → no output, exit 1.
- `net_request_capability_denial_maps_to_error_reply` — host `E_CAP` reply → invoke error reply.

The target is wired into the crate with a `[[test]]` entry in
`crates/ds-plugin-sdk/Cargo.toml` (`path = "../../tests/contract/plugins/plugins.rs"`),
because the workspace only auto-discovers test targets inside member crates while this corpus
lives under `tests/contract/` per the ticket's touches. Run it with either of:

```sh
cargo test -p ds-plugin-sdk --test plugins
cargo test --test plugins          # from the workspace root
```

When the DS-050 host lands, both sides must keep this corpus green; the shapes the contract
leaves open (error reply op, shutdown op, get/set reply payloads, event `request_id`
echoing, plugin-side request-id numbering) are decided by the SDK and listed in
`crates/ds-plugin-sdk/README.md` → "Contract notes / ambiguities".
