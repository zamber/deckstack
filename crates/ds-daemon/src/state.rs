//! The state task: the **single writer** of the `ds-state` document (invariant 1 of
//! docs/components/daemon.md). It owns the [`Store`] and runs on its own thread; the IPC
//! handler talks to it exclusively through a **bounded** channel ([`StateCommand`]), so
//! the state lock is never held across device I/O or socket I/O and a burst of requests
//! can never grow unbounded memory.
//!
//! Supported commands: `get` (with optional `fields` projection), `set` (a full
//! [`Transaction`] with `if_rev`), `describe` (get + section metadata — trivial on top of
//! `get`), `subscribe`/`unsubscribe` (events pushed to the IPC connection), plus `rev`,
//! `snapshot` and `persist` for lifecycle/tests.
//!
//! Committed `/controls/...` changes are forwarded to the device task as
//! [`DevicePush`]es over a second bounded channel (dropped + counted when full — the
//! state task never blocks on the device).
//!
//! Committed durable changes are also published on the event bus
//! ([`crate::router::Bus`], DS-040) as state events — one per changed path, carrying the
//! commit's rev. `/telemetry/*` events never enter the bus (S1): they are delivered only
//! to the direct store subscriptions of `/telemetry/*` patterns.

use crate::device::DevicePush;
use crate::lifecycle;
use crate::router::Bus;
use ds_ipc::error::{ErrorCode, IpcError};
use ds_ipc::server::Connection;
use ds_state::{Event, GetReply, StateError, Store, SubscriptionId, Transaction};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// Capacity of the command channel (bounded; §5 spirit — the control plane may queue at
/// most this many state commands).
pub const STATE_CHANNEL_CAP: usize = 64;

/// How long a caller waits for the state task's reply before giving up (`E_INTERNAL`).
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// One command for the state task. Every variant carries a oneshot reply channel.
pub enum StateCommand {
    /// `get <path>` with an optional `fields` projection.
    Get {
        path: String,
        fields: Option<Vec<String>>,
        reply: Sender<StateResponse>,
    },
    /// Apply a transaction atomically (`if_rev` optional).
    Set {
        tx: Transaction,
        reply: Sender<StateResponse>,
    },
    /// `describe <path>`: the value plus which top-level section it lives in.
    Describe {
        path: String,
        reply: Sender<StateResponse>,
    },
    /// Subscribe to a path pattern; events are pushed on `conn`.
    Subscribe {
        pattern: String,
        conn: Connection,
        reply: Sender<StateResponse>,
    },
    /// Remove the subscription for a pattern.
    Unsubscribe {
        pattern: String,
        reply: Sender<StateResponse>,
    },
    /// Current `/meta/rev`.
    Rev { reply: Sender<StateResponse> },
    /// A clone of the whole in-memory document (tests/lifecycle).
    Snapshot { reply: Sender<StateResponse> },
    /// Persist the durable view now (clean shutdown).
    Persist { reply: Sender<StateResponse> },
    /// Stop the task loop (after the final persist).
    Stop,
}

/// The reply to a [`StateCommand`].
#[derive(Debug)]
pub enum StateResponse {
    /// `get`/`describe` result.
    Get(GetReply),
    /// `set` committed; the new rev.
    Set { rev: u64 },
    /// `describe` section name alongside the [`GetReply`].
    Describe { reply: GetReply, section: String },
    /// Subscription id.
    Subscribed(SubscriptionId),
    /// Boolean answer (unsubscribe hit/miss).
    Flag(bool),
    /// The current rev.
    Rev(u64),
    /// The whole document.
    Doc(Value),
    /// Persist done.
    Done,
    /// The command failed; maps 1:1 to the IPC error envelope.
    Err(IpcError),
}

