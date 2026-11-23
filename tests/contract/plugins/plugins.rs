//! Plugin wire-protocol contract tests (ticket DS-051).
//!
//! These run the SDK's example plugin binary (`crates/ds-plugin-sdk/src/bin/demo_plugin.rs`)
//! against a **mock host** — no daemon needed — and assert the wire messages **byte-for-byte**
//! for: handshake (`hello` → `register`), event dispatch, invoke → effects (result), invoke →
//! error, request/reply (`get`/`set_state` and host-mediated `net`), malformed-input tolerance,
//! and shutdown (explicit op and EOF).
//!
//! The pinned shapes are the ones documented in `docs/contracts/plugins.md` §1; where the
//! contract leaves the shape open (invoke error reply, shutdown op, reply payloads for
//! `get`/`set`), the choices made by the SDK are pinned here and documented in the crate
//! README (see "Contract notes / ambiguities" there). When DS-050 lands the host side, both
//! sides must keep this corpus green.
//!
//! Run with: `cargo test -p ds-plugin-sdk --test plugins` (or `cargo test --test plugins` from
//! the workspace root). The target is wired in `crates/ds-plugin-sdk/Cargo.toml` via a
//! `[[test]] path` pointing here, because the workspace only auto-discovers targets inside
//! member crates and this corpus lives under `tests/contract/plugins/` per the ticket.

mod mock_host;

use mock_host::{expected_register_line, hello_line, MockHost};

