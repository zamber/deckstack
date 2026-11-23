//! Integration tests (in-crate, like `bins/ds/src/stream/it.rs`): the `Bridge` is
//! driven line-by-line against an in-process mock daemon (`ds_ipc::Server` +
//! a handler shaped like the phase-1 `ds-daemon`) on a real Unix socket, so the
//! framing, tool dispatch, error mapping and the deck_watch notification stream are
//! all exercised end to end without the real daemon.

use crate::bridge::{Bridge, In};
use crate::conn::Config;
use ds_ipc::error::{ErrorCode, IpcError};
use ds_ipc::limits::ServerConfig;
use ds_ipc::server::{Connection, Handler, Server};
use ds_ipc::testing::write_token_dir;
use ds_ipc::{Request, Response};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A mock daemon with the phase-1 op surface and result shapes: get →
/// `{"path","value"}` (+rev), set → rev bump + one event per subscriber, bind/unbind →
/// ids, emit, daemon status, schema, and `plugin` → E_INTERNAL like the phase-1 daemon.
struct MockDaemon {
    state: Mutex<HashMap<String, Value>>,
    rev: Mutex<u64>,
    subs: Mutex<Vec<(Connection, String)>>,
    next_binding: Mutex<u64>,
}

impl MockDaemon {
    fn new() -> Self {
        Self {
            state: Mutex::new(HashMap::new()),
            rev: Mutex::new(1),
            subs: Mutex::new(Vec::new()),
            next_binding: Mutex::new(1),
        }
    }

    fn matches(sub: &str, path: &str) -> bool {
        match sub.strip_suffix('*') {
            Some(prefix) => path.starts_with(prefix),
            None => sub == path,
        }
    }
}

impl Handler for MockDaemon {
    fn rev(&self) -> u64 {
        *self.rev.lock().unwrap()
    }

    fn handle(&self, conn: &Connection, req: &Request) -> Response {
        let id = req.id();
        match req.op() {
            "ping" => Response::ok(id, json!({"pong": true})),
            "get" => {
                let Some(path) = req.get("path").and_then(Value::as_str) else {
                    return Response::error_code(id, ErrorCode::EPath, "get needs a path");
                };
                let state = self.state.lock().unwrap();
                match state.get(path) {
                    Some(v) => Response::ok_rev(
                        id,
                        self.rev(),
                        json!({"path": path, "value": v.clone()}),
                    ),
                    None => Response::error(
                        id,
                        IpcError::new(ErrorCode::EPath, format!("path not found: {path}"))
                            .with_path(path),
                    ),
                }
            }
            "set" => {
                let Some(ops) = req.get("ops").and_then(Value::as_array).cloned() else {
                    return Response::error_code(id, ErrorCode::ESchema, "set needs ops");
                };
                let mut state = self.state.lock().unwrap();
                for op in &ops {
                    if let (Some(p), Some(v)) = (
                        op.get("path").and_then(Value::as_str),
                        op.get("value").cloned(),
                    ) {
                        state.insert(p.to_string(), v);
                    }
                }
                drop(state);
                let mut rev = self.rev.lock().unwrap();
                *rev += 1;
                let new_rev = *rev;
                drop(rev);
                let subs = self.subs.lock().unwrap().clone();
                for (c, pattern) in &subs {
                    for (path, value) in self.state.lock().unwrap().iter() {
                        if Self::matches(pattern, path) {
                            let _ = c.send_event(&json!({
                                "op": "event",
                                "path": path,
                                "rev": new_rev,
                                "value": value,
                            }));
                        }
                    }
                }
                Response {
                    id,
                    ok: true,
                    rev: Some(new_rev),
                    result: None,
                    error: None,
                }
            }
            "emit" => {
                let topic = req.get("topic").and_then(Value::as_str).unwrap_or("");
                let payload = req.get("payload").cloned().unwrap_or(Value::Null);
                let n = self.subs.lock().unwrap().len();
                let _ = payload;
                Response::ok(id, json!({"emitted": topic, "subscribers": n}))
            }
            "bind" => {
                let mut next = self.next_binding.lock().unwrap();
                let bid = format!("b{:04}", *next);
                *next += 1;
                drop(next);
                let mut rev = self.rev.lock().unwrap();
                *rev += 1;
                let new_rev = *rev;
                drop(rev);
                Response::ok_rev(id, new_rev, json!({"id": bid}))
            }
            "unbind" => {
                let mut rev = self.rev.lock().unwrap();
                *rev += 1;
                let new_rev = *rev;
                drop(rev);
                let removed = match req.get("binding").and_then(Value::as_str) {
                    Some(b) => vec![b.to_string()],
                    None => vec![],
                };
                Response::ok_rev(id, new_rev, json!({"removed": removed}))
            }
            "subscribe" => {
                let Some(path) = req.get("path").and_then(Value::as_str) else {
                    return Response::error_code(id, ErrorCode::EPath, "subscribe needs a path");
                };
                conn.set_idle_exempt(true);
                self.subs
                    .lock()
                    .unwrap()
                    .push((conn.clone(), path.to_string()));
                Response::ok(id, json!({"subscribed": path}))
            }
            "unsubscribe" => {
                let Some(path) = req.get("path").and_then(Value::as_str) else {
                    return Response::error_code(id, ErrorCode::EPath, "unsubscribe needs a path");
                };
                self.subs.lock().unwrap().retain(|(_, p)| p != path);
                Response::ok(id, json!({"unsubscribed": path}))
            }
            "daemon" => Response::ok(
                id,
                json!({
                    "running": true,
                    "rev": self.rev(),
                    "pid": std::process::id(),
                    "server": "mock-daemon/0",
                    "device": "virtual",
                    "protocol": ds_ipc::PROTOCOL_VERSION,
                }),
            ),
            "schema" => Response::ok(
                id,
                json!({"$id": "https://deckstack.dev/schemas/state.json/v1", "type": "object"}),
            ),
            // like the phase-1 daemon: plugin lands in a later ticket
            "plugin" => Response::error_code(
                id,
                ErrorCode::EInternal,
                "op 'plugin' is not served by this phase-1 daemon (plugins/streams/token land in later tickets)",
            ),
            other => Response::error(
                id,
                IpcError::new(ErrorCode::EPath, format!("mock: no such op {other}")).with_path(other),
            ),
        }
    }
}

