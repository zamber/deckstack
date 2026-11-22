//! The daemon's control-plane handler: embedded in the `ds-ipc` [`Server`], which owns
//! the socket, the NDJSON framing, the `hello`/`auth` handshake and the per-op scope
//! enforcement. This handler only sees authenticated, scope-checked requests.
//!
//! Phase-1 ops served here (the state ops of `docs/contracts/ipc.md` §4):
//! `ping`, `get`, `set`, `subscribe`/`unsubscribe`, `emit`, `daemon status|stop`, `schema`.
//! Binding/plugin/stream/token ops answer a clear "later ticket" error envelope.
//!
//! All state access goes through the [`StateHandle`] (bounded channel to the state
//! task) — the IPC thread never touches the state document directly. Subscriptions and
//! emits go through the event bus ([`crate::router::Bus`], DS-040): a `subscribe` whose
//! `path` starts with a topic root (`/deck/…`, `/app/…`, `/daemon/…`, `/plugin/…`) is a
//! topic subscription; any other path is a state-path subscription delivered with the
//! historical `{"op":"event","path":…,"rev":…}` shape. `/telemetry/*` subscriptions stay
//! on the direct store subscription (S1: telemetry never enters the bus).
//!
//! [`Server`]: ds_ipc::Server

use crate::lifecycle::Shutdown;
use crate::router::{Bus, IpcSink, Pattern};
use crate::state::StateHandle;
use ds_ipc::error::ErrorCode;
use ds_ipc::message::{Request, Response};
use ds_ipc::server::{Connection, Handler};
use ds_state::{Op, Transaction};
use serde_json::{json, Value};
use std::sync::Arc;

/// Static daemon status fields reported by `daemon status`.
#[derive(Debug, Clone)]
pub struct StatusInfo {
    /// Daemon pid.
    pub pid: u32,
    /// The device URI as given on the command line (`none` when absent).
    pub device: String,
    /// Start time, ms since the Unix epoch.
    pub started_ms: u128,
    /// Device status string from the device task (`connected …` / `none` / `error: …`).
    pub device_status: crate::device::DeviceStatus,
}

/// The daemon's op handler. Cloneable state: a [`StateHandle`], the event bus, plus the
/// shutdown flag.
pub struct DaemonHandler {
    state: StateHandle,
    bus: Bus,
    shutdown: Shutdown,
    status: StatusInfo,
    server_name: String,
}

impl DaemonHandler {
    /// Build the handler.
    pub fn new(
        state: StateHandle,
        bus: Bus,
        shutdown: Shutdown,
        status: StatusInfo,
        server_name: String,
    ) -> Self {
        Self {
            state,
            bus,
            shutdown,
            status,
            server_name,
        }
    }

    /// Dispatch a request that needs no connection (unit-testable core of `handle`).
    pub fn dispatch(&self, req: &Request) -> Response {
        let id = req.id();
        match req.op() {
            "ping" => Response::ok(id, json!({"pong": true})),
            "get" => self.op_get(req, id),
            "set" => self.op_set(req, id),
            "emit" => self.op_emit(req, id),
            "daemon" => self.op_daemon(req, id),
            "schema" => self.op_schema(req, id),
            other => Response::error_code(
                id,
                ErrorCode::EInternal,
                format!(
                    "op '{other}' is not served by this phase-1 daemon (bindings/plugins/streams/token land in later tickets)"
                ),
            ),
        }
    }

