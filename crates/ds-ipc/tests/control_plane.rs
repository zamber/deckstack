//! End-to-end tests of the control plane against a real Unix socket: an in-process
//! `ds_ipc::Server` with the `testing::MockHandler`, driven by a raw NDJSON peer.

use ds_ipc::auth::{Scope, TokenSet};
use ds_ipc::error::ErrorCode;
use ds_ipc::limits::ServerConfig;
use ds_ipc::server::Server;
use ds_ipc::testing::{issue_scoped, write_token_dir, MockHandler};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

struct Peer {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Peer {
    fn connect(path: &Path) -> Self {
        let writer = UnixStream::connect(path).expect("connect");
        let reader = BufReader::new(writer.try_clone().expect("clone"));
        Self { reader, writer }
    }

    fn send(&mut self, v: &Value) {
        let mut line = serde_json::to_vec(v).unwrap();
        line.push(b'\n');
        // A refused connection (at the server's connection cap) may EPIPE here; the
        // interesting signal is the missing response, so write errors are ignored.
        let _ = self.writer.write_all(&line);
        let _ = self.writer.flush();
    }

    fn send_raw(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    fn recv(&mut self) -> Option<Value> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => None,
            Ok(_) => Some(serde_json::from_str(line.trim()).expect("json line")),
            Err(_) => None,
        }
    }

    fn roundtrip(&mut self, v: &Value) -> Value {
        self.send(v);
        self.recv().expect("a response line")
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    socket: std::path::PathBuf,
    token_dir: std::path::PathBuf,
    install_token: String,
    _handle: std::thread::JoinHandle<()>,
}

fn fixture() -> Fixture {
    fixture_with(|_| {})
}

fn fixture_with(tweak: impl FnOnce(&mut ServerConfig)) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let token_dir = dir.path().join("tokens");
    let install_token = write_token_dir(&token_dir).unwrap();
    let socket = dir.path().join("run/daemon.sock");
    let mut config = ServerConfig::default();
    tweak(&mut config);
    let server = Server::bind(&socket, &token_dir, config).expect("bind");
    let handle = server.start(Arc::new(MockHandler::new())).expect("start");
    Fixture {
        _dir: dir,
        socket,
        token_dir,
        install_token,
        _handle: handle,
    }
}

fn hello(peer: &mut Peer, id: i64) -> Value {
    peer.roundtrip(&json!({"id": id, "op": "hello", "protocol": 1, "client": "test/0.1"}))
}

fn auth(peer: &mut Peer, id: i64, token: &str) -> Value {
    peer.roundtrip(&json!({"id": id, "op": "auth", "token": token}))
}

fn authed_peer(fx: &Fixture) -> Peer {
    let mut p = Peer::connect(&fx.socket);
    let w = hello(&mut p, 0);
    assert_eq!(w["ok"], true, "welcome: {w}");
    assert_eq!(w["result"]["op"], "welcome");
    assert_eq!(w["result"]["protocol"], 1);
    let a = auth(&mut p, 1, &fx.install_token);
    assert_eq!(a["ok"], true, "auth: {a}");
    assert_eq!(a["result"]["scope"], "admin");
    p
}

#[test]
fn hello_welcome_and_ping_round_trip() {
    let fx = fixture();
    let mut p = Peer::connect(&fx.socket);
    let w = hello(&mut p, 0);
    assert_eq!(w["id"], 0);
    assert_eq!(w["ok"], true);
    assert_eq!(w["result"]["op"], "welcome");
    assert_eq!(
        w["result"]["server"],
        json!(format!("ds-daemon/{}", ds_ipc::version()))
    );
    let a = auth(&mut p, 1, &fx.install_token);
    assert_eq!(a["result"]["scope"], "admin");
    let pong = p.roundtrip(&json!({"id": 2, "op": "ping"}));
    assert_eq!(pong, json!({"id": 2, "ok": true, "result": {"pong": true}}));
}

#[test]
fn first_message_must_be_hello_and_protocol_must_match() {
    let fx = fixture();
    // not hello first
    let mut p = Peer::connect(&fx.socket);
    let r = p.roundtrip(&json!({"id": 0, "op": "ping"}));
    assert_eq!(r["ok"], false);
    assert_eq!(r["error"]["code"], "E_VERSION");
    // connection closed after E_VERSION
    assert!(p.recv().is_none());

    // wrong protocol
    let mut p = Peer::connect(&fx.socket);
    let r = p.roundtrip(&json!({"id": 0, "op": "hello", "protocol": 2, "client": "x"}));
    assert_eq!(r["error"]["code"], "E_VERSION");
    assert!(p.recv().is_none());
}

