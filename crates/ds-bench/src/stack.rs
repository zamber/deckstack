//! The in-process bench stack (DS-033): a throttled ds-vdev, a control-plane server
//! (`<dir>/daemon.sock`) and the DS-031 frame service serving `<dir>/stream.sock`,
//! all on one tmp data dir — the same library wiring the DS-031 in-crate integration
//! test uses (`crates/ds-daemon/src/frames/service.rs`), because `daemon::run` does
//! not serve `stream.sock` yet (docs/components/frame-service.md §wiring).
//!
//! The control handler is a minimal `ds-ipc` [`Handler`] backed by the **same**
//! `Arc<Store>` the frame service samples telemetry into, so `get
//! /telemetry/streams/<target>` over the socket sees the live counters. The daemon
//! tasks (state task, device task) are not needed by the bench: frames flow through
//! the frame service's own writer threads to the vdev transport.

use ds_client::Client;
use ds_daemon::frames::service::FrameService;
use ds_daemon::frames::FrameTarget;
use ds_daemon::lifecycle::{self, Shutdown};
use ds_daemon::router::Bus;
use ds_daemon::state::state_error;
use ds_device::transport::Transport;
use ds_device::uri::DeviceUri;
use ds_device::vtransport::virtual_module_id;
use ds_ipc::error::ErrorCode;
use ds_ipc::frames::StreamConfig;
use ds_ipc::limits::ServerConfig;
use ds_ipc::message::{Request, Response};
use ds_ipc::server::{Connection, Handler, Server};
use ds_protocol::ModuleKind;
use ds_state::store::WatchCallback;
use ds_state::{Event, Op, Store, Transaction};
use ds_vdev::{HubConfig, Throttle, VdevServer};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Knobs for the virtual device.
#[derive(Debug, Clone, Default)]
pub struct StackConfig {
    /// `--throttle-kbps`: simulated device write cap; `None` = unlimited.
    pub throttle_kbps: Option<u32>,
    /// `--drop-after`: hot-unplug after n writes; the bench never uses it.
    pub drop_after: Option<u32>,
}

/// One booted stack: vdev + control server + frame service on a tmp dir.
pub struct Stack {
    /// The tmp dir (removed on drop / [`Stack::stop`]).
    pub tmp: tempfile::TempDir,
    /// The virtual device.
    pub vdev: VdevServer,
    /// The vdev device socket path (`virtual:<path>`).
    pub vdev_sock: PathBuf,
    /// The data dir holding daemon.sock / stream.sock / token / state.json.
    pub data_dir: PathBuf,
    /// The control socket (`<data_dir>/daemon.sock`).
    pub socket: PathBuf,
    /// The frame-stream socket (`<data_dir>/stream.sock`).
    pub stream_sock: PathBuf,
    /// The store shared by the control handler and the frame service.
    pub store: Arc<Store>,
    /// The DS-031 frame service (queues + writers + telemetry sampler).
    pub frames: FrameService,
    /// Shutdown flag shared by the frame service.
    pub shutdown: Shutdown,
    /// The device transport the writer threads send through.
    pub transport: Arc<dyn Transport>,
    /// The vdev socket path again (for `ds-vdevctl`-style inspection).
    accept_thread: Option<JoinHandle<()>>,
}

/// The minimal control-plane handler: `ping`/`get`/`set`/`subscribe`/`unsubscribe`/
/// `daemon status` against the shared store, with the same reply shapes as the
/// daemon's own handler (`crates/ds-daemon/src/ipc.rs`): durable reads carry `rev`,
/// `/telemetry/*` reads do not (S1).
struct BenchHandler {
    store: Arc<Store>,
    subs: Mutex<HashMap<String, u64>>,
}

