//! The in-daemon event bus: pub/sub over topics with bounded per-subscriber queues
//! (`docs/components/event-router.md`, ticket DS-040).
//!
//! ## Producers and consumers
//!
//! Producers (the state task after every commit, the device task on input reports and
//! connect/disconnect, the IPC handler on `emit`, the daemon loop on lifecycle) call
//! [`Bus::publish`], which only takes the bus lock and enqueues into each matching
//! subscriber's bounded queue — **it never blocks and never writes to a socket**, so a
//! slow or dead subscriber can never stall the state task or the device reader.
//!
//! Each subscription owns a drainer thread that pops its queue FIFO and delivers the
//! wire-format message through its [`Sink`]. A blocked sink (client not reading) only
//! stalls its own drainer; the queue then fills up to [`SUBSCRIBER_QUEUE_CAP`] and
//! further events are **dropped oldest**, counted, logged with a clear `warn` line and
//! reported to the client as `{"op":"dropped","path":<pattern>,"count":n}`
//! (state.md §5, ipc.md §5). A dead sink is detected on the next failed send and the
//! subscription removes itself.
//!
//! ## Dispatch order
//!
//! On publish, matching subscribers are served **exact patterns first, then wildcard
//! patterns** (a pattern containing `*`), each group in subscription (id) order —
//! actions.md §2 "exact topic → `<prefix>/*` wildcard, all matches in array order".
//! Within a subscriber, events are delivered strictly in publish order (one event per
//! committed revision for state events; frames and telemetry never enter the bus, S1).

use crate::lifecycle;
use ds_ipc::error::IpcError;
use ds_ipc::server::Connection;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::Pattern;

/// Per-subscriber queue capacity. ipc.md §5 caps queued events per subscription at 1000;
/// overflow is handled by drop-oldest + a counted `dropped` message (never by blocking
/// the publisher, state.md §5 "the state task never blocks").
pub const SUBSCRIBER_QUEUE_CAP: usize = 1000;

/// How long a drainer waits on its condvar before re-checking the stop flag.
const DRAIN_POLL: Duration = Duration::from_millis(100);

/// One event on the bus: `{topic, ts_ms, payload, rev}` (actions.md §1).
///
/// - [`Event::State`] is a committed state change: its "topic" is the state path itself
///   (`/controls/keys/0/image`) and it always carries the commit's `rev`. Its wire
///   message is exactly the historical `{"op":"event","path":…,"rev":…,"value":…,"prev":…}`.
/// - [`Event::Topic`] is a bus topic event (`deck/…`, `app/…`, `daemon/…`, `plugin/…`);
///   `rev` is `None` (only state events carry a revision).
#[derive(Debug, Clone)]
pub enum Event {
    /// A topic event (device input, emit, lifecycle).
    Topic {
        topic: String,
        ts_ms: u64,
        payload: Value,
        rev: Option<u64>,
    },
    /// A committed state change (one per changed path per revision).
    State {
        path: String,
        ts_ms: u64,
        value: Value,
        prev: Option<Value>,
        rev: u64,
    },
}

impl Event {
    /// A topic event with the current time and no rev.
    pub fn topic(topic: impl Into<String>, payload: Value) -> Self {
        Self::Topic {
            topic: topic.into(),
            ts_ms: lifecycle::now_ms() as u64,
            payload,
            rev: None,
        }
    }

    /// A topic event with an explicit rev (rare; state events carry the rev instead).
    pub fn topic_with_rev(topic: impl Into<String>, payload: Value, rev: u64) -> Self {
        Self::Topic {
            topic: topic.into(),
            ts_ms: lifecycle::now_ms() as u64,
            payload,
            rev: Some(rev),
        }
    }

    /// A committed state change event.
    pub fn state(path: impl Into<String>, value: Value, prev: Option<Value>, rev: u64) -> Self {
        Self::State {
            path: path.into(),
            ts_ms: lifecycle::now_ms() as u64,
            value,
            prev,
            rev,
        }
    }

    /// The event's subject for logs: the topic, or the state path.
    pub fn subject(&self) -> &str {
        match self {
            Event::Topic { topic, .. } => topic,
            Event::State { path, .. } => path,
        }
    }

