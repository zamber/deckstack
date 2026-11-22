//! The daemon's control-plane handler: embedded in the `ds-ipc` [`Server`], which owns
//! the socket, the NDJSON framing, the `hello`/`auth` handshake and the per-op scope
//! enforcement. This handler only sees authenticated, scope-checked requests.
//!
//! Phase-1 ops served here (the state ops of `docs/contracts/ipc.md` §4):
//! `ping`, `get`, `set`, `subscribe`/`unsubscribe`, `daemon status|stop`, `schema`.
//! Router/binding/plugin/stream/token ops answer a clear "later ticket" error envelope.
//!
//! All state access goes through the [`StateHandle`] (bounded channel to the state
//! task) — the IPC thread never touches the state document directly.
//!
//! [`Server`]: ds_ipc::Server

use crate::lifecycle::Shutdown;
use crate::state::StateHandle;
use ds_ipc::error::ErrorCode;
use ds_ipc::message::{Request, Response};
use ds_ipc::server::{Connection, Handler};
use ds_state::{Op, Transaction};
use serde_json::{json, Value};

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

/// The daemon's op handler. Cloneable state: a [`StateHandle`] plus the shutdown flag.
pub struct DaemonHandler {
    state: StateHandle,
    shutdown: Shutdown,
    status: StatusInfo,
    server_name: String,
}

impl DaemonHandler {
    /// Build the handler.
    pub fn new(
        state: StateHandle,
        shutdown: Shutdown,
        status: StatusInfo,
        server_name: String,
    ) -> Self {
        Self {
            state,
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
            "daemon" => self.op_daemon(req, id),
            "schema" => self.op_schema(req, id),
            other => Response::error_code(
                id,
                ErrorCode::EInternal,
                format!(
                    "op '{other}' is not served by this phase-1 daemon (router/bindings/plugins/streams/token land in later tickets)"
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
                match self.state.subscribe(pattern, conn.clone()) {
                    Ok(sub_id) => Response::ok(id, json!({"subscribed": pattern, "id": sub_id})),
                    Err(e) => Response::error(id, e),
                }
            }
            "unsubscribe" => {
                let Some(pattern) = req.get("path").and_then(Value::as_str) else {
                    return Response::error_code(id, ErrorCode::EPath, "unsubscribe needs a path");
                };
                match self.state.unsubscribe(pattern) {
                    Ok(removed) => {
                        Response::ok(id, json!({"unsubscribed": pattern, "removed": removed}))
                    }
                    Err(e) => Response::error(id, e),
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
        std::thread::JoinHandle<()>,
    ) {
        let (state, t) = crate::state::StateTask::spawn(Store::fresh(), None);
        let status = StatusInfo {
            pid: std::process::id(),
            device: "none".into(),
            started_ms: lifecycle::now_ms(),
            device_status: Arc::new(std::sync::Mutex::new(Some("none".into()))),
        };
        let h = DaemonHandler::new(
            state.clone(),
            Shutdown::manual(),
            status,
            crate::server_name(),
        );
        (h, state, t)
    }

    fn req(v: Value) -> Request {
        Request::new(v)
    }

    #[test]
    fn ping_and_schema() {
        let (h, state, t) = handler();
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
        let (h, state, t) = handler();
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
        let (h, state, t) = handler();
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
        let (state, st) = crate::state::StateTask::spawn(Store::fresh(), None);
        let status = StatusInfo {
            pid: std::process::id(),
            device: "none".into(),
            started_ms: lifecycle::now_ms(),
            device_status: Arc::new(std::sync::Mutex::new(Some("none".into()))),
        };
        let handler = Arc::new(DaemonHandler::new(
            state.clone(),
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
}