impl BenchHandler {
    fn op_get(&self, req: &Request, id: Option<i64>) -> Response {
        let Some(path) = req.get("path").and_then(Value::as_str) else {
            return Response::error_code(id, ErrorCode::EPath, "get needs a path");
        };
        match self.store.get(path) {
            Ok(reply) => {
                let result = json!({"path": path, "value": reply.value});
                match reply.rev {
                    Some(rev) => Response::ok_rev(id, rev, result),
                    None => Response::ok(id, result),
                }
            }
            Err(e) => Response::error(id, state_error(e)),
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
                other => {
                    return Response::error_code(
                        id,
                        ErrorCode::ESchema,
                        format!("unknown set op {other:?}"),
                    );
                }
            }
        }
        let mut tx = Transaction::new(ops);
        if let Some(rev) = req.get("if_rev").and_then(Value::as_u64) {
            tx = tx.with_if_rev(rev);
        }
        match self.store.commit(&tx) {
            Ok(commit) => Response::ok_rev(id, commit.rev, json!({"rev": commit.rev})),
            Err(e) => Response::error(id, state_error(e)),
        }
    }

    fn op_subscribe(&self, conn: &Connection, req: &Request, id: Option<i64>) -> Response {
        let Some(pattern) = req.get("path").and_then(Value::as_str) else {
            return Response::error_code(id, ErrorCode::EPath, "subscribe needs a path");
        };
        let conn = conn.clone();
        let cb: WatchCallback = Arc::new(move |ev: Event| {
            let mut msg = json!({"op": "event", "path": ev.path, "value": ev.value});
            if let Some(rev) = ev.rev {
                msg["rev"] = rev.into();
            }
            if let Some(prev) = ev.prev {
                msg["prev"] = prev;
            }
            if ev.telemetry {
                msg["telemetry"] = true.into();
            }
            let _ = conn.send_event(&msg);
        });
        match self.store.subscribe(pattern, cb) {
            Ok(sub_id) => {
                self.subs
                    .lock()
                    .unwrap()
                    .insert(pattern.to_string(), sub_id);
                Response::ok(id, json!({"subscribed": pattern, "id": sub_id}))
            }
            Err(e) => Response::error(id, state_error(e)),
        }
    }

    fn op_unsubscribe(&self, req: &Request, id: Option<i64>) -> Response {
        let Some(pattern) = req.get("path").and_then(Value::as_str) else {
            return Response::error_code(id, ErrorCode::EPath, "unsubscribe needs a path");
        };
        let sub_id = self.subs.lock().unwrap().remove(pattern);
        let removed = sub_id.is_some_and(|sid| self.store.unsubscribe(sid));
        Response::ok(id, json!({"unsubscribed": pattern, "removed": removed}))
    }
}

impl Handler for BenchHandler {
    fn rev(&self) -> u64 {
        self.store.rev()
    }

    fn handle(&self, conn: &Connection, req: &Request) -> Response {
        let id = req.id();
        match req.op() {
            "ping" => Response::ok(id, json!({"pong": true})),
            "get" => self.op_get(req, id),
            "set" => self.op_set(req, id),
            "subscribe" => self.op_subscribe(conn, req, id),
            "unsubscribe" => self.op_unsubscribe(req, id),
            // B1: reads /telemetry/streams/<target> (non-durable, no rev, S1). Used
            // instead of `get` for `key:N` targets because the ipc-message path pattern
            // excludes ':' — exactly why the contract has this op.
            "stream-stats" => {
                let Some(target) = req.get("target").and_then(Value::as_str) else {
                    return Response::error_code(
                        id,
                        ErrorCode::ESchema,
                        "stream-stats needs a target",
                    );
                };
                match self.store.get(&format!("/telemetry/streams/{target}")) {
                    Ok(reply) => Response::ok(id, json!({"target": target, "value": reply.value})),
                    Err(e) => Response::error(id, state_error(e)),
                }
            }
            "daemon" if req.get("action").and_then(Value::as_str) == Some("status") => {
                Response::ok(
                    id,
                    json!({
                        "running": true,
                        "pid": std::process::id(),
                        "device": format!("virtual:{}", "<bench vdev>"),
                        "rev": self.store.rev(),
                    }),
                )
            }
            other => Response::error_code(
                id,
                ErrorCode::EInternal,
                format!("op '{other}' is not served by the ds-bench harness daemon"),
            ),
        }
    }
}