    /// The NDJSON server message delivered to subscribers of this event.
    ///
    /// State events keep the historical shape exactly; topic events carry the topic (and
    /// its slash-spelled `path`, so every event has a path-like subject), `ts_ms` and
    /// `payload` (extra properties are allowed by `schemas/ipc-message.json`).
    pub fn wire(&self) -> Value {
        match self {
            Event::Topic {
                topic,
                ts_ms,
                payload,
                rev,
            } => {
                let mut msg = json!({
                    "op": "event",
                    "topic": topic,
                    "path": format!("/{topic}"),
                    "ts_ms": ts_ms,
                    "payload": payload,
                });
                if let Some(rev) = rev {
                    msg["rev"] = (*rev).into();
                }
                msg
            }
            Event::State {
                path,
                value,
                prev,
                rev,
                ..
            } => {
                // Exactly the historical state-event shape (state.md §5); the bus-level
                // ts_ms is internal metadata and is not serialized here.
                let mut msg = json!({
                    "op": "event",
                    "path": path,
                    "rev": rev,
                    "value": value,
                });
                if let Some(prev) = prev {
                    msg["prev"] = prev.clone();
                }
                msg
            }
        }
    }
}

/// Delivery end of a subscription. [`IpcSink`] writes NDJSON server messages on the IPC
/// connection; tests can use any other implementation.
pub trait Sink: Send + Sync + 'static {
    /// Deliver one wire-format message. `Err` means the subscriber is gone.
    fn deliver(&self, msg: &Value) -> Result<(), IpcError>;
}

/// A subscription sink writing on an IPC connection.
pub struct IpcSink {
    /// The connection events are pushed on.
    pub conn: Connection,
    /// The wire spelling of the subscription pattern (for `dropped` messages).
    pub wire_pattern: String,
}

impl Sink for IpcSink {
    fn deliver(&self, msg: &Value) -> Result<(), IpcError> {
        self.conn.send_event(msg)
    }
}

/// A sink invoking a closure (unit tests, in-process consumers).
pub struct FnSink(pub Arc<dyn Fn(&Value) + Send + Sync>);

impl Sink for FnSink {
    fn deliver(&self, msg: &Value) -> Result<(), IpcError> {
        (self.0)(msg);
        Ok(())
    }
}

/// The bounded FIFO queue of one subscriber, shared between the bus (producer side,
/// [`SubQueue::enqueue`]) and the drainer thread (consumer side).
pub struct SubQueue {
    state: Mutex<QState>,
    cond: Condvar,
    stopped: AtomicBool,
}

struct QState {
    items: VecDeque<Event>,
    /// Events dropped oldest so far (cumulative).
    dropped: u64,
    /// Drops already reported to the subscriber via `dropped` messages.
    notified: u64,
}

impl SubQueue {
    fn new() -> Self {
        Self {
            state: Mutex::new(QState {
                items: VecDeque::new(),
                dropped: 0,
                notified: 0,
            }),
            cond: Condvar::new(),
            stopped: AtomicBool::new(false),
        }
    }

    /// Enqueue one event; drop-oldest + count + log when the queue is at the cap.
    /// Never blocks.
    fn enqueue(&self, ev: Event, sub_id: u64, wire: &str) {
        let mut st = self.state.lock().unwrap();
        if st.items.len() >= SUBSCRIBER_QUEUE_CAP {
            st.items.pop_front();
            st.dropped += 1;
            lifecycle::log(
                "warn",
                "ds-daemon::router",
                &format!(
                    "subscriber #{sub_id} ({wire}) queue full ({SUBSCRIBER_QUEUE_CAP} queued): \
                     dropped oldest event {} — total dropped {}",
                    ev.subject(),
                    st.dropped
                ),
            );
        }
        st.items.push_back(ev);
        self.cond.notify_one();
    }

    /// Signal the drainer to stop (unsubscribe / dead sink); remaining items are dropped.
    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.cond.notify_all();
    }

    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
}

/// Dispatch order for one event: indices of the matching subscribers, exact patterns
/// first, then wildcard patterns; subscription (id) order within each group
/// (actions.md §2). Stable.
fn dispatch_order(subs: &[Sub], ev: &Event) -> Vec<usize> {
    let mut matched: Vec<usize> = subs
        .iter()
        .enumerate()
        .filter(|(_, s)| s.pattern.matches(ev))
        .map(|(i, _)| i)
        .collect();
    matched.sort_by_key(|&i| (subs[i].pattern.is_wildcard(), subs[i].id));
    matched
}

struct Sub {
    id: u64,
    pattern: Pattern,
    /// Original wire spelling of the pattern (subscribe op `path`).
    wire: String,
    /// Connection id for IPC sinks (used for connection-scoped unsubscribe).
    conn_id: Option<u64>,
    queue: Arc<SubQueue>,
}