#[test]
fn ops_before_auth_get_e_auth() {
    let fx = fixture();
    let mut p = Peer::connect(&fx.socket);
    hello(&mut p, 0);
    let r = p.roundtrip(&json!({"id": 1, "op": "ping"}));
    assert_eq!(r["error"]["code"], "E_AUTH");
}

#[test]
fn bad_token_gets_e_auth() {
    let fx = fixture();
    let mut p = Peer::connect(&fx.socket);
    hello(&mut p, 0);
    let r = auth(&mut p, 1, &"0".repeat(64));
    assert_eq!(r["ok"], false);
    assert_eq!(r["error"]["code"], "E_AUTH");
    // still unauthenticated afterwards
    let r = p.roundtrip(&json!({"id": 2, "op": "ping"}));
    assert_eq!(r["error"]["code"], "E_AUTH");
}

#[test]
fn get_set_round_trip_with_rev_and_conflict() {
    let fx = fixture();
    let mut p = authed_peer(&fx);
    let set = p.roundtrip(&json!({
        "id": 2, "op": "set",
        "ops": [{"op": "set", "path": "/controls/keys/0/image", "value": {"kind": "color", "value": "#3cc86e"}}],
        "if_rev": 1,
    }));
    assert_eq!(set["ok"], true);
    assert_eq!(set["rev"], 2);
    let get = p.roundtrip(&json!({"id": 3, "op": "get", "path": "/controls/keys/0/image"}));
    assert_eq!(get["ok"], true);
    assert_eq!(get["result"]["value"], "#3cc86e");
    // if_rev mismatch → E_CONFLICT with current_rev
    let conflict = p.roundtrip(&json!({
        "id": 4, "op": "set",
        "ops": [{"op": "delete", "path": "/controls/keys/0/image"}],
        "if_rev": 99,
    }));
    assert_eq!(conflict["ok"], false);
    assert_eq!(conflict["error"]["code"], "E_CONFLICT");
    assert_eq!(conflict["error"]["current_rev"], 2);
    // unknown path → E_PATH
    let miss = p.roundtrip(&json!({"id": 5, "op": "get", "path": "/nope"}));
    assert_eq!(miss["error"]["code"], "E_PATH");
}

#[test]
fn unknown_op_is_a_schema_error_not_a_crash() {
    let fx = fixture();
    let mut p = authed_peer(&fx);
    let r = p.roundtrip(&json!({"id": 2, "op": "migrate", "path": "/x"}));
    assert_eq!(r["ok"], false);
    assert_eq!(r["error"]["code"], "E_SCHEMA");
    // the connection survives and keeps working
    let pong = p.roundtrip(&json!({"id": 3, "op": "ping"}));
    assert_eq!(pong["ok"], true);
}

#[test]
fn invalid_json_line_gets_id_less_or_id_ed_schema_error() {
    let fx = fixture();
    let mut p = authed_peer(&fx);
    p.send_raw(b"this is not json\n");
    let r = p.recv().unwrap();
    assert_eq!(r["ok"], false);
    assert_eq!(r["error"]["code"], "E_SCHEMA");
    assert!(r.get("id").is_none());
    // a valid line still works afterwards
    assert_eq!(p.roundtrip(&json!({"id": 9, "op": "ping"}))["ok"], true);
}

#[test]
fn oversized_line_gets_e_limit_and_closes() {
    let fx = fixture();
    let mut p = authed_peer(&fx);
    let mut line = b"{\"id\":1,\"op\":\"ping\",\"pad\":\"".to_vec();
    line.extend(std::iter::repeat_n(b'a', 2 * 1024 * 1024));
    line.extend(b"\"}\n");
    p.send_raw(&line);
    let r = p.recv().expect("E_LIMIT envelope");
    assert_eq!(r["ok"], false);
    assert_eq!(r["error"]["code"], "E_LIMIT");
    assert!(r.get("id").is_none());
    // connection is closed after the limit violation
    assert!(p.recv().is_none());
}