impl Stack {
    /// Boot the stack on a fresh tmp dir.
    pub fn boot(cfg: &StackConfig) -> Result<Self, String> {
        let tmp = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
        let vdev_sock = tmp.path().join("vdev.sock");
        let vdev = VdevServer::start(
            &vdev_sock,
            HubConfig {
                modules: ModuleKind::ALL.to_vec(),
                throttle: Throttle {
                    kbps: cfg.throttle_kbps,
                    drop_after: cfg.drop_after,
                },
            },
        )
        .map_err(|e| format!("vdev start: {e}"))?;

        let uri = DeviceUri::parse(&format!("virtual:{}", vdev_sock.display()))
            .map_err(|e| format!("device uri: {e}"))?;
        let transport: Arc<dyn Transport> =
            Arc::from(ds_device::open(&uri).map_err(|e| format!("open vdev transport: {e}"))?);
        // Software-mode handshake before any command/write (mh-protocol safety invariant).
        for kind in [ModuleKind::Keys, ModuleKind::Dial] {
            if let Some((report, _mode)) = ds_protocol::enter_software_mode(kind) {
                transport
                    .send(&virtual_module_id(kind), &report)
                    .map_err(|e| format!("software-mode handshake {kind:?}: {e}"))?;
            }
        }

        let data_dir = tmp.path().join("data");
        lifecycle::prepare_data_dir(&data_dir).map_err(|e| format!("data dir: {e}"))?;
        lifecycle::ensure_token(&data_dir).map_err(|e| format!("token: {e}"))?;
        let socket = data_dir.join("daemon.sock");
        let server = Server::bind(
            &socket,
            &data_dir,
            ServerConfig {
                idle_timeout: None,
                server_name: ds_daemon::server_name(),
                ..ServerConfig::default()
            },
        )
        .map_err(|e| format!("control server bind: {e}"))?;
        let store = Arc::new(
            Store::open(&data_dir.join("state.json")).map_err(|e| format!("store open: {e}"))?,
        );
        let shutdown = Shutdown::manual();
        let bus = Bus::new();
        let handler = Arc::new(BenchHandler {
            store: store.clone(),
            subs: Mutex::new(HashMap::new()),
        });
        let accept = server
            .start(handler)
            .map_err(|e| format!("control server start: {e}"))?;
        let frames = FrameService::spawn(
            store.clone(),
            Some(transport.clone()),
            bus,
            shutdown.clone(),
        );
        let (_stream_bus, stream_sock) = frames
            .serve_stream_socket(
                &data_dir,
                StreamConfig {
                    idle_timeout: None,
                    ..StreamConfig::default()
                },
            )
            .map_err(|e| format!("stream socket bind: {e}"))?;
        Ok(Stack {
            tmp,
            vdev,
            vdev_sock,
            data_dir,
            socket,
            stream_sock,
            store,
            frames,
            shutdown,
            transport,
            accept_thread: Some(accept),
        })
    }

    /// A connected, authenticated control-plane client for this stack.
    pub fn control_client(&self) -> Result<Client, String> {
        let mut c = Client::connect(&self.socket).map_err(|e| format!("connect: {e}"))?;
        c.hello(&format!("ds-bench/{}", crate::version()))
            .map_err(|e| format!("hello: {e}"))?;
        c.authenticate_token_file(None)
            .map_err(|e| format!("auth: {e}"))?;
        Ok(c)
    }

    /// Deterministic cleanup: trigger shutdown, join the frame-service writers and
    /// sampler (they drain their queues first), persist the store, stop the vdev and
    /// remove the tmp dir. The control accept thread cannot be unblocked from outside
    /// (same as the daemon itself); it ends with the process.
    pub fn stop(mut self) {
        self.shutdown.trigger();
        self.frames.join();
        let _ = self.store.persist_now();
        self.vdev.shutdown();
        drop(self.accept_thread.take());
        drop(self.tmp);
    }
}

/// Send one `DSctl\n` NDJSON control line on a stream socket (test-side helper).
pub fn send_dsctl(w: &mut impl Write, v: &Value) -> Result<(), String> {
    let mut line = serde_json::to_vec(v).map_err(|e| format!("serialize dsctl: {e}"))?;
    line.push(b'\n');
    w.write_all(ds_ipc::frames::DSCTL_MAGIC)
        .and_then(|_| w.write_all(&line))
        .and_then(|_| w.flush())
        .map_err(|e| format!("stream socket write: {e}"))
}

