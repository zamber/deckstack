//! `ds-client` against a mock in-process server (`ds_ipc::Server` +
//! `ds_ipc::testing::MockHandler` on a tmp socket): hello/auth, one-shot ops, the
//! watch subscription, and every client-side error path.

use ds_client::{Client, ClientError};
use ds_ipc::auth::{Scope, TokenSet};
use ds_ipc::error::ErrorCode;
use ds_ipc::limits::ServerConfig;
use ds_ipc::server::Server;
use ds_ipc::testing::{issue_scoped, write_token_dir, MockHandler};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

struct Mock {
    _dir: tempfile::TempDir,
    socket: PathBuf,
    token_dir: PathBuf,
}

fn mock() -> Mock {
    let dir = tempfile::tempdir().unwrap();
    let token_dir = dir.path().join("tokens");
    write_token_dir(&token_dir).unwrap();
    let socket = dir.path().join("run/daemon.sock");
    let server = Server::bind(&socket, &token_dir, ServerConfig::default()).unwrap();
    server.start(Arc::new(MockHandler::new())).unwrap();
    Mock {
        _dir: dir,
        socket,
        token_dir,
    }
}

fn connected(m: &Mock) -> Client {
    let mut c = Client::connect(&m.socket).unwrap();
    let w = c.hello("ds-client-test/0.1").unwrap();
    assert_eq!(w["op"], "welcome");
    assert_eq!(w["protocol"], 1);
    c
}

fn authed(m: &Mock) -> Client {
    let mut c = connected(m);
    // production layout keeps `token` next to `daemon.sock`; the test splits the dirs,
    // so pass the token file explicitly
    let scope = c
        .authenticate_token_file(Some(&m.token_dir.join("token")))
        .unwrap();
    assert_eq!(scope, Scope::Admin);
    c
}

#[test]
fn one_shot_hello_auth_ping_get_set() {
    let m = mock();
    let mut c = authed(&m);
    assert_eq!(c.ping().unwrap(), json!({"pong": true}));

    let resp = c
        .set(
            json!([{"op": "set", "path": "/controls/keys/0/image", "value": {"kind": "color", "value": "#3cc86e"}}]),
            Some(1),
        )
        .unwrap();
    assert_eq!(resp["ok"], true);
    assert_eq!(resp["rev"], 2);

    let got = c.get("/controls/keys/0/image", None).unwrap();
    assert_eq!(got["result"]["value"], "#3cc86e");

    // conflict carries current_rev
    let err = c
        .set(
            json!([{"op": "delete", "path": "/controls/keys/0/image"}]),
            Some(99),
        )
        .unwrap_err();
    match err {
        ClientError::Server(e) => {
            assert_eq!(e.code, ErrorCode::EConflict);
            assert_eq!(e.current_rev, Some(2));
            assert_eq!(e.code.exit_code(), 3);
        }
        other => panic!("expected E_CONFLICT, got {other:?}"),
    }
}

#[test]
fn watch_receives_events_until_max() {
    let m = mock();
    let mut watcher = authed(&m);
    let mut writer = authed(&m);

    // subscribe BEFORE the producer writes, so the events are not missed
    watcher.subscribe("/controls/*", None).unwrap();

    let events = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let events2 = Arc::clone(&events);
    let producer = thread::spawn(move || {
        for i in 0..5 {
            writer
                .set(
                    json!([{"op": "set", "path": format!("/controls/keys/{i}"), "value": i}]),
                    None,
                )
                .unwrap();
        }
    });
    let n = watcher
        .watch("/controls/*", None, Some(3), &mut |ev| {
            events2.lock().unwrap().push(ev.clone());
        })
        .unwrap();
    assert_eq!(n, 3);
    producer.join().unwrap();
    let evs = events.lock().unwrap();
    assert!(evs.iter().all(|e| e["op"] == "event"));
    assert!(evs[0]["path"]
        .as_str()
        .unwrap()
        .starts_with("/controls/keys/"));
}

#[test]
fn bad_token_is_e_auth_exit_4() {
    let m = mock();
    let mut c = connected(&m);
    let err = c.authenticate(&"f".repeat(64), None).unwrap_err();
    match err {
        ClientError::Server(e) => {
            assert_eq!(e.code, ErrorCode::EAuth);
            assert_eq!(e.code.exit_code(), 4);
        }
        other => panic!("expected E_AUTH, got {other:?}"),
    }
}