#[test]
fn scoped_tokens_enforce_e_scope() {
    let fx = fixture();
    let read_tok = issue_scoped(&fx.token_dir, "t0001", Scope::Read).unwrap();
    let mut p = Peer::connect(&fx.socket);
    hello(&mut p, 0);
    let a = auth(&mut p, 1, &read_tok);
    assert_eq!(a["result"]["scope"], "read");
    // read op ok
    assert_eq!(p.roundtrip(&json!({"id": 2, "op": "ping"}))["ok"], true);
    // mutate op → E_SCOPE
    let r = p.roundtrip(&json!({
        "id": 3, "op": "set", "ops": [{"op": "set", "path": "/a", "value": 1}]
    }));
    assert_eq!(r["ok"], false);
    assert_eq!(r["error"]["code"], "E_SCOPE");
    // requesting a higher scope than granted clamps to the token's scope (min)
    let mut p2 = Peer::connect(&fx.socket);
    hello(&mut p2, 0);
    let a = p2.roundtrip(&json!({"id": 1, "op": "auth", "token": read_tok, "scope": "admin"}));
    assert_eq!(a["result"]["scope"], "read");
}

#[test]
fn revoked_token_fails_new_connections_instantly() {
    let fx = fixture();
    let scoped = issue_scoped(&fx.token_dir, "t0002", Scope::Mutate).unwrap();
    // new connection with the scoped token works before revocation
    let mut p = Peer::connect(&fx.socket);
    hello(&mut p, 0);
    assert_eq!(auth(&mut p, 1, &scoped)["ok"], true);
    assert_eq!(p.roundtrip(&json!({"id": 2, "op": "ping"}))["ok"], true);

    // revoke (rotation) — instant for NEW connections (S12/M4)
    assert!(TokenSet::revoke(&fx.token_dir, "t0002").unwrap());
    let mut p2 = Peer::connect(&fx.socket);
    hello(&mut p2, 0);
    let r = auth(&mut p2, 1, &scoped);
    assert_eq!(r["ok"], false);
    assert_eq!(r["error"]["code"], "E_AUTH");
    // the install token still works
    assert_eq!(auth(&mut p2, 2, &fx.install_token)["ok"], true);
}

#[test]
fn revoked_token_fails_established_connection_after_grace() {
    // grace = 0: once rotated, new requests on the old connection are rejected too
    let fx = fixture_with(|c| c.revocation_grace = Duration::ZERO);
    let scoped = issue_scoped(&fx.token_dir, "t0003", Scope::Read).unwrap();
    let mut p = Peer::connect(&fx.socket);
    hello(&mut p, 0);
    assert_eq!(auth(&mut p, 1, &scoped)["ok"], true);
    assert_eq!(p.roundtrip(&json!({"id": 2, "op": "ping"}))["ok"], true);

    assert!(TokenSet::revoke(&fx.token_dir, "t0003").unwrap());
    let r = p.roundtrip(&json!({"id": 3, "op": "ping"}));
    assert_eq!(r["ok"], false);
    assert_eq!(r["error"]["code"], "E_AUTH");
    assert_eq!(r["error"]["message"], "token revoked");
}

#[test]
fn subscription_receives_events_on_set() {
    let fx = fixture();
    let mut watcher = authed_peer(&fx);
    let sub = watcher.roundtrip(&json!({"id": 2, "op": "subscribe", "path": "/controls/*"}));
    assert_eq!(sub["ok"], true);

    let mut writer = authed_peer(&fx);
    let set = writer.roundtrip(&json!({
        "id": 2, "op": "set",
        "ops": [{"op": "set", "path": "/controls/keys/0/image", "value": {"kind": "color", "value": "#3cc86e"}}]
    }));
    assert_eq!(set["ok"], true);

    let ev = watcher.recv().expect("event");
    assert_eq!(ev["op"], "event");
    assert_eq!(ev["path"], "/controls/keys/0/image");
    assert_eq!(ev["value"]["value"], "#3cc86e");
    assert!(ev["rev"].is_number());
}

#[test]
fn connection_cap_is_enforced() {
    let fx = fixture_with(|c| c.max_connections = 2);
    let mut p1 = authed_peer(&fx);
    let mut p2 = authed_peer(&fx);
    // third connection is accepted then dropped at the cap
    let mut p3 = Peer::connect(&fx.socket);
    p3.send(&json!({"id": 0, "op": "hello", "protocol": 1}));
    assert!(p3.recv().is_none(), "connection over the cap gets EOF");
    // existing connections still work
    assert_eq!(p1.roundtrip(&json!({"id": 2, "op": "ping"}))["ok"], true);
    assert_eq!(p2.roundtrip(&json!({"id": 2, "op": "ping"}))["ok"], true);
}