struct BusInner {
    next_id: u64,
    subs: Vec<Sub>,
}

/// The event bus. Cloneable handle; all clones share one subscriber set.
#[derive(Clone)]
pub struct Bus {
    inner: Arc<Mutex<BusInner>>,
}

impl Default for Bus {
    fn default() -> Self {
        Self::new()
    }
}

impl Bus {
    /// A fresh, empty bus.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(BusInner {
                next_id: 1,
                subs: Vec::new(),
            })),
        }
    }

    /// Subscribe to a pattern. `conn_id` scopes the subscription to an IPC connection
    /// (for connection-scoped `unsubscribe`); `wire` is the original pattern spelling
    /// used in `dropped` messages. Subscribing the same (conn, wire) twice returns the
    /// existing id (no duplicate delivery). Returns the subscription id.
    pub fn subscribe(
        &self,
        pattern: Pattern,
        conn_id: Option<u64>,
        wire: String,
        sink: Arc<dyn Sink>,
    ) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        if let Some(cid) = conn_id {
            if let Some(existing) = inner
                .subs
                .iter()
                .find(|s| s.conn_id == Some(cid) && s.wire == wire)
            {
                return existing.id;
            }
        }
        let id = inner.next_id;
        inner.next_id += 1;
        let queue = Arc::new(SubQueue::new());
        inner.subs.push(Sub {
            id,
            pattern,
            wire: wire.clone(),
            conn_id,
            queue: queue.clone(),
        });
        drop(inner);
        Self::spawn_drainer(self.inner.clone(), id, queue, sink, wire);
        id
    }

    /// Remove one subscription by id. Returns false when unknown.
    pub fn unsubscribe(&self, id: u64) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let before = inner.subs.len();
        if let Some(pos) = inner.subs.iter().position(|s| s.id == id) {
            let sub = inner.subs.remove(pos);
            sub.queue.stop();
            true
        } else {
            inner.subs.len() != before
        }
    }

    /// Remove every subscription of `conn_id` whose wire pattern equals `wire`
    /// (the `unsubscribe` op). Returns whether anything was removed.
    pub fn unsubscribe_wire(&self, conn_id: u64, wire: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let before = inner.subs.len();
        let mut removed: Vec<Arc<SubQueue>> = Vec::new();
        inner.subs.retain(|s| {
            if s.conn_id == Some(conn_id) && s.wire == wire {
                removed.push(s.queue.clone());
                false
            } else {
                true
            }
        });
        let any = inner.subs.len() != before;
        drop(inner);
        for q in removed {
            q.stop();
        }
        any
    }

    /// Publish one event. Returns the number of subscribers it was enqueued for.
    /// Never blocks: only the bus lock and the per-subscriber queue locks are taken.
    pub fn publish(&self, ev: Event) -> usize {
        let inner = self.inner.lock().unwrap();
        for idx in dispatch_order(&inner.subs, &ev) {
            let sub = &inner.subs[idx];
            sub.queue.enqueue(ev.clone(), sub.id, &sub.wire);
        }
        inner.subs.iter().filter(|s| s.pattern.matches(&ev)).count()
    }

    /// Number of live subscriptions.
    pub fn subscriber_count(&self) -> usize {
        self.inner.lock().unwrap().subs.len()
    }

    /// Total drops across all subscribers (telemetry/tests).
    pub fn total_dropped(&self) -> u64 {
        self.inner
            .lock()
            .unwrap()
            .subs
            .iter()
            .map(|s| s.queue.state.lock().unwrap().dropped)
            .sum()
    }

    fn spawn_drainer(
        bus: Arc<Mutex<BusInner>>,
        id: u64,
        queue: Arc<SubQueue>,
        sink: Arc<dyn Sink>,
        wire: String,
    ) {
        std::thread::Builder::new()
            .name(format!("ds-router-#{id}"))
            .spawn(move || {
                loop {
                    let ev = {
                        let mut st = queue.state.lock().unwrap();
                        loop {
                            if queue.is_stopped() {
                                return;
                            }
                            if let Some(ev) = st.items.pop_front() {
                                break ev;
                            }
                            let (s, _) = queue
                                .cond
                                .wait_timeout(st, DRAIN_POLL)
                                .expect("router queue condvar");
                            st = s;
                        }
                    };
                    // Report accumulated drops before the event (the dropped events are
                    // older than everything still queued).
                    let dropped = {
                        let mut st = queue.state.lock().unwrap();
                        if st.dropped > st.notified {
                            let d = st.dropped - st.notified;
                            st.notified = st.dropped;
                            Some(d)
                        } else {
                            None
                        }
                    };
                    if let Some(d) = dropped {
                        let msg = json!({"op": "dropped", "path": wire, "count": d});
                        if sink.deliver(&msg).is_err() {
                            Self::remove_dead(&bus, id, &wire);
                            return;
                        }
                    }
                    if sink.deliver(&ev.wire()).is_err() {
                        Self::remove_dead(&bus, id, &wire);
                        return;
                    }
                }
            })
            .expect("spawn ds-router drainer thread");
    }

    fn remove_dead(bus: &Arc<Mutex<BusInner>>, id: u64, wire: &str) {
        let mut inner = bus.lock().unwrap();
        if let Some(pos) = inner.subs.iter().position(|s| s.id == id) {
            let sub = inner.subs.remove(pos);
            sub.queue.stop();
            lifecycle::log(
                "info",
                "ds-daemon::router",
                &format!("subscriber #{id} ({wire}) is gone; subscription removed"),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::topics::TopicPattern;
    use serde_json::json;
    use std::sync::mpsc::channel;

    fn topic_pattern(p: &str) -> Pattern {
        Pattern::Topic(TopicPattern::parse(p).unwrap())
    }

    fn state_pattern(p: &str) -> Pattern {
        let path = ds_state::Path::parse(p).unwrap();
        path.validate_read().unwrap();
        Pattern::State(path)
    }

    /// A sink recording every delivered message, with a channel to read them back.
    fn recording_sink() -> (Arc<FnSink>, std::sync::mpsc::Receiver<Value>) {
        let (tx, rx) = channel();
        (
            Arc::new(FnSink(Arc::new(move |msg: &Value| {
                let _ = tx.send(msg.clone());
            }))),
            rx,
        )
    }

    fn recv_timeout(rx: &std::sync::mpsc::Receiver<Value>) -> Option<Value> {
        rx.recv_timeout(Duration::from_secs(2)).ok()
    }

    #[test]
    fn state_event_wire_format_is_unchanged() {
        let ev = Event::state(
            "/controls/keys/0/image",
            json!({"kind": "color", "value": "#3cc86e"}),
            Some(json!({"kind": "color", "value": "#000000"})),
            42,
        );
        let w = ev.wire();
        assert_eq!(w["op"], "event");
        assert_eq!(w["path"], "/controls/keys/0/image");
        assert_eq!(w["rev"], 42);
        assert_eq!(w["value"]["value"], "#3cc86e");
        assert_eq!(w["prev"]["value"], "#000000");
        assert!(
            w.get("ts_ms").is_none(),
            "state events keep the exact old shape"
        );
        assert!(w.get("topic").is_none());
        // schema-valid server message
        ds_ipc::message::validate_server_message(&w).unwrap();
    }

    #[test]
    fn topic_event_wire_format() {
        let ev = Event::topic(
            "deck/control/keys.0/down",
            json!({"key": 0, "pressed": true}),
        );
        let w = ev.wire();
        assert_eq!(w["op"], "event");
        assert_eq!(w["topic"], "deck/control/keys.0/down");
        assert_eq!(w["path"], "/deck/control/keys.0/down");
        assert_eq!(w["payload"]["pressed"], true);
        assert!(w.get("rev").is_none());
        assert!(w["ts_ms"].is_u64());
        ds_ipc::message::validate_server_message(&w).unwrap();
    }

    #[test]
    fn exact_and_wildcard_subscribers_both_receive_and_dispatch_order_is_exact_first() {
        let bus = Bus::new();
        let (sink_w, rx_w) = recording_sink();
        let (sink_e, rx_e) = recording_sink();
        // wildcard subscribed FIRST (lower id), exact second
        let wildcard = bus.subscribe(topic_pattern("deck/*"), None, "/deck/*".into(), sink_w);
        let exact = bus.subscribe(
            topic_pattern("deck/control/keys.0/down"),
            None,
            "/deck/control/keys.0/down".into(),
            sink_e,
        );
        assert_ne!(wildcard, exact);
        // both receive the event
        let n = bus.publish(Event::topic("deck/control/keys.0/down", json!({"k": 1})));
        assert_eq!(n, 2);
        assert_eq!(
            recv_timeout(&rx_w).unwrap()["topic"],
            "deck/control/keys.0/down"
        );
        assert_eq!(
            recv_timeout(&rx_e).unwrap()["topic"],
            "deck/control/keys.0/down"
        );
        // a non-matching event reaches neither
        assert_eq!(bus.publish(Event::topic("app/x/y", json!({}))), 0);
        // dispatch order: exact patterns before wildcard patterns, id order within a group
        let inner = bus.inner.lock().unwrap();
        let order = dispatch_order(
            &inner.subs,
            &Event::topic("deck/control/keys.0/down", json!({})),
        );
        drop(inner);
        // subs are stored in insertion order and ids start at 1, so index == id - 1
        assert_eq!(
            order,
            vec![(exact - 1) as usize, (wildcard - 1) as usize],
            "exact pattern must be enqueued before the wildcard pattern"
        );
    }

    #[test]
    fn two_subscribers_both_receive_and_unsubscribe_stops_delivery() {
        let bus = Bus::new();
        let (sink_a, rx_a) = recording_sink();
        let (sink_b, rx_b) = recording_sink();
        let id_a = bus.subscribe(
            state_pattern("/controls/*"),
            None,
            "/controls/*".into(),
            sink_a,
        );
        let _id_b = bus.subscribe(
            state_pattern("/controls/*"),
            None,
            "/controls/*".into(),
            sink_b,
        );
        assert_eq!(bus.subscriber_count(), 2);
        bus.publish(Event::state("/controls/keys/3/label", json!("x"), None, 7));
        for rx in [&rx_a, &rx_b] {
            let msg = recv_timeout(rx).expect("both subscribers receive");
            assert_eq!(msg["op"], "event");
            assert_eq!(msg["path"], "/controls/keys/3/label");
            assert_eq!(msg["rev"], 7);
            assert_eq!(msg["value"], "x");
        }
        assert!(bus.unsubscribe(id_a));
        bus.publish(Event::state("/controls/keys/4/label", json!("y"), None, 8));
        assert!(
            recv_timeout(&rx_a).is_none(),
            "unsubscribed sink gets nothing"
        );
        let msg = recv_timeout(&rx_b).expect("remaining subscriber still receives");
        assert_eq!(msg["rev"], 8);
        assert!(!bus.unsubscribe(id_a), "double unsubscribe is false");
    }

    #[test]
    fn state_patterns_use_ds_state_prefix_semantics() {
        let bus = Bus::new();
        let (sink, rx) = recording_sink();
        bus.subscribe(
            state_pattern("/controls/keys/*"),
            None,
            "/controls/keys/*".into(),
            sink,
        );
        // `/controls/keys/*` matches `/controls/keys/0/image` (prefix + one `*` segment)
        assert_eq!(
            bus.publish(Event::state("/controls/keys/0/image", json!(1), None, 1)),
            1
        );
        assert!(recv_timeout(&rx).is_some());
        // but not `/settings/x`
        assert_eq!(
            bus.publish(Event::state("/settings/x", json!(1), None, 2)),
            0
        );
    }

    #[test]
    fn topic_patterns_do_not_match_state_events_and_vice_versa() {
        let bus = Bus::new();
        let (sink_t, rx_t) = recording_sink();
        let (sink_s, rx_s) = recording_sink();
        bus.subscribe(topic_pattern("deck/*"), None, "/deck/*".into(), sink_t);
        bus.subscribe(state_pattern("/controls"), None, "/controls".into(), sink_s);
        assert_eq!(bus.publish(Event::topic("deck/system/start", json!({}))), 1);
        assert_eq!(
            bus.publish(Event::state("/controls/keys/0", json!({}), None, 1)),
            1
        );
        assert!(recv_timeout(&rx_t).is_some());
        assert!(recv_timeout(&rx_s).is_some());
        assert!(recv_timeout(&rx_t).is_none());
        assert!(recv_timeout(&rx_s).is_none());
    }

    #[test]
    fn queue_drops_oldest_with_count_at_the_cap() {
        let q = SubQueue::new();
        for i in 0..SUBSCRIBER_QUEUE_CAP {
            q.enqueue(Event::topic("app/x/y", json!(i)), 1, "/app/*");
            assert_eq!(q.state.lock().unwrap().dropped, 0);
        }
        assert_eq!(q.state.lock().unwrap().items.len(), SUBSCRIBER_QUEUE_CAP);
        // overflow: drop oldest, count
        q.enqueue(Event::topic("app/x/y", json!("last")), 1, "/app/*");
        let st = q.state.lock().unwrap();
        assert_eq!(st.items.len(), SUBSCRIBER_QUEUE_CAP);
        assert_eq!(st.dropped, 1);
        // the oldest (0) was dropped, the newest is at the back
        assert_eq!(st.items.front().unwrap().wire()["payload"], json!(1));
        assert_eq!(st.items.back().unwrap().wire()["payload"], json!("last"));
        drop(st);
        q.enqueue(Event::topic("app/x/y", json!("last2")), 1, "/app/*");
        assert_eq!(q.state.lock().unwrap().dropped, 2);
    }

    #[test]
    fn drainer_reports_dropped_counts_before_events() {
        // Fill a queue directly (no drainer running yet), then attach it to a bus sub and
        // let the drainer run: the dropped notification must come before the events.
        let q = Arc::new(SubQueue::new());
        for i in 0..(SUBSCRIBER_QUEUE_CAP + 50) {
            q.enqueue(Event::topic("app/x/y", json!(i)), 9, "/app/*");
        }
        assert_eq!(q.state.lock().unwrap().dropped, 50);
        let (sink, rx) = recording_sink();
        let inner = Arc::new(Mutex::new(BusInner {
            next_id: 10,
            subs: vec![Sub {
                id: 9,
                pattern: topic_pattern("app/*"),
                wire: "/app/*".into(),
                conn_id: None,
                queue: q.clone(),
            }],
        }));
        let bus = Bus { inner };
        Bus::spawn_drainer(bus.inner.clone(), 9, q, sink, "/app/*".into());
        // first message must be the dropped notification (50), then events from the cap on
        let first = recv_timeout(&rx).expect("dropped notification first");
        assert_eq!(first["op"], "dropped");
        assert_eq!(first["path"], "/app/*");
        assert_eq!(first["count"], 50);
        ds_ipc::message::validate_server_message(&first).unwrap();
        let ev = recv_timeout(&rx).expect("then an event");
        assert_eq!(ev["op"], "event");
        assert_eq!(ev["topic"], "app/x/y");
    }

    #[test]
    fn duplicate_subscribe_same_conn_and_wire_returns_existing_id() {
        let bus = Bus::new();
        let (sink, _rx) = recording_sink();
        let a = bus.subscribe(
            topic_pattern("app/*"),
            Some(7),
            "/app/*".into(),
            sink.clone(),
        );
        let b = bus.subscribe(topic_pattern("app/*"), Some(7), "/app/*".into(), sink);
        assert_eq!(a, b);
        assert_eq!(bus.subscriber_count(), 1);
        // a different connection gets its own subscription
        let (sink2, _rx2) = recording_sink();
        let c = bus.subscribe(topic_pattern("app/*"), Some(8), "/app/*".into(), sink2);
        assert_ne!(a, c);
        assert_eq!(bus.subscriber_count(), 2);
        // unsubscribe by (conn, wire) removes only that connection's subscription
        assert!(bus.unsubscribe_wire(7, "/app/*"));
        assert_eq!(bus.subscriber_count(), 1);
        assert!(!bus.unsubscribe_wire(7, "/app/*"));
    }

    #[test]
    fn blocked_subscriber_never_stalls_publish() {
        // A sink whose drainer blocks forever (a client that never reads): the bus must
        // keep accepting publishes, the queue must cap at SUBSCRIBER_QUEUE_CAP with
        // drop-oldest + counting, and publish must stay fast.
        let bus = Bus::new();
        let (gate_tx, gate_rx) = channel::<()>();
        let gate_rx = Mutex::new(gate_rx);
        let sink = Arc::new(FnSink(Arc::new(move |_msg: &Value| {
            // block until the test drops the gate
            let _ = gate_rx.lock().unwrap().recv();
        })));
        let id = bus.subscribe(topic_pattern("app/*"), None, "/app/*".into(), sink);
        let start = std::time::Instant::now();
        let total = SUBSCRIBER_QUEUE_CAP * 3 + 500;
        for i in 0..total {
            bus.publish(Event::topic("app/x/y", json!(i)));
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "publishing {total} events with a blocked subscriber took {elapsed:?}"
        );
        // the drainer popped at most one event into the blocked sink; everything else
        // went through the bounded queue -> drops counted
        let dropped = bus.total_dropped();
        assert!(
            dropped >= (total - SUBSCRIBER_QUEUE_CAP - 1) as u64,
            "dropped {dropped} >= {}",
            total - SUBSCRIBER_QUEUE_CAP - 1
        );
        drop(gate_tx); // unblock the drainer
        assert!(bus.unsubscribe(id));
    }
}