struct Mock {
    _dir: tempfile::TempDir,
    cfg: Config,
}

fn mock() -> Mock {
    let dir = tempfile::tempdir().unwrap();
    let token_dir = dir.path().join("tokens");
    write_token_dir(&token_dir).unwrap();
    let socket = dir.path().join("run/daemon.sock");
    let server = Server::bind(&socket, &token_dir, ServerConfig::default()).unwrap();
    server.start(Arc::new(MockDaemon::new())).unwrap();
    let cfg = Config::new(socket, Some(token_dir.join("token")));
    Mock { _dir: dir, cfg }
}

/// A bridge whose watch-event sender is wired to the returned receiver.
fn harness(m: &Mock) -> (Bridge, Receiver<In>) {
    let (tx, rx) = std::sync::mpsc::channel();
    (Bridge::new(m.cfg.clone(), tx), rx)
}

fn call(b: &mut Bridge, id: i64, name: &str, args: Value) -> Value {
    let line = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {"name": name, "arguments": args},
    });
    let out = b.handle_line(&line.to_string(), false);
    assert_eq!(out.len(), 1, "one response per request");
    out.into_iter().next().unwrap()
}

#[test]
fn full_session_against_the_mock_daemon() {
    let m = mock();
    let (mut b, _rx) = harness(&m);

    // initialize + initialized notification
    let out = b.handle_line(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
        false,
    );
    assert_eq!(out[0]["result"]["serverInfo"]["name"], json!("ds-mcp"));
    assert!(b
        .handle_line(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            false
        )
        .is_empty());

    // tools/list: ten tools
    let out = b.handle_line(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#, false);
    assert_eq!(out[0]["result"]["tools"].as_array().unwrap().len(), 10);

    // deck_status
    let r = call(&mut b, 3, "deck_status", json!({}));
    assert_eq!(r["result"]["isError"], json!(false));
    assert_eq!(r["result"]["structuredContent"]["running"], json!(true));
    assert!(r["result"]["structuredContent"]["socket"]
        .as_str()
        .unwrap()
        .ends_with("daemon.sock"));

    // deck_set a color on /controls/keys/0/image
    let r = call(
        &mut b,
        4,
        "deck_set",
        json!({"path": "/controls/keys/0/image", "value": {"kind": "color", "value": "#3cc86e"}}),
    );
    assert_eq!(r["result"]["isError"], json!(false), "{r}");
    assert_eq!(r["result"]["structuredContent"]["ok"], json!(true));
    assert!(r["result"]["structuredContent"]["rev"].as_u64().unwrap() >= 2);

    // deck_get reads it back
    let r = call(
        &mut b,
        5,
        "deck_get",
        json!({"path": "/controls/keys/0/image"}),
    );
    let sc = &r["result"]["structuredContent"];
    assert_eq!(sc["path"], json!("/controls/keys/0/image"));
    assert_eq!(sc["value"], json!({"kind": "color", "value": "#3cc86e"}));
    assert!(sc["rev"].is_u64());

    // deck_get with a wrong rev -> E_CONFLICT tool error, exit 3
    let r = call(
        &mut b,
        6,
        "deck_get",
        json!({"path": "/controls/keys/0/image", "rev": 999}),
    );
    assert_eq!(r["result"]["isError"], json!(true));
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("E_CONFLICT"), "{text}");
    assert!(text.contains("[exit 3]"), "{text}");

    // deck_get on an unknown path -> E_PATH, exit 2
    let r = call(&mut b, 7, "deck_get", json!({"path": "/nope"}));
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("E_PATH"), "{text}");
    assert!(text.contains("[exit 2]"), "{text}");

    // deck_schema state
    let r = call(&mut b, 8, "deck_schema", json!({"name": "state"}));
    assert_eq!(r["result"]["isError"], json!(false));
    assert!(r["result"]["structuredContent"]["$id"]
        .as_str()
        .unwrap()
        .contains("state.json"));

    // deck_emit
    let r = call(
        &mut b,
        9,
        "deck_emit",
        json!({"topic": "app/demo/hi", "payload": {"n": 1}}),
    );
    assert_eq!(
        r["result"]["structuredContent"]["emitted"],
        json!("app/demo/hi")
    );

    // deck_bind + deck_unbind
    let r = call(
        &mut b,
        10,
        "deck_bind",
        json!({"topic": "deck/control/keys.0/down", "do": [{"do": "sleep", "ms": 5}]}),
    );
    assert_eq!(r["result"]["structuredContent"]["id"], json!("b0001"));
    let r = call(&mut b, 11, "deck_unbind", json!({"binding": "b0001"}));
    assert_eq!(
        r["result"]["structuredContent"]["removed"],
        json!(["b0001"])
    );

    // deck_plugin_list on the phase-1 mock -> E_INTERNAL tool error, exit 8 (honest mapping)
    let r = call(&mut b, 12, "deck_plugin_list", json!({}));
    assert_eq!(r["result"]["isError"], json!(true));
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("E_INTERNAL"), "{text}");
    assert!(text.contains("[exit 8]"), "{text}");

    // unknown tool -> JSON-RPC -32602 (not a crash, not a tool error)
    let r = call(&mut b, 13, "deck_nope", json!({}));
    assert_eq!(r["error"]["code"], json!(-32602));
    // missing argument -> -32602
    let r = call(&mut b, 14, "deck_set", json!({"path": "/x"}));
    assert_eq!(r["error"]["code"], json!(-32602));
}