#[test]
fn scoped_token_gets_e_scope_on_mutation() {
    let m = mock();
    let read_tok = issue_scoped(&m.token_dir, "t0009", Scope::Read).unwrap();
    let mut c = connected(&m);
    let granted = c.authenticate(&read_tok, None).unwrap();
    assert_eq!(granted, Scope::Read);
    assert!(c.ping().is_ok());
    let err = c
        .set(json!([{"op": "set", "path": "/a", "value": 1}]), None)
        .unwrap_err();
    match err {
        ClientError::Server(e) => assert_eq!(e.code, ErrorCode::EScope),
        other => panic!("expected E_SCOPE, got {other:?}"),
    }
}

#[test]
fn revoked_token_is_rejected_on_new_connection() {
    let m = mock();
    let scoped = issue_scoped(&m.token_dir, "t0010", Scope::Read).unwrap();
    let mut c = connected(&m);
    assert_eq!(c.authenticate(&scoped, None).unwrap(), Scope::Read);
    drop(c);
    assert!(TokenSet::revoke(&m.token_dir, "t0010").unwrap());
    let mut c2 = connected(&m);
    let err = c2.authenticate(&scoped, None).unwrap_err();
    assert!(matches!(err, ClientError::Server(ref e) if e.code == ErrorCode::EAuth));
}

#[test]
fn missing_socket_is_e_unreachable_exit_6() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("no-daemon.sock");
    let err = match Client::connect(&sock) {
        Err(e) => e,
        Ok(_) => panic!("connect should fail"),
    };
    match &err {
        ClientError::Unreachable { path, hint, .. } => {
            assert_eq!(*path, sock);
            assert!(hint.contains("ds daemon start"));
        }
        other => panic!("expected Unreachable, got {other:?}"),
    }
    assert_eq!(err.code(), ErrorCode::EUnreachable);
    assert_eq!(err.exit_code(), 6);
    let env = err.to_envelope();
    assert_eq!(env["code"], "E_UNREACHABLE");
}

#[test]
fn oversized_request_is_rejected_client_side() {
    let m = mock();
    let mut c = authed(&m);
    let big = json!({"op": "emit", "topic": "app/x", "payload": "a".repeat(2 * 1024 * 1024)});
    let err = c.request(big).unwrap_err();
    match err {
        ClientError::Limit(_) => {}
        other => panic!("expected Limit, got {other:?}"),
    }
    assert_eq!(err.code(), ErrorCode::ELimit);
    // the connection still works afterwards
    assert_eq!(c.ping().unwrap(), json!({"pong": true}));
}

#[test]
fn schema_invalid_request_is_rejected_client_side() {
    let m = mock();
    let mut c = authed(&m);
    // unknown op fails client-side schema validation (server would answer E_SCHEMA too)
    let err = c
        .request(json!({"op": "migrate", "path": "/x"}))
        .unwrap_err();
    assert!(matches!(err, ClientError::Schema(_)));
    assert_eq!(err.code(), ErrorCode::ESchema);
    assert_eq!(err.exit_code(), 1);
    // a response-shaped message is not a request
    assert!(matches!(
        c.request(json!({"op": "ping", "ok": true})).unwrap_err(),
        ClientError::Schema(_)
    ));
}

#[test]
fn unreadable_token_file_is_e_auth() {
    let m = mock();
    let mut c = connected(&m);
    let err = c
        .authenticate_token_file(Some(&m.token_dir.join("missing")))
        .unwrap_err();
    assert!(matches!(err, ClientError::Server(ref e) if e.code == ErrorCode::EAuth));
    assert_eq!(err.exit_code(), 4);
}

#[test]
fn protocol_mismatch_surfaces_as_e_version() {
    let m = mock();
    let mut c = Client::connect(&m.socket).unwrap();
    let err = c
        .request(json!({"op": "hello", "protocol": 99, "client": "x"}))
        .unwrap_err();
    match &err {
        ClientError::Server(e) => assert_eq!(e.code, ErrorCode::EVersion),
        other => panic!("expected E_VERSION, got {other:?}"),
    }
    assert_eq!(err.exit_code(), 8);
}

#[test]
fn daemon_status_and_token_list_round_trip() {
    let m = mock();
    let mut c = authed(&m);
    let st = c
        .request(json!({"op": "daemon", "action": "status"}))
        .unwrap();
    assert_eq!(st["result"]["running"], true);
    let tl = c.request(json!({"op": "token", "action": "list"})).unwrap();
    assert_eq!(tl["ok"], true);
}