/// Read one `DSctl\n` control line from a stream socket.
pub fn recv_dsctl(reader: &mut impl std::io::BufRead) -> Result<Value, String> {
    let mut magic = [0u8; 6];
    reader
        .read_exact(&mut magic)
        .map_err(|e| format!("stream socket read: {e}"))?;
    if &magic != b"DSctl\n" {
        return Err(format!("bad stream-socket magic {magic:?}"));
    }
    let line = ds_ipc::framing::read_line(reader, ds_ipc::frames::limits::MAX_DSCTL_LINE_BYTES)
        .map_err(|e| format!("dsctl line: {e:?}"))?;
    serde_json::from_str(line.trim()).map_err(|e| format!("bad dsctl json: {e}"))
}

/// Read the peak RSS (`VmHWM`) of a process from `/proc/<pid>/status`, in bytes.
pub fn read_peak_rss_bytes(pid: u32) -> Result<u64, String> {
    let status =
        std::fs::read_to_string(format!("/proc/{pid}/status")).map_err(|e| format!("{e}"))?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kb: u64 = rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse()
                .map_err(|e| format!("bad VmHWM line {line:?}: {e}"))?;
            return Ok(kb * 1024);
        }
    }
    Err(format!("no VmHWM in /proc/{pid}/status"))
}

/// Parse a stream target name (`keys` | `key:0-14` | `dial`) into a [`FrameTarget`].
pub fn parse_target(s: &str) -> Result<FrameTarget, String> {
    FrameTarget::parse(s)
        .ok_or_else(|| format!("invalid stream target {s:?} — valid: keys | key:0-14 | dial (S6)"))
}

/// Milliseconds since the Unix epoch (frame meta `ts_ms`).
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_target_accepts_the_s6_vocabulary() {
        assert_eq!(parse_target("keys").unwrap(), FrameTarget::Panel);
        assert_eq!(parse_target("key:0").unwrap(), FrameTarget::Key(0));
        assert_eq!(parse_target("key:14").unwrap(), FrameTarget::Key(14));
        assert_eq!(parse_target("dial").unwrap(), FrameTarget::Dial);
        assert!(parse_target("panel").is_err());
        assert!(parse_target("key:15").is_err());
        assert!(parse_target("screen").is_err());
    }

    #[test]
    fn peak_rss_of_self_is_readable_and_sane() {
        let bytes = read_peak_rss_bytes(std::process::id()).unwrap();
        assert!(bytes > 0, "VmHWM must be positive");
        assert!(
            bytes < 16 * 1024 * 1024 * 1024,
            "VmHWM implausibly large: {bytes}"
        );
    }

    #[test]
    fn boot_serves_control_and_stream_sockets() {
        let stack = Stack::boot(&StackConfig {
            throttle_kbps: Some(16800),
            drop_after: None,
        })
        .unwrap();
        assert!(stack.socket.exists());
        assert!(stack.stream_sock.exists());
        let mut c = stack.control_client().unwrap();
        let pong = c.ping().unwrap();
        assert_eq!(pong["pong"], json!(true));
        // fresh stack: empty state at rev 0
        let g = c.get("/controls", None).unwrap();
        assert_eq!(g["result"]["value"], json!({}));
        assert_eq!(g["rev"], json!(0));
        // a set bumps rev, a telemetry read carries no rev (S1)
        c.set(
            json!([{"op": "set", "path": "/controls/keys/0/label", "value": "x"}]),
            None,
        )
        .unwrap();
        let g = c.get("/controls/keys/0", None).unwrap();
        assert_eq!(g["result"]["value"]["label"], json!("x"));
        assert_eq!(g["rev"], json!(1));
        let t = c.request(json!({"op": "stream-stats", "target": "key:0"}));
        assert!(t.is_err(), "no telemetry before any frame: {t:?}");
        // /telemetry paths are absent until the sampler writes them (no rev, S1)
        assert!(c.get("/telemetry/streams/keys", None).is_err());
        stack.stop();
    }
}