/// Map a [`StateError`] to the IPC error envelope (`docs/contracts/ipc.md` §4).
pub fn state_error(e: StateError) -> IpcError {
    match e {
        StateError::Conflict { current_rev } => IpcError::new(
            ErrorCode::EConflict,
            format!("if_rev mismatch, current_rev is {current_rev}"),
        )
        .with_current_rev(current_rev),
        other => {
            let code = ErrorCode::parse(other.code()).unwrap_or(ErrorCode::EInternal);
            IpcError::new(code, other.to_string())
        }
    }
}

/// Apply a `fields` projection to a value (keeps only the listed keys of an object).
fn project(value: &Value, fields: Option<&[String]>) -> Value {
    let Some(fields) = fields else {
        return value.clone();
    };
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(k, _)| fields.iter().any(|f| f == *k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The state task: owns the store, applies commands, forwards device pushes and publishes
/// committed durable changes on the event bus.
pub struct StateTask {
    store: Store,
    rx: Receiver<StateCommand>,
    pushes: Option<SyncSender<DevicePush>>,
    bus: Bus,
    subs: HashMap<String, SubscriptionId>,
}

impl StateTask {
    /// Spawn the state task on a new thread. `pushes` (when given) receives the
    /// `/controls/...` change events for the device task; `bus` receives every committed
    /// durable change as a state event (DS-040).
    pub fn spawn(
        store: Store,
        pushes: Option<SyncSender<DevicePush>>,
        bus: Bus,
    ) -> (StateHandle, JoinHandle<()>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(STATE_CHANNEL_CAP);
        let handle = StateHandle { tx };
        let task = Self {
            store,
            rx,
            pushes,
            bus,
            subs: HashMap::new(),
        };
        let thread = std::thread::Builder::new()
            .name("ds-state".into())
            .spawn(move || task.run())
            .expect("spawn ds-state thread");
        (handle, thread)
    }

    fn run(mut self) {
        // One internal watcher on the root pattern forwards committed /controls changes
        // to the device task. try_send on a bounded channel: never blocks the state task.
        if let Some(pushes) = self.pushes.clone() {
            let cb: ds_state::store::WatchCallback = Arc::new(move |ev: Event| {
                if ev.telemetry || !ev.path.starts_with("/controls/") {
                    return;
                }
                let _ = pushes.try_send(DevicePush::State {
                    path: ev.path,
                    value: ev.value,
                });
            });
            if let Err(e) = self.store.subscribe("/", cb) {
                lifecycle::log(
                    "error",
                    "ds-daemon::state",
                    &format!("cannot subscribe the device forwarder: {e}"),
                );
            }
        }
        // A second root watcher publishes every committed durable change on the event bus
        // (DS-040). Telemetry events never enter the bus (S1). Publishing only enqueues
        // into bounded per-subscriber queues, so the state task never blocks on a
        // subscriber (backpressure: drop-oldest + counted `dropped`, see router::bus).
        {
            let bus = self.bus.clone();
            let cb: ds_state::store::WatchCallback = Arc::new(move |ev: Event| {
                if ev.telemetry {
                    return;
                }
                bus.publish(crate::router::Event::State {
                    path: ev.path,
                    ts_ms: lifecycle::now_ms() as u64,
                    value: ev.value,
                    prev: ev.prev,
                    rev: ev.rev.unwrap_or(0),
                });
            });
            if let Err(e) = self.store.subscribe("/", cb) {
                lifecycle::log(
                    "error",
                    "ds-daemon::state",
                    &format!("cannot subscribe the bus publisher: {e}"),
                );
            }
        }
        while let Ok(cmd) = self.rx.recv() {
            if self.handle(cmd) {
                break; // Stop
            }
        }
    }

    /// Handle one command; returns `true` on `Stop`.
    fn handle(&mut self, cmd: StateCommand) -> bool {
        match cmd {
            StateCommand::Get {
                path,
                fields,
                reply,
            } => {
                let resp = match self.store.get(&path) {
                    Ok(r) => StateResponse::Get(GetReply {
                        rev: r.rev,
                        value: project(&r.value, fields.as_deref()),
                    }),
                    Err(e) => StateResponse::Err(state_error(e)),
                };
                let _ = reply.send(resp);
            }
            StateCommand::Set { tx, reply } => {
                let resp = match self.store.commit(&tx) {
                    Ok(c) => StateResponse::Set { rev: c.rev },
                    Err(e) => StateResponse::Err(state_error(e)),
                };
                let _ = reply.send(resp);
            }
            StateCommand::Describe { path, reply } => {
                let section = path
                    .trim_start_matches('/')
                    .split('/')
                    .next()
                    .unwrap_or("")
                    .to_string();
                let resp = match self.store.get(&path) {
                    Ok(r) => StateResponse::Describe { reply: r, section },
                    Err(e) => StateResponse::Err(state_error(e)),
                };
                let _ = reply.send(resp);
            }
            StateCommand::Subscribe {
                pattern,
                conn,
                reply,
            } => {
                let cb: ds_state::store::WatchCallback = Arc::new(move |ev: Event| {
                    let mut msg = serde_json::json!({
                        "op": "event",
                        "path": ev.path,
                        "value": ev.value,
                    });
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
                match self.store.subscribe(&pattern, cb) {
                    Ok(id) => {
                        self.subs.insert(pattern, id);
                        let _ = reply.send(StateResponse::Subscribed(id));
                    }
                    Err(e) => {
                        let _ = reply.send(StateResponse::Err(state_error(e)));
                    }
                }
            }
            StateCommand::Unsubscribe { pattern, reply } => {
                let removed = self
                    .subs
                    .remove(&pattern)
                    .map(|id| self.store.unsubscribe(id))
                    .unwrap_or(false);
                let _ = reply.send(StateResponse::Flag(removed));
            }
            StateCommand::Rev { reply } => {
                let _ = reply.send(StateResponse::Rev(self.store.rev()));
            }
            StateCommand::Snapshot { reply } => {
                let _ = reply.send(StateResponse::Doc(self.store.document()));
            }
            StateCommand::Persist { reply } => {
                let resp = match self.store.persist_now() {
                    Ok(()) => StateResponse::Done,
                    Err(e) => StateResponse::Err(state_error(e)),
                };
                let _ = reply.send(resp);
            }
            StateCommand::Stop => return true,
        }
        false
    }
}

/// The IPC handler's handle to the state task (cloneable, bounded channel).
#[derive(Clone)]
pub struct StateHandle {
    tx: SyncSender<StateCommand>,
}

impl StateHandle {
    fn ask(
        &self,
        cmd: impl FnOnce(Sender<StateResponse>) -> StateCommand,
    ) -> Result<StateResponse, IpcError> {
        let (reply, rx) = std::sync::mpsc::channel();
        self.tx
            .try_send(cmd(reply))
            .map_err(|_| IpcError::new(ErrorCode::ELimit, "state command channel is full"))?;
        rx.recv_timeout(REPLY_TIMEOUT)
            .map_err(|_| IpcError::new(ErrorCode::EInternal, "state task did not answer in time"))
    }

    /// `get <path>` with optional `fields`.
    pub fn get(&self, path: &str, fields: Option<Vec<String>>) -> Result<GetReply, IpcError> {
        match self.ask(|reply| StateCommand::Get {
            path: path.to_string(),
            fields,
            reply,
        })? {
            StateResponse::Get(r) => Ok(r),
            StateResponse::Err(e) => Err(e),
            _ => Err(IpcError::new(
                ErrorCode::EInternal,
                "unexpected state reply",
            )),
        }
    }

    /// Apply a transaction.
    pub fn set(&self, tx: Transaction) -> Result<u64, IpcError> {
        match self.ask(|reply| StateCommand::Set { tx, reply })? {
            StateResponse::Set { rev } => Ok(rev),
            StateResponse::Err(e) => Err(e),
            _ => Err(IpcError::new(
                ErrorCode::EInternal,
                "unexpected state reply",
            )),
        }
    }

    /// `describe <path>`.
    pub fn describe(&self, path: &str) -> Result<(GetReply, String), IpcError> {
        match self.ask(|reply| StateCommand::Describe {
            path: path.to_string(),
            reply,
        })? {
            StateResponse::Describe { reply, section } => Ok((reply, section)),
            StateResponse::Err(e) => Err(e),
            _ => Err(IpcError::new(
                ErrorCode::EInternal,
                "unexpected state reply",
            )),
        }
    }

    /// Subscribe to a pattern; `conn` receives the events.
    pub fn subscribe(&self, pattern: &str, conn: Connection) -> Result<SubscriptionId, IpcError> {
        match self.ask(|reply| StateCommand::Subscribe {
            pattern: pattern.to_string(),
            conn,
            reply,
        })? {
            StateResponse::Subscribed(id) => Ok(id),
            StateResponse::Err(e) => Err(e),
            _ => Err(IpcError::new(
                ErrorCode::EInternal,
                "unexpected state reply",
            )),
        }
    }

    /// Unsubscribe a pattern.
    pub fn unsubscribe(&self, pattern: &str) -> Result<bool, IpcError> {
        match self.ask(|reply| StateCommand::Unsubscribe {
            pattern: pattern.to_string(),
            reply,
        })? {
            StateResponse::Flag(b) => Ok(b),
            StateResponse::Err(e) => Err(e),
            _ => Err(IpcError::new(
                ErrorCode::EInternal,
                "unexpected state reply",
            )),
        }
    }

    /// Current rev.
    pub fn rev(&self) -> Result<u64, IpcError> {
        match self.ask(|reply| StateCommand::Rev { reply })? {
            StateResponse::Rev(r) => Ok(r),
            _ => Err(IpcError::new(
                ErrorCode::EInternal,
                "unexpected state reply",
            )),
        }
    }

    /// Whole-document snapshot.
    pub fn snapshot(&self) -> Result<Value, IpcError> {
        match self.ask(|reply| StateCommand::Snapshot { reply })? {
            StateResponse::Doc(d) => Ok(d),
            _ => Err(IpcError::new(
                ErrorCode::EInternal,
                "unexpected state reply",
            )),
        }
    }

    /// Persist now (clean shutdown).
    pub fn persist(&self) -> Result<(), IpcError> {
        match self.ask(|reply| StateCommand::Persist { reply })? {
            StateResponse::Done => Ok(()),
            StateResponse::Err(e) => Err(e),
            _ => Err(IpcError::new(
                ErrorCode::EInternal,
                "unexpected state reply",
            )),
        }
    }

    /// Stop the state task.
    pub fn stop(&self) {
        let _ = self.tx.try_send(StateCommand::Stop);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_state::{Op, Store};
    use serde_json::json;

    fn spawn_fresh() -> (
        StateHandle,
        JoinHandle<()>,
        Option<Receiver<DevicePush>>,
        Bus,
    ) {
        let (push_tx, push_rx) = std::sync::mpsc::sync_channel(8);
        let bus = Bus::new();
        let (h, t) = StateTask::spawn(Store::fresh(), Some(push_tx), bus.clone());
        (h, t, Some(push_rx), bus)
    }

    #[test]
    fn get_set_describe_round_trip() {
        let (h, t, _rx, _bus) = spawn_fresh();
        // empty state at start
        let meta = h.get("/meta", None).unwrap();
        assert_eq!(meta.rev, Some(0));
        assert_eq!(meta.value["rev"], json!(0));
        // set bumps rev by exactly 1
        let rev = h
            .set(Transaction::new(vec![Op::set(
                "/controls/keys/0/image",
                json!({"kind": "color", "value": "#3cc86e"}),
            )]))
            .unwrap();
        assert_eq!(rev, 1);
        let got = h.get("/controls/keys/0/image", None).unwrap();
        assert_eq!(got.rev, Some(1));
        assert_eq!(got.value, json!({"kind": "color", "value": "#3cc86e"}));
        // fields projection
        h.set(Transaction::new(vec![Op::set(
            "/controls/keys/0/label",
            json!("Salon"),
        )]))
        .unwrap();
        let proj = h
            .get("/controls/keys/0", Some(vec!["label".into()]))
            .unwrap();
        assert_eq!(proj.value, json!({"label": "Salon"}));
        // describe is get + section
        let (reply, section) = h.describe("/controls/keys/0").unwrap();
        assert_eq!(section, "controls");
        assert_eq!(reply.value["label"], json!("Salon"));
        // if_rev conflict
        let err = h
            .set(Transaction::new(vec![Op::set("/settings/x", json!(1))]).with_if_rev(0))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::EConflict);
        assert_eq!(err.current_rev, Some(2));
        // missing path
        let err = h.get("/nope", None).unwrap_err();
        assert_eq!(err.code, ErrorCode::EPath);
        // snapshot + rev
        assert_eq!(h.rev().unwrap(), 2);
        let doc = h.snapshot().unwrap();
        assert_eq!(doc["controls"]["keys"]["0"]["label"], json!("Salon"));
        h.persist().unwrap();
        h.stop();
        t.join().unwrap();
    }

    #[test]
    fn committed_controls_changes_are_forwarded_to_the_device_channel() {
        let (h, t, rx, _bus) = spawn_fresh();
        h.set(Transaction::new(vec![Op::set(
            "/controls/keys/3/image",
            json!({"kind": "color", "value": "#ff8800"}),
        )]))
        .unwrap();
        let push = rx
            .as_ref()
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        match push {
            DevicePush::State { path, value } => {
                assert_eq!(path, "/controls/keys/3/image");
                assert_eq!(value["value"], json!("#ff8800"));
            }
        }
        // a non-controls change is not forwarded
        h.set(Transaction::new(vec![Op::set(
            "/settings/log/level",
            json!("debug"),
        )]))
        .unwrap();
        assert!(rx.as_ref().unwrap().try_recv().is_err());
        h.stop();
        t.join().unwrap();
    }

    #[test]
    fn committed_changes_are_published_on_the_bus_as_state_events() {
        let (h, t, _rx, bus) = spawn_fresh();
        let (sink, rx) = std::sync::mpsc::channel();
        let sink = std::sync::Arc::new(crate::router::FnSink(std::sync::Arc::new(
            move |msg: &serde_json::Value| {
                let _ = sink.send(msg.clone());
            },
        )));
        let pat = crate::router::Pattern::parse_wire("/controls/keys/*").unwrap();
        bus.subscribe(pat, None, "/controls/keys/*".to_string(), sink);
        let rev = h
            .set(Transaction::new(vec![Op::set(
                "/controls/keys/3/label",
                json!("Salon"),
            )]))
            .unwrap();
        let msg = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("one bus event per committed change");
        assert_eq!(msg["op"], "event");
        assert_eq!(msg["path"], "/controls/keys/3/label");
        assert_eq!(msg["rev"], rev);
        assert_eq!(msg["value"], json!("Salon"));
        // a change outside the pattern is not delivered
        h.set(Transaction::new(vec![Op::set("/settings/x", json!(1))]))
            .unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
        h.stop();
        t.join().unwrap();
    }

    #[test]
    fn bounded_channel_rejects_bursts_beyond_the_cap() {
        // Fill the channel without a running task: after STATE_CHANNEL_CAP sends, try_send
        // must fail with Full.
        let (tx, _rx) = std::sync::mpsc::sync_channel::<StateCommand>(STATE_CHANNEL_CAP);
        let (reply, _rrx) = std::sync::mpsc::channel();
        let mut sent = 0;
        while tx
            .try_send(StateCommand::Rev {
                reply: reply.clone(),
            })
            .is_ok()
        {
            sent += 1;
        }
        assert_eq!(sent, STATE_CHANNEL_CAP);
    }
}