#[test]
fn idle_connection_is_closed_after_timeout() {
    let fx = fixture_with(|c| c.idle_timeout = Some(Duration::from_millis(200)));
    let mut p = Peer::connect(&fx.socket);
    hello(&mut p, 0);
    // no further traffic: the server should close within ~2 s
    p.writer
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    assert!(p.recv().is_none(), "idle connection must be closed");
}

#[test]
fn socket_and_dir_permissions_are_0600_and_0700() {
    use std::os::unix::fs::PermissionsExt;
    let fx = fixture();
    let sock_mode = std::fs::metadata(&fx.socket).unwrap().permissions().mode();
    assert_eq!(sock_mode & 0o777, 0o600);
    let dir_mode = std::fs::metadata(fx.socket.parent().unwrap())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(dir_mode & 0o777, 0o700);
    let token_mode = std::fs::metadata(fx.token_dir.join("token"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(token_mode & 0o777, 0o600);
}

#[test]
fn stale_socket_file_is_replaced_but_live_one_refused() {
    let fx = fixture();
    // a second bind on the live socket fails
    let err = Server::bind(&fx.socket, &fx.token_dir, ServerConfig::default());
    assert!(err.is_err());
    // a stale file (no listener) is removed and rebound
    let dir = tempfile::tempdir().unwrap();
    let stale = dir.path().join("daemon.sock");
    std::fs::write(&stale, b"garbage").unwrap();
    let server = Server::bind(&stale, &fx.token_dir, ServerConfig::default()).unwrap();
    assert!(stale.exists());
    drop(server);
}

#[test]
fn fixture_corpus_validates_against_the_embedded_schema() {
    // The same corpus the fixture gate (tests/fixtures/validate.sh) checks with node.
    let corpus: Value =
        serde_json::from_str(include_str!("../../../tests/fixtures/ipc-examples.json")).unwrap();
    let messages = corpus["messages"].as_array().unwrap();
    assert!(messages.len() >= 30);
    for (i, m) in messages.iter().enumerate() {
        ds_ipc::validate_message(m).unwrap_or_else(|e| panic!("corpus[{i}]: {e}"));
        let op = m.get("op").and_then(Value::as_str);
        let is_request =
            m.get("ok").is_none() && op.is_some_and(|o| ds_ipc::message::REQUEST_OPS.contains(&o));
        if is_request {
            ds_ipc::validate_request(m).unwrap_or_else(|e| panic!("corpus[{i}] req: {e}"));
        }
    }
}

#[test]
fn error_code_exit_mapping_matches_contract() {
    assert_eq!(ErrorCode::EAuth.exit_code(), 4);
    assert_eq!(ErrorCode::EScope.exit_code(), 4);
    assert_eq!(ErrorCode::ECap.exit_code(), 4);
    assert_eq!(ErrorCode::EPath.exit_code(), 2);
    assert_eq!(ErrorCode::ESchema.exit_code(), 1);
    assert_eq!(ErrorCode::EConflict.exit_code(), 3);
    assert_eq!(ErrorCode::ELimit.exit_code(), 5);
    assert_eq!(ErrorCode::EUnreachable.exit_code(), 6);
    assert_eq!(ErrorCode::EDevice.exit_code(), 7);
    assert_eq!(ErrorCode::EVersion.exit_code(), 8);
    assert_eq!(ErrorCode::EInternal.exit_code(), 8);
}

#[test]
fn reader_handles_multiple_requests_on_one_connection() {
    let fx = fixture();
    let mut p = authed_peer(&fx);
    // pipelined: send three requests before reading
    p.send(&json!({"id": 10, "op": "ping"}));
    p.send(&json!({"id": 11, "op": "get", "path": "/meta"}));
    p.send(&json!({"id": 12, "op": "ping"}));
    let r1 = p.recv().unwrap();
    let r2 = p.recv().unwrap();
    let r3 = p.recv().unwrap();
    assert_eq!(r1["id"], 10);
    assert_eq!(r2["id"], 11);
    assert_eq!(r2["error"]["code"], "E_PATH");
    assert_eq!(r3["id"], 12);
}