    fn op_get(&self, req: &Request, id: Option<i64>) -> Response {
        let Some(path) = req.get("path").and_then(Value::as_str) else {
            return Response::error_code(id, ErrorCode::EPath, "get needs a path");
        };
        let fields = req.get("fields").and_then(Value::as_array).map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        });
        match self.state.get(path, fields) {
            Ok(reply) => {
                let result = json!({"path": path, "value": reply.value});
                match reply.rev {
                    // Durable reads carry the rev; /telemetry/* reads do not (S1).
                    Some(rev) => Response::ok_rev(id, rev, result),
                    None => Response::ok(id, result),
                }
            }
            Err(e) => Response::error(id, e),
        }
    }

    fn op_set(&self, req: &Request, id: Option<i64>) -> Response {
        let Some(ops_value) = req.get("ops").and_then(Value::as_array) else {
            return Response::error_code(id, ErrorCode::ESchema, "set needs an ops array");
        };
        let mut ops: Vec<Op> = Vec::new();
        for op in ops_value {
            let Some(path) = op.get("path").and_then(Value::as_str) else {
                return Response::error_code(id, ErrorCode::ESchema, "set op needs a path");
            };
            match op.get("op").and_then(Value::as_str) {
                Some("set") => ops.push(Op::set(
                    path,
                    op.get("value").cloned().unwrap_or(Value::Null),
                )),
                Some("delete") => ops.push(Op::delete(path)),
                Some("merge") => {
                    return Response::error_code(
                        id,
                        ErrorCode::ESchema,
                        "op 'merge' is not supported by ds-state yet",
                    );
                }
                _ => {
                    return Response::error_code(
                        id,
                        ErrorCode::ESchema,
                        format!("unknown set op {:?}", op.get("op")),
                    );
                }
            }
        }
        let mut tx = Transaction::new(ops);
        if let Some(rev) = req.get("if_rev").and_then(Value::as_u64) {
            tx = tx.with_if_rev(rev);
        }
        match self.state.set(tx) {
            Ok(rev) => Response {
                id,
                ok: true,
                rev: Some(rev),
                result: None,
                error: None,
            },
            Err(e) => Response::error(id, e),
        }
    }

    /// `emit <topic> [payload]` (ipc-message.json): publish the topic on the event bus
    /// (DS-040). The schema already restricts `topic` to `^(deck|app)/`; the full topic
    /// grammar (actions.md §1) is validated here.
    fn op_emit(&self, req: &Request, id: Option<i64>) -> Response {
        let Some(topic) = req.get("topic").and_then(Value::as_str) else {
            return Response::error_code(id, ErrorCode::ESchema, "emit needs a topic");
        };
        let payload = req.get("payload").cloned().unwrap_or(Value::Null);
        if let Err(msg) = crate::router::topics::validate_topic(topic) {
            return Response::error_code(id, ErrorCode::ESchema, msg);
        }
        let n = self
            .bus
            .publish(crate::router::Event::topic(topic, payload));
        Response::ok(id, json!({"emitted": topic, "subscribers": n}))
    }

    fn op_daemon(&self, req: &Request, id: Option<i64>) -> Response {
        match req.get("action").and_then(Value::as_str) {
            Some("status") => {
                let rev = self.state.rev().unwrap_or(0);
                let device_status = self
                    .status
                    .device_status
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string());
                Response::ok(
                    id,
                    json!({
                        "running": true,
                        "rev": rev,
                        "pid": self.status.pid,
                        "server": self.server_name,
                        "device": self.status.device,
                        "device_status": device_status,
                        "protocol": ds_ipc::PROTOCOL_VERSION,
                        "uptime_ms": crate::lifecycle::now_ms().saturating_sub(self.status.started_ms),
                    }),
                )
            }
            Some("stop") => {
                self.shutdown.trigger();
                Response::ok(id, json!({"stopping": true}))
            }
            Some("reload") => Response::error_code(
                id,
                ErrorCode::EInternal,
                "daemon reload is not implemented in phase 1",
            ),
            _ => Response::error_code(id, ErrorCode::ESchema, "daemon needs an action"),
        }
    }

    fn op_schema(&self, req: &Request, id: Option<i64>) -> Response {
        let Some(name) = req.get("name").and_then(Value::as_str) else {
            return Response::error_code(id, ErrorCode::ESchema, "schema needs a name");
        };
        let raw = match name {
            "state" => ds_state::schema::STATE_SCHEMA_JSON,
            "ipc" => ds_ipc::message::IPC_SCHEMA_JSON,
            "binding" => ds_ipc::message::BINDING_SCHEMA_JSON,
            "plugin-manifest" => ds_state::schema::PLUGIN_MANIFEST_SCHEMA_JSON,
            "stream-frame" => crate::STREAM_FRAME_SCHEMA_JSON,
            other => {
                return Response::error_code(
                    id,
                    ErrorCode::ESchema,
                    format!("unknown schema name '{other}'"),
                );
            }
        };
        match serde_json::from_str::<Value>(raw) {
            Ok(v) => Response::ok(id, v),
            Err(e) => Response::error_code(
                id,
                ErrorCode::EInternal,
                format!("embedded schema {name} does not parse: {e}"),
            ),
        }
    }
}