/// Full scripted session: handshake, register, event, invoke→effects, invoke→request/reply
/// (net), invoke→error (unknown action + params), malformed input, explicit shutdown.
#[test]
fn demo_plugin_session_byte_for_byte() {
    let mut host = MockHost::spawn();

    // 1. handshake: host says hello, plugin must answer register with its manifest.
    host.send(&hello_line());
    let register = host.recv();
    assert_eq!(register, expected_register_line(), "register wire message");

    // 2. event dispatch: keyDown (no request_id) → effects without request_id.
    host.send(r#"{"op":"event","event":"keyDown","control":"keys.0","state":{"rev":1}}"#);
    assert_eq!(
        host.recv(),
        concat!(
            r#"{"op":"effects","effects":[{"effect":"log","level":"info","message":"saw event keyDown on Some(\"keys.0\")"}]}"#,
            "\n"
        )
    );

    // 3. invoke demo.bump {"by":2} → two set effects, request_id echoed.
    host.send(r#"{"op":"invoke","id":"demo.bump","args":{"by":2},"request_id":7}"#);
    assert_eq!(
        host.recv(),
        concat!(
            r##"{"op":"effects","request_id":7,"effects":[{"effect":"set","path":"/controls/keys.0/label","value":"2"},{"effect":"set","path":"/controls/keys.0/image","value":{"kind":"color","value":"#ff8800"}}]}"##,
            "\n"
        )
    );

    // 4. invoke again: the counter accumulates (2 + 3 = 5).
    host.send(r#"{"op":"invoke","id":"demo.bump","args":{"by":3},"request_id":8}"#);
    assert_eq!(
        host.recv(),
        concat!(
            r##"{"op":"effects","request_id":8,"effects":[{"effect":"set","path":"/controls/keys.0/label","value":"5"},{"effect":"set","path":"/controls/keys.0/image","value":{"kind":"color","value":"#ff8800"}}]}"##,
            "\n"
        )
    );

    // 5. invoke demo.get_lamp: the plugin issues request kind:"net" (host-mediated HTTP, S4)
    //    with its own request_id, waits for the reply, then returns the set effect.
    host.send(r#"{"op":"invoke","id":"demo.get_lamp","args":{},"request_id":9}"#);
    assert_eq!(
        host.recv(),
        concat!(
            r#"{"op":"request","request_id":1,"kind":"net","method":"GET","url":"http://lights.lan/api/lamp"}"#,
            "\n"
        )
    );
    host.send(
        r#"{"op":"reply","request_id":1,"ok":true,"status":200,"body":"{\"state\":\"on\"}"}"#,
    );
    assert_eq!(
        host.recv(),
        concat!(
            r#"{"op":"effects","request_id":9,"effects":[{"effect":"set","path":"/plugins/demo/state/last","value":"{\"state\":\"on\"}"}]}"#,
            "\n"
        )
    );

    // 6. invoke with an unknown action → error reply with E_SCHEMA.
    host.send(r#"{"op":"invoke","id":"demo.nope","args":{},"request_id":10}"#);
    assert_eq!(
        host.recv(),
        concat!(
            r#"{"op":"error","request_id":10,"error":{"code":"E_SCHEMA","message":"unknown action: demo.nope"}}"#,
            "\n"
        )
    );

    // 7. invoke with args violating the declared params schema → E_SCHEMA error reply.
    host.send(r#"{"op":"invoke","id":"demo.bump","args":{"by":"x"},"request_id":11}"#);
    let err = host.recv();
    assert!(
        err.starts_with(
            r#"{"op":"error","request_id":11,"error":{"code":"E_SCHEMA","message":"args at $.by: expected type "#
        ),
        "params error reply: {err}"
    );

    // 8. malformed lines are skipped without killing the session.
    host.send("this is not json");
    host.send(r#"{"op":"wat"}"#);
    host.send(r#"{"op":"invoke","id":"demo.bump","args":{"by":1},"request_id":12}"#);
    assert_eq!(
        host.recv(),
        concat!(
            r##"{"op":"effects","request_id":12,"effects":[{"effect":"set","path":"/controls/keys.0/label","value":"6"},{"effect":"set","path":"/controls/keys.0/image","value":{"kind":"color","value":"#ff8800"}}]}"##,
            "\n"
        )
    );

    // 9. explicit shutdown op → clean exit 0.
    host.send(r#"{"op":"shutdown"}"#);
    let status = host.wait_exit();
    assert!(
        status.success(),
        "plugin must exit 0 on shutdown op, got {status}"
    );
}

/// EOF on stdin (daemon exit closes the pipe) is a clean shutdown too: exit 0.
#[test]
fn eof_on_stdin_is_clean_shutdown() {
    let mut host = MockHost::spawn();
    host.send(&hello_line());
    assert_eq!(host.recv(), expected_register_line());
    host.expect_clean_exit_on_eof();
}

/// Protocol version mismatch: the SDK must surface it cleanly (no register, exit code 8 —
/// the E_VERSION mapping) instead of speaking a protocol it does not share.
#[test]
fn version_mismatch_exits_cleanly_without_register() {
    let mut host = MockHost::spawn();
    host.send(r#"{"op":"hello","protocol":99,"caps":[]}"#);
    let status = host.wait_exit();
    assert_eq!(
        status.code(),
        Some(8),
        "E_VERSION maps to exit code 8, got {status}"
    );
}

/// A first message that is not `hello` is a protocol violation: no wire output, nonzero exit.
#[test]
fn first_message_must_be_hello() {
    let mut host = MockHost::spawn();
    host.send(r#"{"op":"invoke","id":"demo.bump","args":{},"request_id":1}"#);
    let status = host.wait_exit();
    assert_eq!(
        status.code(),
        Some(1),
        "E_SCHEMA usage error maps to exit code 1, got {status}"
    );
}

/// Error reply for a host-mediated request the host refuses (capability): the handler gets
/// the error and can turn it into an error reply.
#[test]
fn net_request_capability_denial_maps_to_error_reply() {
    let mut host = MockHost::spawn();
    host.send(&hello_line());
    assert_eq!(host.recv(), expected_register_line());
    host.send(r#"{"op":"invoke","id":"demo.get_lamp","args":{},"request_id":20}"#);
    assert_eq!(
        host.recv(),
        concat!(
            r#"{"op":"request","request_id":1,"kind":"net","method":"GET","url":"http://lights.lan/api/lamp"}"#,
            "\n"
        )
    );
    host.send(r#"{"op":"reply","request_id":1,"ok":false,"error":{"code":"E_CAP","message":"net:lights.lan not granted"}}"#);
    assert_eq!(
        host.recv(),
        concat!(
            r#"{"op":"error","request_id":20,"error":{"code":"E_CAP","message":"net:lights.lan not granted"}}"#,
            "\n"
        )
    );
    host.send(r#"{"op":"shutdown"}"#);
    assert!(host.wait_exit().success());
}