#[test]
fn deck_watch_streams_events_and_cancels() {
    let m = mock();
    let (mut b, rx) = harness(&m);

    // open the subscription (request id 20)
    let r = call(
        &mut b,
        20,
        "deck_watch",
        json!({"path": "/controls/keys/*"}),
    );
    assert_eq!(r["result"]["isError"], json!(false), "{r}");
    let sub = r["result"]["structuredContent"]["subscription"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(sub.starts_with('w'), "{sub}");
    assert_eq!(
        r["result"]["structuredContent"]["events"],
        json!("notifications/deck/event")
    );

    // a set on a matching path must arrive as a watch event
    let r = call(
        &mut b,
        21,
        "deck_set",
        json!({"path": "/controls/keys/1/label", "value": "hi"}),
    );
    assert_eq!(r["result"]["isError"], json!(false), "{r}");
    let ev = match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(In::Event { sub: s, event }) => {
            assert_eq!(s, sub);
            event
        }
        other => panic!("expected In::Event, got {other:?}"),
    };
    assert_eq!(ev["op"], json!("event"));
    assert_eq!(ev["path"], json!("/controls/keys/1/label"));
    assert_eq!(ev["value"], json!("hi"));

    // cancel via notifications/cancelled with the tools/call request id
    let out = b.handle_line(
        r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":20}}"#,
        false,
    );
    assert!(out.is_empty());
    let closed = match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(In::Closed { sub: s, reason }) => {
            assert_eq!(s, sub);
            reason
        }
        other => panic!("expected In::Closed, got {other:?}"),
    };
    assert!(closed.contains("cancelled"), "{closed}");
}

#[test]
fn deck_watch_max_events_closes_the_stream() {
    let m = mock();
    let (mut b, rx) = harness(&m);
    let r = call(
        &mut b,
        30,
        "deck_watch",
        json!({"path": "/controls/*", "max_events": 1}),
    );
    assert_eq!(r["result"]["isError"], json!(false), "{r}");
    let _ = call(
        &mut b,
        31,
        "deck_set",
        json!({"path": "/controls/keys/2/label", "value": "x"}),
    );
    // one event, then the closed notification
    let mut saw_event = false;
    let mut saw_closed = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline && !(saw_event && saw_closed) {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(In::Event { .. }) => saw_event = true,
            Ok(In::Closed { reason, .. }) => {
                saw_closed = true;
                assert!(reason.contains("max_events"), "{reason}");
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }
    assert!(saw_event && saw_closed, "event + closed expected");
}

#[test]
fn daemon_down_is_an_actionable_tool_error() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::new(
        dir.path().join("missing/daemon.sock"),
        Some(dir.path().join("missing/token")),
    );
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut b = Bridge::new(cfg, tx);
    let r = call(&mut b, 1, "deck_status", json!({}));
    assert_eq!(r["result"]["isError"], json!(true));
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("E_UNREACHABLE"), "{text}");
    assert!(text.contains("ds daemon start"), "{text}");
    assert!(text.contains("[exit 6]"), "{text}");
    // deck_watch against a down daemon is a tool error too, not a hang
    let r = call(&mut b, 2, "deck_watch", json!({"path": "/controls/*"}));
    assert_eq!(r["result"]["isError"], json!(true));
    assert!(r["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("E_UNREACHABLE"));
}