impl Handler for DaemonHandler {
    fn rev(&self) -> u64 {
        self.state.rev().unwrap_or(0)
    }

    fn handle(&self, conn: &Connection, req: &Request) -> Response {
        let id = req.id();
        match req.op() {
            "subscribe" => {
                let Some(pattern) = req.get("path").and_then(Value::as_str) else {
                    return Response::error_code(id, ErrorCode::EPath, "subscribe needs a path");
                };
                // Subscriptions are exempt from the idle timeout (ipc.md §5).
                conn.set_idle_exempt(true);
                if pattern == "/telemetry" || pattern.starts_with("/telemetry/") {
                    // S1: telemetry never enters the bus; /telemetry/* subscriptions are
                    // served directly by the state store, exactly as before.
                    match self.state.subscribe(pattern, conn.clone()) {
                        Ok(sub_id) => {
                            Response::ok(id, json!({"subscribed": pattern, "id": sub_id}))
                        }
                        Err(e) => Response::error(id, e),
                    }
                } else {
                    match Pattern::parse_wire(pattern) {
                        Ok(pat) => {
                            let wire = pat.as_wire();
                            let sub_id = self.bus.subscribe(
                                pat,
                                Some(conn.id()),
                                wire.clone(),
                                Arc::new(IpcSink {
                                    conn: conn.clone(),
                                    wire_pattern: wire,
                                }),
                            );
                            Response::ok(id, json!({"subscribed": pattern, "id": sub_id}))
                        }
                        Err(e) => Response::error(id, e),
                    }
                }
            }
            "unsubscribe" => {
                let Some(pattern) = req.get("path").and_then(Value::as_str) else {
                    return Response::error_code(id, ErrorCode::EPath, "unsubscribe needs a path");
                };
                if pattern == "/telemetry" || pattern.starts_with("/telemetry/") {
                    match self.state.unsubscribe(pattern) {
                        Ok(removed) => {
                            Response::ok(id, json!({"unsubscribed": pattern, "removed": removed}))
                        }
                        Err(e) => Response::error(id, e),
                    }
                } else {
                    let removed = self.bus.unsubscribe_wire(conn.id(), pattern);
                    Response::ok(id, json!({"unsubscribed": pattern, "removed": removed}))
                }
            }
            _ => self.dispatch(req),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle;
    use ds_state::Store;
    use serde_json::json;
    use std::sync::Arc;

    fn handler() -> (
        DaemonHandler,
        crate::state::StateHandle,
        crate::router::Bus,
        std::thread::JoinHandle<()>,
    ) {
        let (state, t) = crate::state::StateTask::spawn(Store::fresh(), None, Bus::new());
        let bus = Bus::new();
        let status = StatusInfo {
            pid: std::process::id(),
            device: "none".into(),
            started_ms: lifecycle::now_ms(),
            device_status: Arc::new(std::sync::Mutex::new(Some("none".into()))),
        };
        let h = DaemonHandler::new(
            state.clone(),
            bus.clone(),
            Shutdown::manual(),
            status,
            crate::server_name(),
        );
        (h, state, bus, t)
    }

    fn req(v: Value) -> Request {
        Request::new(v)
    }

    #[test]
    fn ping_and_schema() {
        let (h, state, _bus, t) = handler();
        let r = h.dispatch(&req(json!({"id": 1, "op": "ping"})));
        assert!(r.ok);
        assert_eq!(r.result.as_ref().unwrap()["pong"], json!(true));
        r.validate().unwrap();
        let r = h.dispatch(&req(json!({"id": 2, "op": "schema", "name": "state"})));
        assert!(r.ok);
        assert_eq!(
            r.result.as_ref().unwrap()["$id"],
            json!("https://deckstack.dev/schemas/state.json/v1")
        );
        r.validate().unwrap();
        let r = h.dispatch(&req(json!({"id": 3, "op": "schema", "name": "nope"})));
        assert!(!r.ok);
        assert_eq!(r.error.as_ref().unwrap().code, ErrorCode::ESchema);
        state.stop();
        t.join().unwrap();
    }

    #[test]
    fn get_set_round_trip_and_conflict() {
        let (h, state, _bus, t) = handler();
        // empty state at start
        let r = h.dispatch(&req(json!({"id": 1, "op": "get", "path": "/meta"})));
        assert!(r.ok);
        assert_eq!(r.rev, Some(0));
        assert_eq!(r.result.as_ref().unwrap()["value"]["rev"], json!(0));
        r.validate().unwrap();
        // set
        let r = h.dispatch(&req(json!({
            "id": 2, "op": "set",
            "ops": [{"op": "set", "path": "/controls/keys/0/image",
                     "value": {"kind": "color", "value": "#3cc86e"}}]
        })));
        assert!(r.ok);
        assert_eq!(r.rev, Some(1));
        assert!(
            r.result.is_none(),
            "set replies ok+rev only, like the corpus"
        );
        r.validate().unwrap();
        // get it back
        let r = h.dispatch(&req(
            json!({"id": 3, "op": "get", "path": "/controls/keys/0/image"}),
        ));
        assert_eq!(
            r.result.as_ref().unwrap()["value"],
            json!({"kind": "color", "value": "#3cc86e"})
        );
        // fields projection
        let r = h.dispatch(&req(
            json!({"id": 4, "op": "get", "path": "/controls/keys/0", "fields": ["image"]}),
        ));
        assert_eq!(
            r.result.as_ref().unwrap()["value"],
            json!({"image": {"kind": "color", "value": "#3cc86e"}})
        );
        // if_rev conflict
        let r = h.dispatch(&req(json!({
            "id": 5, "op": "set", "if_rev": 0,
            "ops": [{"op": "set", "path": "/settings/x", "value": 1}]
        })));
        assert!(!r.ok);
        let e = r.error.as_ref().unwrap();
        assert_eq!(e.code, ErrorCode::EConflict);
        assert_eq!(e.current_rev, Some(1));
        r.validate().unwrap();
        // delete op
        let r = h.dispatch(&req(json!({
            "id": 6, "op": "set",
            "ops": [{"op": "delete", "path": "/controls/keys/0/image"}]
        })));
        assert!(r.ok);
        assert_eq!(r.rev, Some(2));
        // missing path -> E_PATH
        let r = h.dispatch(&req(json!({"id": 7, "op": "get", "path": "/nope"})));
        assert_eq!(r.error.as_ref().unwrap().code, ErrorCode::EPath);
        // merge not supported
        let r = h.dispatch(&req(json!({
            "id": 8, "op": "set",
            "ops": [{"op": "merge", "path": "/settings", "value": {}}]
        })));
        assert_eq!(r.error.as_ref().unwrap().code, ErrorCode::ESchema);
        state.stop();
        t.join().unwrap();
    }

    #[test]
    fn daemon_status_and_stop_and_unimplemented_ops() {
        let (h, state, _bus, t) = handler();
        let r = h.dispatch(&req(json!({"id": 1, "op": "daemon", "action": "status"})));
        assert!(r.ok);
        let result = r.result.as_ref().unwrap();
        assert_eq!(result["running"], json!(true));
        assert_eq!(result["server"], json!(crate::server_name()));
        assert_eq!(result["device"], json!("none"));
        assert_eq!(result["pid"], json!(std::process::id() as u64));
        r.validate().unwrap();
        // an op of a later phase answers a clear error, not a crash
        let r = h.dispatch(&req(json!({"id": 2, "op": "bind", "binding": {}})));
        assert!(!r.ok);
        assert_eq!(r.error.as_ref().unwrap().code, ErrorCode::EInternal);
        // stop triggers the shutdown flag
        assert!(!h.shutdown.get());
        let r = h.dispatch(&req(json!({"id": 3, "op": "daemon", "action": "stop"})));
        assert!(r.ok);
        assert!(h.shutdown.get());
        state.stop();
        t.join().unwrap();
    }

    #[test]
    fn in_process_server_round_trip_with_auth_and_scope() {
        // Full stack minus the binary: ds-ipc Server + DaemonHandler on a temp socket.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("data");
        lifecycle::prepare_data_dir(&dir).unwrap();
        let token = lifecycle::ensure_token(&dir).unwrap();
        let bus = Bus::new();
        let (state, st) = crate::state::StateTask::spawn(Store::fresh(), None, bus.clone());
        let status = StatusInfo {
            pid: std::process::id(),
            device: "none".into(),
            started_ms: lifecycle::now_ms(),
            device_status: Arc::new(std::sync::Mutex::new(Some("none".into()))),
        };
        let handler = Arc::new(DaemonHandler::new(
            state.clone(),
            bus,
            Shutdown::manual(),
            status,
            crate::server_name(),
        ));
        let config = ds_ipc::ServerConfig {
            server_name: crate::server_name(),
            idle_timeout: Some(std::time::Duration::from_secs(5)),
            ..ds_ipc::ServerConfig::default()
        };
        let server = ds_ipc::Server::bind(&dir.join("daemon.sock"), &dir, config).unwrap();
        let _accept = server.start(handler).unwrap();

        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixStream;
        let stream = UnixStream::connect(dir.join("daemon.sock")).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut w = stream;
        let mut send = |v: Value| {
            let mut line = serde_json::to_vec(&v).unwrap();
            line.push(b'\n');
            w.write_all(&line).unwrap();
            w.flush().unwrap();
        };
        let recv = |reader: &mut BufReader<UnixStream>| -> Value {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            serde_json::from_str(line.trim()).unwrap()
        };
        send(json!({"id": 0, "op": "hello", "protocol": 1, "client": "test/1"}));
        let welcome = recv(&mut reader);
        assert_eq!(welcome["ok"], json!(true));
        assert_eq!(welcome["result"]["rev"], json!(0), "empty state at start");
        // bad token -> E_AUTH
        send(json!({"id": 1, "op": "auth", "token": "0".repeat(64), "scope": "admin"}));
        let r = recv(&mut reader);
        assert_eq!(r["ok"], json!(false));
        assert_eq!(r["error"]["code"], json!("E_AUTH"));
        // good token
        send(json!({"id": 2, "op": "auth", "token": token, "scope": "admin"}));
        let r = recv(&mut reader);
        assert_eq!(r["result"]["scope"], json!("admin"));
        // ping / get / set / get
        send(json!({"id": 3, "op": "ping"}));
        assert_eq!(recv(&mut reader)["result"]["pong"], json!(true));
        send(json!({"id": 4, "op": "get", "path": "/controls"}));
        assert_eq!(recv(&mut reader)["result"]["value"], json!({}));
        send(
            json!({"id": 5, "op": "set", "ops": [{"op": "set", "path": "/controls/dial/image", "value": {"kind": "color", "value": "#112233"}}]}),
        );
        let r = recv(&mut reader);
        assert_eq!(r["ok"], json!(true));
        assert_eq!(r["rev"], json!(1));
        send(json!({"id": 6, "op": "get", "path": "/controls/dial/image"}));
        let r = recv(&mut reader);
        assert_eq!(
            r["result"]["value"],
            json!({"kind": "color", "value": "#112233"})
        );
        // op before auth on a fresh connection -> E_AUTH
        let s2 = UnixStream::connect(dir.join("daemon.sock")).unwrap();
        let mut r2 = BufReader::new(s2.try_clone().unwrap());
        let mut w2 = s2;
        let mut line = serde_json::to_vec(&json!({"id": 9, "op": "ping"})).unwrap();
        line.push(b'\n');
        w2.write_all(&line).unwrap();
        let mut l = String::new();
        r2.read_line(&mut l).unwrap();
        // first message must be hello -> E_VERSION
        let v: Value = serde_json::from_str(l.trim()).unwrap();
        assert_eq!(v["error"]["code"], json!("E_VERSION"));

        state.stop();
        st.join().unwrap();
    }

    #[test]
    fn state_response_variants_are_consumed() {
        // keep the match on StateResponse exhaustive in the helpers
        use crate::state::StateResponse;
        let r = StateResponse::Rev(7);
        assert!(matches!(r, StateResponse::Rev(7)));
    }

    #[test]
    fn emit_op_validates_topic_and_publishes_on_the_bus() {
        let (h, state, bus, t) = handler();
        let (sink, rx) = std::sync::mpsc::channel();
        let sink = Arc::new(crate::router::FnSink(Arc::new(move |msg: &Value| {
            let _ = sink.send(msg.clone());
        })));
        bus.subscribe(
            crate::router::Pattern::parse_wire("/app/*").unwrap(),
            None,
            "/app/*".to_string(),
            sink,
        );
        // valid emit
        let r = h.dispatch(&req(
            json!({"id": 1, "op": "emit", "topic": "app/demo/hi", "payload": {"n": 1}}),
        ));
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.result.as_ref().unwrap()["emitted"], "app/demo/hi");
        assert_eq!(r.result.as_ref().unwrap()["subscribers"], 1);
        r.validate().unwrap();
        let msg = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("emit reaches bus subscribers");
        assert_eq!(msg["op"], "event");
        assert_eq!(msg["topic"], "app/demo/hi");
        assert_eq!(msg["path"], "/app/demo/hi");
        assert_eq!(msg["payload"]["n"], 1);
        ds_ipc::message::validate_server_message(&msg).unwrap();
        // invalid grammar -> E_SCHEMA, nothing published
        let r = h.dispatch(&req(
            json!({"id": 2, "op": "emit", "topic": "app/onlysender"}),
        ));
        assert!(!r.ok);
        assert_eq!(r.error.as_ref().unwrap().code, ErrorCode::ESchema);
        let r = h.dispatch(&req(
            json!({"id": 3, "op": "emit", "topic": "deck/control/keys.99/down"}),
        ));
        assert!(!r.ok);
        assert_eq!(r.error.as_ref().unwrap().code, ErrorCode::ESchema);
        // missing topic -> E_SCHEMA
        let r = h.dispatch(&req(json!({"id": 4, "op": "emit"})));
        assert!(!r.ok);
        assert_eq!(r.error.as_ref().unwrap().code, ErrorCode::ESchema);
        state.stop();
        t.join().unwrap();
    }

    /// Boot an in-process server and return (dir, token, state handle, state thread).
    fn boot_server() -> (
        tempfile::TempDir,
        String,
        crate::state::StateHandle,
        std::thread::JoinHandle<()>,
        crate::router::Bus,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("data");
        lifecycle::prepare_data_dir(&dir).unwrap();
        let token = lifecycle::ensure_token(&dir).unwrap();
        let bus = Bus::new();
        let (state, st) = crate::state::StateTask::spawn(Store::fresh(), None, bus.clone());
        let status = StatusInfo {
            pid: std::process::id(),
            device: "none".into(),
            started_ms: lifecycle::now_ms(),
            device_status: Arc::new(std::sync::Mutex::new(Some("none".into()))),
        };
        let handler = Arc::new(DaemonHandler::new(
            state.clone(),
            bus.clone(),
            Shutdown::manual(),
            status,
            crate::server_name(),
        ));
        let config = ds_ipc::ServerConfig {
            server_name: crate::server_name(),
            idle_timeout: Some(std::time::Duration::from_secs(30)),
            ..ds_ipc::ServerConfig::default()
        };
        let server = ds_ipc::Server::bind(&dir.join("daemon.sock"), &dir, config).unwrap();
        let _accept = server.start(handler).unwrap();
        (tmp, token, state, st, bus)
    }

    /// One authenticated control connection with line helpers.
    struct TestConn {
        reader: std::io::BufReader<std::os::unix::net::UnixStream>,
        w: std::os::unix::net::UnixStream,
    }

    impl TestConn {
        fn connect(sock: &std::path::Path, token: &str) -> Self {
            use std::io::{BufRead, Write};
            let stream = std::os::unix::net::UnixStream::connect(sock).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            let mut c = Self {
                reader: std::io::BufReader::new(stream.try_clone().unwrap()),
                w: stream,
            };
            c.rpc(json!({"id": 0, "op": "hello", "protocol": 1, "client": "test/1"}));
            c.rpc(json!({"id": 1, "op": "auth", "token": token, "scope": "admin"}));
            c
        }

        fn send(&mut self, v: Value) {
            use std::io::Write;
            let mut line = serde_json::to_vec(&v).unwrap();
            line.push(b'\n');
            self.w.write_all(&line).unwrap();
            self.w.flush().unwrap();
        }

        fn recv(&mut self) -> Value {
            use std::io::BufRead;
            let mut line = String::new();
            self.reader.read_line(&mut line).unwrap();
            serde_json::from_str(line.trim()).unwrap()
        }

        fn rpc(&mut self, v: Value) -> Value {
            self.send(v);
            self.recv()
        }
    }

    #[test]
    fn bus_subscriptions_over_the_real_socket_state_and_topic_events() {
        let (tmp, token, state, st, _bus) = boot_server();
        let sock = tmp.path().join("data").join("daemon.sock");
        let mut sub = TestConn::connect(&sock, &token); // the subscriber connection
        let mut ctl = TestConn::connect(&sock, &token); // the acting connection

        // state-path subscription via the bus
        let r = sub.rpc(json!({"id": 2, "op": "subscribe", "path": "/controls/keys/*"}));
        assert!(r["ok"] == json!(true), "{r}");
        assert_eq!(r["result"]["subscribed"], "/controls/keys/*");
        let sub_id = r["result"]["id"].as_u64().unwrap();
        assert!(sub_id >= 1);

        // a topic subscription on the same connection
        let r = sub.rpc(json!({"id": 3, "op": "subscribe", "path": "/app/*"}));
        assert!(r["ok"] == json!(true), "{r}");

        // set on the control connection -> one state event with the new rev on sub
        let r = ctl.rpc(json!({"id": 10, "op": "set", "ops": [{"op": "set", "path": "/controls/keys/3/label", "value": "watched"}]}));
        assert!(r["ok"] == json!(true));
        let rev = r["rev"].as_u64().unwrap();
        // read lines on sub until the event arrives (the set response goes to ctl, not sub)
        let mut got = None;
        for _ in 0..10 {
            let line = sub.recv();
            if line["op"] == "event" {
                got = Some(line);
                break;
            }
        }
        let ev = got.expect("state event delivered on the bus subscription");
        assert_eq!(ev["path"], "/controls/keys/3/label");
        assert_eq!(ev["rev"], rev);
        assert_eq!(ev["value"], "watched");
        assert!(ev.get("topic").is_none(), "state events keep the old shape");

        // emit on ctl -> topic event on the /app/* subscription
        let r =
            ctl.rpc(json!({"id": 11, "op": "emit", "topic": "app/demo/hi", "payload": {"n": 7}}));
        assert!(r["ok"] == json!(true));
        let mut got = None;
        for _ in 0..10 {
            let line = sub.recv();
            if line["op"] == "event" && line["topic"] == "app/demo/hi" {
                got = Some(line);
                break;
            }
        }
        let ev = got.expect("emit reaches the /app/* topic subscription");
        assert_eq!(ev["topic"], "app/demo/hi");
        assert_eq!(ev["path"], "/app/demo/hi");
        assert_eq!(ev["payload"]["n"], 7);
        assert!(ev.get("rev").is_none());

        // unsubscribe stops delivery of both kinds
        let r = sub.rpc(json!({"id": 4, "op": "unsubscribe", "path": "/controls/keys/*"}));
        assert!(r["ok"] == json!(true));
        assert_eq!(r["result"]["removed"], true);
        let r = sub.rpc(json!({"id": 5, "op": "unsubscribe", "path": "/app/*"}));
        assert!(r["ok"] == json!(true));
        assert_eq!(r["result"]["removed"], true);
        let r = ctl.rpc(json!({"id": 12, "op": "set", "ops": [{"op": "set", "path": "/controls/keys/4/label", "value": "no"}]}));
        assert!(r["ok"] == json!(true));
        let r = ctl.rpc(json!({"id": 13, "op": "emit", "topic": "app/demo/again"}));
        assert!(r["ok"] == json!(true));
        // nothing more may arrive on sub (give the daemon a moment)
        sub.w
            .set_read_timeout(Some(std::time::Duration::from_millis(400)))
            .unwrap();
        use std::io::BufRead;
        let mut line = String::new();
        match sub.reader.read_line(&mut line) {
            Ok(0) => {}
            Ok(_) => panic!("event after unsubscribe: {line}"),
            Err(_) => {} // timeout: correct
        }

        state.stop();
        st.join().unwrap();
    }

    #[test]
    fn slow_subscriber_does_not_stall_sets_over_the_real_socket() {
        let (tmp, token, state, st, bus) = boot_server();
        let sock = tmp.path().join("data").join("daemon.sock");
        let mut slow = TestConn::connect(&sock, &token);
        let mut ctl = TestConn::connect(&sock, &token);
        // the slow subscriber registers and then never reads again
        let r = slow.rpc(json!({"id": 2, "op": "subscribe", "path": "/controls/*"}));
        assert!(r["ok"] == json!(true));

        // flood the bus from the control connection; every publish must be fast and every
        // set must succeed even though `slow` never reads
        let n = 4000;
        let start = std::time::Instant::now();
        for i in 0..n {
            let r = ctl.rpc(json!({
                "id": 100 + i,
                "op": "set",
                "ops": [{"op": "set", "path": "/controls/keys/0/label", "value": format!("v{i}")}]
            }));
            assert!(r["ok"] == json!(true), "set {i} failed: {r}");
        }
        let flood = start.elapsed();
        // one more set must still be answered promptly (the state task is not blocked)
        let t0 = std::time::Instant::now();
        let r = ctl.rpc(json!({"id": 9999, "op": "set", "ops": [{"op": "set", "path": "/controls/keys/1/label", "value": "final"}]}));
        assert!(r["ok"] == json!(true));
        let last = t0.elapsed();
        assert!(
            last < std::time::Duration::from_secs(2),
            "final set took {last:?} — the state task must not block on a slow subscriber"
        );
        // the slow subscriber's queue overflowed: drop-oldest with a counted drop
        let dropped = bus.total_dropped();
        assert!(
            dropped > 0,
            "expected counted drops on the slow subscriber (flood {n} events in {flood:?}), got {dropped}"
        );
        // drain the slow connection: it must contain at least one `dropped` message
        slow.w
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut dropped_msgs = 0u64;
        let mut counted = 0u64;
        loop {
            use std::io::BufRead;
            let mut line = String::new();
            match slow.reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
                        if v["op"] == "dropped" {
                            dropped_msgs += 1;
                            counted += v["count"].as_u64().unwrap_or(0);
                        }
                    }
                }
                Err(_) => break,
            }
            if line.is_empty() {
                break;
            }
        }
        assert!(
            dropped_msgs >= 1,
            "no dropped message on the slow connection"
        );
        assert_eq!(
            counted, dropped,
            "wire dropped counts must match the bus counter"
        );

        state.stop();
        st.join().unwrap();
    }
}
