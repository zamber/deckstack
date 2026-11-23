//! The frame service (DS-031): the in-daemon queues between the DSF1 stream socket and
//! the device writer threads (`docs/contracts/streaming.md` §3-§6,
//! `docs/components/frame-service.md`).
//!
//! - **Per-target queue, latest-frame-wins:** each target (`keys` | `key:N` | `dial`) has
//!   exactly one frame **in flight** plus one **waiting**. A newly accepted frame replaces
//!   only the waiting one and increments `telemetry.dropped`; the in-flight frame is never
//!   dropped, replaced or timed out — the writer sends it whole via `send_all`-equivalent
//!   chunked reports (streaming.md §3, S16/M8).
//! - **Global 256 MiB budget:** the sum of queued (waiting + in-flight) frame bytes across
//!   all targets never exceeds [`FRAME_QUEUE_BUDGET_BYTES`]. Beyond it the **oldest waiting
//!   frame is dropped and counted** (streaming.md §5); when only in-flight frames remain
//!   (which are never dropped) and the budget would still be exceeded, the new frame is
//!   refused with a typed `E_LIMIT` — the control plane is never killed by frame traffic.
//! - **Structural control-plane independence:** accepting a frame only takes the slots
//!   lock and pushes a wake signal — no device I/O and no state lock is ever held on the
//!   accept path; the actual encode + chunked send happens on the per-module writer thread
//!   ([`super::writer`]). The queue is bounded (depth 2 per target), so the IPC/stream
//!   task is never blocked by the device.
//! - **S1:** the service writes only non-durable `/telemetry/streams/<target>` samples
//!   (via [`super::telemetry`]) — no durable state, no `rev` bump, no frame bytes in the
//!   JSON state document.
//!
//! Wiring: [`FrameService::serve_stream_socket`] binds `<dir>/stream.sock` with the
//! DS-013 [`StreamServer`] and installs this service as its [`FrameSink`], so `DSctl
//! stream-open` sessions route accepted frames straight into the target queues;
//! `stream-stop` removes the server-side session (further frames are nacked `E_SCOPE`)
//! and [`FrameService::stop_stream`] refuses new frames at the service while the writer
//! drains what is queued. The DS-095 state-push image path goes through
//! [`FrameService::push_image`] — the same queues, the same writer threads — instead of
//! pushing inline on the device task.

use super::{encode, telemetry, writer, FrameTarget};
use crate::lifecycle::{self, Shutdown};
use crate::router::Bus;
use ds_device::transport::Transport;
use ds_ipc::error::{ErrorCode, IpcError};
use ds_ipc::frames::codec::Frame;
use ds_ipc::frames::limits::MAX_QUEUE_MEMORY_BYTES;
use ds_ipc::frames::server::{FrameSink, StreamBus, StreamServer};
use ds_ipc::frames::StreamConfig;
use ds_protocol::ModuleKind;
use ds_state::Store;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

/// Global frame-queue memory budget (docs/contracts/streaming.md §5): **256 MiB** across
/// all targets (waiting + in-flight frame bytes).
pub const FRAME_QUEUE_BUDGET_BYTES: usize = MAX_QUEUE_MEMORY_BYTES;

/// Capacity of the per-module writer wake channel (bounded; a full channel only means the
/// writer is already awake and will rescan the queues).
pub const WRITER_SIGNAL_CAP: usize = 16;

/// One queued frame: the wire payload (or, for `pre_encoded`, the already-encoded device
/// JPEG) plus its producer sequence and arrival order (for oldest-waiting eviction).
#[derive(Debug, Clone)]
pub struct QueuedFrame {
    /// Producer-side sequence (meta `seq`; internal counter for state-push images).
    pub seq: u64,
    /// The frame bytes held against the queue budget.
    pub payload: Vec<u8>,
    /// `true` when `payload` is already the device wire JPEG (state-push path, DS-095).
    pub pre_encoded: bool,
    /// Global arrival order (monotonic) — picks the oldest waiting frame on eviction.
    pub arrival: u64,
}

/// The in-flight marker of a target: the writer owns the frame until it is fully sent.
#[derive(Debug, Clone, Copy)]
pub struct InFlight {
    pub seq: u64,
    pub arrival: u64,
    /// Bytes held against the budget while in flight.
    pub len: usize,
}

/// Per-target counters — the source of truth for `/telemetry/streams/<target>`.
#[derive(Debug)]
pub struct TargetStats {
    /// Frames fully sent to the device.
    pub sent: u64,
    /// Frames dropped (replaced waiting, evicted for the budget, encode/send failures).
    pub dropped: u64,
    /// Device-JPEG bytes sent.
    pub bytes: u64,
    pub last_seq: Option<u64>,
    pub last_sha256: Option<String>,
    pub last_frame_at: Option<Instant>,
    /// Ring of the last writer send durations (for `writer_ms_p99`).
    pub writer_ms: std::collections::VecDeque<std::time::Duration>,
    /// `sent` at the last fps-window reset (the ≤ 1 Hz sampler resets it).
    pub window_sent: u64,
    /// Start of the current fps window.
    pub window_start: Instant,
}

impl TargetStats {
    fn new() -> Self {
        Self {
            sent: 0,
            dropped: 0,
            bytes: 0,
            last_seq: None,
            last_sha256: None,
            last_frame_at: None,
            writer_ms: std::collections::VecDeque::with_capacity(129),
            window_sent: 0,
            window_start: Instant::now(),
        }
    }

    pub fn note_writer_ms(&mut self, d: std::time::Duration) {
        if self.writer_ms.len() >= 128 {
            self.writer_ms.pop_front();
        }
        self.writer_ms.push_back(d);
    }
}

/// The per-target queue slot: 1 in flight + 1 waiting + the stats.
#[derive(Debug)]
pub struct Slot {
    pub in_flight: Option<InFlight>,
    pub waiting: Option<QueuedFrame>,
    pub stats: TargetStats,
}

impl Slot {
    pub fn new() -> Self {
        Self {
            in_flight: None,
            waiting: None,
            stats: TargetStats::new(),
        }
    }
}

impl Default for Slot {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared service state. The slots lock is the ONLY lock the accept path takes, and it is
/// never held across device I/O or store writes.
pub struct ServiceInner {
    /// Telemetry sink (non-durable plane only; never bumps rev, S1). The daemon holds the
    /// store in an `Arc` shared with the state task; only `set_telemetry` is ever called.
    pub store: Arc<Store>,
    /// Shared device transport handle (the writer threads own the send side per module).
    pub transport: Option<Arc<dyn Transport>>,
    /// Event bus (module-gone → `deck/device/<serial>/disconnected`).
    pub bus: Bus,
    pub shutdown: Shutdown,
    /// Set when the stream socket is served (credit re-grants as the queue drains).
    pub stream_bus: Mutex<Option<StreamBus>>,
    /// Per-target queues + stats.
    pub slots: Mutex<HashMap<FrameTarget, Slot>>,
    /// Per-module writer wake channels.
    pub signals: Mutex<HashMap<ModuleKind, SyncSender<()>>>,
    /// Spawned writer threads (one per module).
    pub writers: Mutex<HashMap<ModuleKind, JoinHandle<()>>>,
    /// Targets whose stream was stopped (no new frames accepted; queue drains).
    pub stopped: Mutex<HashSet<FrameTarget>>,
    /// Bytes currently held in queues (waiting + in-flight), against the budget.
    pub queued_bytes: AtomicUsize,
    /// Monotonic arrival counter.
    pub arrival: AtomicU64,
    /// Internal seq for state-push images (DS-095 path).
    pub image_seq: AtomicU64,
    /// Global frame-queue budget in bytes.
    pub budget: usize,
    /// Telemetry sampler thread.
    pub sampler: Mutex<Option<JoinHandle<()>>>,
}

/// The frame service handle (cloneable; installs as the stream server's [`FrameSink`]).
#[derive(Clone)]
pub struct FrameService {
    pub(crate) inner: Arc<ServiceInner>,
}

impl FrameService {
    /// Spawn the service with the contract budget (256 MiB): queues + writer threads +
    /// the ≤ 1 Hz telemetry sampler.
    pub fn spawn(
        store: Arc<Store>,
        transport: Option<Arc<dyn Transport>>,
        bus: Bus,
        shutdown: Shutdown,
    ) -> Self {
        Self::spawn_with_budget(store, transport, bus, shutdown, FRAME_QUEUE_BUDGET_BYTES)
    }

    /// Spawn with an explicit queue budget (tests use a small budget to exercise the cap).
    pub fn spawn_with_budget(
        store: Arc<Store>,
        transport: Option<Arc<dyn Transport>>,
        bus: Bus,
        shutdown: Shutdown,
        budget: usize,
    ) -> Self {
        let inner = Arc::new(ServiceInner {
            store,
            transport,
            bus,
            shutdown,
            stream_bus: Mutex::new(None),
            slots: Mutex::new(HashMap::new()),
            signals: Mutex::new(HashMap::new()),
            writers: Mutex::new(HashMap::new()),
            stopped: Mutex::new(HashSet::new()),
            queued_bytes: AtomicUsize::new(0),
            arrival: AtomicU64::new(0),
            image_seq: AtomicU64::new(0),
            budget,
            sampler: Mutex::new(None),
        });
        let sampler = telemetry::spawn_sampler(Arc::clone(&inner));
        *inner.sampler.lock().unwrap() = Some(sampler);
        Self { inner }
    }

    /// Bind `<dir>/stream.sock` (0600, dir 0700) with the DS-013 stream server and install
    /// this service as its sink: `DSctl stream-open` sessions route accepted DSF1 frames
    /// into the target queues. Returns the [`StreamBus`] (credits/events/stats) and the
    /// socket path.
    pub fn serve_stream_socket(
        &self,
        dir: &Path,
        config: StreamConfig,
    ) -> std::io::Result<(StreamBus, PathBuf)> {
        let sock = dir.join("stream.sock");
        let server = StreamServer::bind(&sock, config)?;
        let sink: Arc<dyn FrameSink> = Arc::new(self.clone());
        let bus = server.start(sink)?;
        *self.inner.stream_bus.lock().unwrap() = Some(bus.clone());
        Ok((bus, sock))
    }

    /// Enqueue one frame for `target` (the accept path — lock + signal only, never blocks
    /// on the device). Latest-frame-wins: replaces only the waiting frame; refuses with a
    /// typed error when stopped (`E_SCOPE`) or when the global budget would be exceeded
    /// and only in-flight frames remain (`E_LIMIT`).
    pub fn push_frame(
        &self,
        target: FrameTarget,
        payload: Vec<u8>,
        seq: u64,
        pre_encoded: bool,
    ) -> Result<(), IpcError> {
        let len = payload.len();
        if self.inner.stopped.lock().unwrap().contains(&target) {
            return Err(IpcError::new(
                ErrorCode::EScope,
                format!(
                    "stream for target '{}' is stopped (stream-stop): no new frames accepted",
                    target.wire_name()
                ),
            ));
        }
        let mut slots = self.inner.slots.lock().unwrap();
        // Global budget: evict the oldest waiting frame across all targets while the new
        // frame would exceed it. In-flight frames are never evicted, so if none is waiting
        // and the budget is still exceeded, refuse the new frame with E_LIMIT.
        loop {
            let total = self.inner.queued_bytes.load(Ordering::SeqCst);
            if total.saturating_add(len) <= self.inner.budget {
                break;
            }
            let victim = slots
                .iter()
                .filter(|(_, s)| s.waiting.is_some())
                .min_by_key(|(_, s)| s.waiting.as_ref().map(|w| w.arrival).unwrap_or(u64::MAX))
                .map(|(t, _)| *t);
            match victim {
                Some(vt) => {
                    let slot = slots.get_mut(&vt).expect("victim slot exists");
                    let old = slot.waiting.take().expect("victim has a waiting frame");
                    slot.stats.dropped += 1;
                    self.inner
                        .queued_bytes
                        .fetch_sub(old.payload.len(), Ordering::SeqCst);
                    lifecycle::log(
                        "warn",
                        "ds-daemon::frames::service",
                        &format!(
                            "frame-queue budget {} bytes: dropped oldest waiting frame seq {} for target {}",
                            self.inner.budget,
                            old.seq,
                            vt.wire_name()
                        ),
                    );
                }
                None => {
                    return Err(IpcError::new(
                        ErrorCode::ELimit,
                        format!(
                            "global frame-queue budget of {} bytes would be exceeded and only \
                             in-flight frames remain (in-flight frames are never dropped)",
                            self.inner.budget
                        ),
                    ));
                }
            }
        }
        let mut frame = QueuedFrame {
            seq,
            payload,
            pre_encoded,
            arrival: 0,
        };
        frame.arrival = self.inner.arrival.fetch_add(1, Ordering::SeqCst);
        self.inner.queued_bytes.fetch_add(len, Ordering::SeqCst);
        let slot = slots.entry(target).or_default();
        if let Some(old) = slot.waiting.replace(frame) {
            // Latest-frame-wins: the new frame replaces only the waiting one (counted).
            self.inner
                .queued_bytes
                .fetch_sub(old.payload.len(), Ordering::SeqCst);
            slot.stats.dropped += 1;
        }
        let module = target.module();
        drop(slots);
        self.ensure_writer(module);
        if let Some(tx) = self.inner.signals.lock().unwrap().get(&module) {
            let _ = tx.try_send(());
        }
        Ok(())
    }

    /// The DS-095 state-push image path: encode the wire image to the device format now
    /// (a refusal is `E_FORMAT: …`, never forwarded), then enqueue the encoded JPEG through
    /// the same per-target queue and writer threads as stream frames.
    pub fn push_image(&self, target: FrameTarget, wire: &[u8]) -> Result<(), String> {
        let jpeg = encode::encode_frame(target, wire).map_err(|e| format!("{}: {e}", e.code()))?;
        let seq = self.inner.image_seq.fetch_add(1, Ordering::SeqCst);
        self.push_frame(target, jpeg, seq, true)
            .map_err(|e| e.message)
    }

    /// Stop accepting new frames for `target` (the service side of `stream-stop`); the
    /// writer drains the queued frames and then the target goes quiet.
    pub fn stop_stream(&self, target: FrameTarget) {
        self.inner.stopped.lock().unwrap().insert(target);
        lifecycle::log(
            "info",
            "ds-daemon::frames::service",
            &format!(
                "stream for target {} stopped: draining the queue, refusing new frames",
                target.wire_name()
            ),
        );
    }

    /// Current stats snapshot for one target (the `stream-stats` op shape, no rev).
    pub fn stats(&self, target: FrameTarget) -> Option<Value> {
        let slots = self.inner.slots.lock().unwrap();
        let slot = slots.get(&target)?;
        Some(telemetry::stats_value(
            &target.wire_name(),
            slot,
            Instant::now(),
            &lifecycle::rfc3339_utc(lifecycle::now_secs()),
        ))
    }

    /// Current stats snapshots for all known targets.
    pub fn all_stats(&self) -> Value {
        let slots = self.inner.slots.lock().unwrap();
        let now = Instant::now();
        let at = lifecycle::rfc3339_utc(lifecycle::now_secs());
        Value::Object(
            slots
                .iter()
                .map(|(t, s)| {
                    (
                        t.wire_name(),
                        telemetry::stats_value(&t.wire_name(), s, now, &at),
                    )
                })
                .collect(),
        )
    }

    /// Introspection for tests/stats: (in_flight seq, waiting seq, sent, dropped, queued bytes).
    pub fn queue_state(&self, target: FrameTarget) -> (Option<u64>, Option<u64>, u64, u64, usize) {
        let slots = self.inner.slots.lock().unwrap();
        let s = slots.get(&target);
        match s {
            None => (
                None,
                None,
                0,
                0,
                self.inner.queued_bytes.load(Ordering::SeqCst),
            ),
            Some(slot) => (
                slot.in_flight.map(|f| f.seq),
                slot.waiting.as_ref().map(|w| w.seq),
                slot.stats.sent,
                slot.stats.dropped,
                self.inner.queued_bytes.load(Ordering::SeqCst),
            ),
        }
    }

    /// Write one telemetry sample now (the sampler does this ≤ 1 Hz; exposed for tests and
    /// for a final sample at shutdown).
    pub fn sample_now(&self) {
        telemetry::sample_once(&self.inner);
    }

    /// Join the writer threads and the sampler (call after `shutdown.trigger()`; the
    /// writers drain their queues first — graceful stop).
    pub fn join(&self) {
        let writers: Vec<JoinHandle<()>> = self
            .inner
            .writers
            .lock()
            .unwrap()
            .drain()
            .map(|(_, h)| h)
            .collect();
        for h in writers {
            let _ = h.join();
        }
        if let Some(h) = self.inner.sampler.lock().unwrap().take() {
            let _ = h.join();
        }
    }

    /// Spawn the per-module writer thread on first use of the module.
    fn ensure_writer(&self, module: ModuleKind) {
        let mut writers = self.inner.writers.lock().unwrap();
        if writers.contains_key(&module) {
            return;
        }
        let (tx, rx) = std::sync::mpsc::sync_channel::<()>(WRITER_SIGNAL_CAP);
        self.inner.signals.lock().unwrap().insert(module, tx);
        let inner = Arc::clone(&self.inner);
        let handle = std::thread::Builder::new()
            .name(format!("ds-frames-writer-{module:?}"))
            .spawn(move || writer::run(inner, module, rx))
            .expect("spawn frame writer thread");
        writers.insert(module, handle);
    }
}

impl FrameSink for FrameService {
    fn on_frame(&self, target: &str, frame: &Frame) -> Result<(), IpcError> {
        let Some(ft) = FrameTarget::parse(target) else {
            return Err(IpcError::new(
                ErrorCode::ESchema,
                format!("unknown stream target {target:?}"),
            ));
        };
        self.push_frame(ft, frame.payload.clone(), frame.meta.seq, false)
    }
}

/// A small test/telemetry helper: the stats object is also exposed as `/telemetry/streams`.
pub fn streams_path(target: FrameTarget) -> String {
    format!("/telemetry/streams/{}", target.wire_name())
}

/// Keep `json` import used for doc-shape parity checks in tests.
#[allow(dead_code)]
fn _shape() -> Value {
    json!({})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frames::encode;
    use ds_device::transport::DeviceEvent;
    use ds_ipc::frames::limits::MAX_PAYLOAD_BYTES;
    use ds_protocol::Report;
    use mh_device::ModuleId;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::mpsc::Receiver;
    use std::sync::{Condvar, Mutex as StdMutex};
    use std::time::Duration;

    /// A mock transport whose `send` blocks until the test opens the gate — so the writer
    /// holds a frame in flight while the test pushes more frames.
    struct GateTransport {
        sent: StdMutex<Vec<(ModuleId, Vec<u8>)>>,
        open: StdMutex<bool>,
        cv: Condvar,
    }

    impl GateTransport {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                sent: StdMutex::new(Vec::new()),
                open: StdMutex::new(false),
                cv: Condvar::new(),
            })
        }

        fn open(&self) {
            let mut g = self.open.lock().unwrap();
            *g = true;
            self.cv.notify_all();
        }

        /// Reassemble the chunked image transfers (cmd 0x36) into whole payloads.
        fn transfers(&self, cmd: u8, index: u8) -> Vec<Vec<u8>> {
            let sent = self.sent.lock().unwrap();
            let mut out: Vec<Vec<u8>> = Vec::new();
            let mut cur: Vec<u8> = Vec::new();
            for (_, raw) in sent.iter() {
                if raw.len() < 8 || raw[0] != 0x02 || raw[1] != cmd || raw[6] != index {
                    continue;
                }
                let len = u16::from_le_bytes([raw[4], raw[5]]) as usize;
                cur.extend_from_slice(&raw[8..8 + len]);
                if raw[7] == 1 {
                    out.push(std::mem::take(&mut cur));
                }
            }
            out
        }
    }

    impl Transport for GateTransport {
        fn send(&self, module: &ModuleId, report: &Report) -> ds_device::Result<usize> {
            let bytes = report.bytes().to_vec();
            let mut g = self.open.lock().unwrap();
            while !*g {
                g = self.cv.wait(g).unwrap();
            }
            drop(g);
            self.sent
                .lock()
                .unwrap()
                .push((module.clone(), bytes.clone()));
            Ok(bytes.len())
        }
        fn query(
            &self,
            _module: &ModuleId,
            _feature_id: u8,
            _timeout: Duration,
        ) -> ds_device::Result<Vec<u8>> {
            Ok(vec![])
        }
        fn recv(&self, _timeout: Duration) -> ds_device::Result<DeviceEvent> {
            Err(ds_device::Error::Timeout(Duration::from_millis(1)))
        }
        fn close(&self) -> ds_device::Result<()> {
            Ok(())
        }
        fn module_id(&self, kind: ds_protocol::ModuleKind) -> Option<ModuleId> {
            // the gate stands in for a whole deck: every kind resolves to its virtual id
            Some(ds_device::vtransport::virtual_module_id(kind))
        }
    }

    fn test_service(
        transport: Option<Arc<dyn Transport>>,
        budget: usize,
    ) -> (FrameService, Arc<Store>, Shutdown) {
        let store = Arc::new(Store::fresh());
        let shutdown = Shutdown::manual();
        let svc = FrameService::spawn_with_budget(
            store.clone(),
            transport,
            Bus::new(),
            shutdown.clone(),
            budget,
        );
        (svc, store, shutdown)
    }

    fn frame(seq: u64, len: usize) -> Vec<u8> {
        // pre-encoded payload: arbitrary bytes, the writer sends them as-is
        let mut v = vec![0u8; len];
        v[0] = (seq & 0xff) as u8;
        v
    }

    fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration, what: &str) {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if cond() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out after {timeout:?} waiting for {what}");
    }

    #[test]
    fn latest_frame_wins_replaces_only_waiting_and_in_flight_is_never_dropped() {
        let gate = GateTransport::new();
        let (svc, _store, shutdown) = test_service(Some(gate.clone()), FRAME_QUEUE_BUDGET_BYTES);
        let target = FrameTarget::Key(0);
        // seq 0 goes in flight and blocks on the closed gate
        svc.push_frame(target, frame(0, 64), 0, true).unwrap();
        wait_until(
            || svc.queue_state(target).0 == Some(0),
            Duration::from_secs(5),
            "frame 0 to be in flight",
        );
        // seq 1..3 arrive while 0 is in flight: they replace only the waiting one
        for seq in 1..=3u64 {
            svc.push_frame(target, frame(seq, 64), seq, true).unwrap();
        }
        let (in_flight, waiting, _sent, dropped, _q) = svc.queue_state(target);
        assert_eq!(in_flight, Some(0), "the in-flight frame is never dropped");
        assert_eq!(waiting, Some(3), "the latest frame waits");
        assert_eq!(dropped, 2, "seq 1 and 2 were replaced (counted)");
        // open the gate: the writer sends 0 whole, then 3 — never 1 or 2
        gate.open();
        wait_until(
            || {
                let (_, _, sent, _, q) = svc.queue_state(target);
                sent == 2 && q == 0
            },
            Duration::from_secs(5),
            "both frames to be sent",
        );
        let (_, _, sent, dropped, _) = svc.queue_state(target);
        assert_eq!(sent, 2);
        assert_eq!(dropped, 2);
        let transfers = gate.transfers(0x36, 0);
        assert_eq!(transfers.len(), 2, "two whole transfers");
        assert_eq!(transfers[0], frame(0, 64));
        assert_eq!(
            transfers[1],
            frame(3, 64),
            "the final frame is the last one"
        );
        shutdown.trigger();
        svc.join();
    }

    #[test]
    fn budget_refusal_is_typed_elimit_and_oldest_waiting_is_evicted_counted() {
        let gate = GateTransport::new();
        // budget 1000 bytes: an in-flight 600 B frame leaves room for only 400 B more
        let (svc, _store, shutdown) = test_service(Some(gate.clone()), 1000);
        let target = FrameTarget::Key(1);
        svc.push_frame(target, frame(0, 600), 0, true).unwrap();
        wait_until(
            || svc.queue_state(target).0 == Some(0),
            Duration::from_secs(5),
            "frame 0 in flight",
        );
        // 600 + 600 > 1000 and nothing is waiting (in-flight is never evicted) -> E_LIMIT
        let err = svc.push_frame(target, frame(1, 600), 1, true).unwrap_err();
        assert_eq!(err.code, ErrorCode::ELimit, "{err:?}");
        // 600 + 300 <= 1000 -> accepted as waiting
        svc.push_frame(target, frame(2, 300), 2, true).unwrap();
        assert_eq!(svc.queue_state(target).1, Some(2));
        // another 300 B would exceed the budget -> the oldest waiting (seq 2) is evicted
        svc.push_frame(target, frame(3, 300), 3, true).unwrap();
        let (in_flight, waiting, _sent, dropped, q) = svc.queue_state(target);
        assert_eq!(in_flight, Some(0));
        assert_eq!(waiting, Some(3));
        assert_eq!(dropped, 1, "the evicted waiting frame is counted");
        assert_eq!(q, 900, "600 in flight + 300 waiting");
        gate.open();
        wait_until(
            || {
                let (_, _, sent, _, q) = svc.queue_state(target);
                sent == 2 && q == 0
            },
            Duration::from_secs(5),
            "drain",
        );
        let transfers = gate.transfers(0x36, 1);
        assert_eq!(transfers.len(), 2);
        assert_eq!(transfers[0], frame(0, 600));
        assert_eq!(
            transfers[1],
            frame(3, 300),
            "the refused/evicted frames never sent"
        );
        shutdown.trigger();
        svc.join();
    }

    #[test]
    fn stopped_stream_refuses_new_frames_with_typed_error_and_drains() {
        let gate = GateTransport::new();
        let (svc, _store, shutdown) = test_service(Some(gate.clone()), FRAME_QUEUE_BUDGET_BYTES);
        let target = FrameTarget::Dial;
        svc.push_frame(target, frame(0, 32), 0, true).unwrap();
        svc.stop_stream(target);
        let err = svc.push_frame(target, frame(1, 32), 1, true).unwrap_err();
        assert_eq!(err.code, ErrorCode::EScope, "{err:?}");
        assert!(err.message.contains("stopped"), "{err:?}");
        gate.open();
        wait_until(
            || svc.queue_state(target).2 == 1,
            Duration::from_secs(5),
            "the queued frame to drain",
        );
        let (_, _, sent, dropped, _) = svc.queue_state(target);
        assert_eq!((sent, dropped), (1, 0), "stop drains, it does not drop");
        assert_eq!(gate.transfers(0x45, 0), vec![frame(0, 32)]);
        shutdown.trigger();
        svc.join();
    }

    #[test]
    fn no_transport_drops_and_counts_frames() {
        let (svc, _store, shutdown) = test_service(None, FRAME_QUEUE_BUDGET_BYTES);
        let target = FrameTarget::Panel;
        svc.push_frame(target, frame(0, 32), 0, true).unwrap();
        wait_until(
            || svc.queue_state(target).3 >= 1,
            Duration::from_secs(5),
            "frame dropped without a transport",
        );
        let (_, _, sent, dropped, q) = svc.queue_state(target);
        assert_eq!((sent, dropped, q), (0, 1, 0));
        shutdown.trigger();
        svc.join();
    }

    #[test]
    fn telemetry_sample_shape_no_rev_and_no_frame_bytes_in_state() {
        let gate = GateTransport::new();
        let (svc, store, shutdown) = test_service(Some(gate.clone()), FRAME_QUEUE_BUDGET_BYTES);
        let target = FrameTarget::Key(2);
        gate.open();
        svc.push_frame(target, frame(7, 48), 7, true).unwrap();
        wait_until(
            || svc.queue_state(target).2 == 1,
            Duration::from_secs(5),
            "frame sent",
        );
        svc.sample_now();
        let reply = store.get("/telemetry/streams/key:2").unwrap();
        assert_eq!(reply.rev, None, "telemetry reads carry no rev (S1)");
        let v = reply.value;
        for key in [
            "sent",
            "dropped",
            "fps",
            "bytes",
            "queue",
            "in_flight",
            "writer_ms_p99",
            "sampled_at",
            "last_sha256",
            "last_seq",
        ] {
            assert!(v.get(key).is_some(), "missing {key} in {v}");
        }
        assert_eq!(v["sent"], json!(1));
        assert_eq!(v["dropped"], json!(0));
        assert_eq!(v["queue"], json!(0));
        assert_eq!(v["in_flight"], json!(false));
        assert_eq!(v["last_seq"], json!(7));
        let sha = v["last_sha256"].as_str().unwrap();
        assert_eq!(sha.len(), 64, "{sha}");
        assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(v["sampled_at"].as_str().unwrap().contains('T'));
        assert_eq!(store.rev(), 0, "sampling never bumps rev (S1)");
        assert_eq!(
            store.durable_document()["telemetry"],
            json!({}),
            "telemetry is emptied in the persisted view (never persisted)"
        );
        let durable = store.durable_document().to_string();
        assert!(
            !durable.contains(&format!("{}", 7)),
            "no frame bytes in durable state"
        );
        // the service-level stats view has the same shape
        let s = svc.stats(target).unwrap();
        assert_eq!(s["sent"], json!(1));
        assert!(svc.all_stats().get("key:2").is_some());
        shutdown.trigger();
        svc.join();
    }

    #[test]
    fn push_image_encodes_now_and_refuses_non_images_with_e_format() {
        let gate = GateTransport::new();
        let (svc, _store, shutdown) = test_service(Some(gate.clone()), FRAME_QUEUE_BUDGET_BYTES);
        let target = FrameTarget::Key(0);
        gate.open();
        // a real wire image (the frame plane's own JPEG output) encodes and is sent
        let wire = encode::encode_solid(target, [0x11, 0x22, 0x33]);
        svc.push_image(target, &wire).unwrap();
        wait_until(
            || svc.queue_state(target).2 == 1,
            Duration::from_secs(5),
            "state-push image sent",
        );
        let transfers = gate.transfers(0x36, 0);
        assert_eq!(transfers.len(), 1);
        assert_eq!(
            transfers[0],
            encode::encode_frame(target, &wire).unwrap(),
            "the state path pushes the re-encoded device JPEG through the same queue"
        );
        // a non-image is refused eagerly with E_FORMAT and never queued
        let err = svc.push_image(target, b"not an image").unwrap_err();
        assert!(err.starts_with("E_FORMAT"), "{err}");
        assert_eq!(svc.queue_state(target).1, None);
        shutdown.trigger();
        svc.join();
    }

    /// Helper for the vdev integration test below: one DSctl line + one DSF1 frame client.
    #[cfg(test)]
    pub(crate) struct StreamClient {
        pub reader: std::io::BufReader<std::os::unix::net::UnixStream>,
        pub w: std::os::unix::net::UnixStream,
    }

    #[cfg(test)]
    impl StreamClient {
        pub fn connect(sock: &Path) -> Self {
            let w = std::os::unix::net::UnixStream::connect(sock).unwrap();
            w.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
            let reader = std::io::BufReader::new(w.try_clone().unwrap());
            Self { reader, w }
        }

        pub fn dsctl(&mut self, v: &Value) {
            self.w.write_all(ds_ipc::frames::DSCTL_MAGIC).unwrap();
            let mut line = serde_json::to_vec(v).unwrap();
            line.push(b'\n');
            self.w.write_all(&line).unwrap();
            self.w.flush().unwrap();
        }

        pub fn recv(&mut self) -> Value {
            use std::io::{BufRead, Read};
            // the DSctl magic contains the \n, so consume the 6-byte magic first, then the line
            let mut magic = [0u8; 6];
            self.reader.read_exact(&mut magic).unwrap();
            assert_eq!(&magic, b"DSctl\n", "expected a DSctl reply");
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).unwrap();
            assert!(n > 0, "stream socket closed");
            serde_json::from_str(line.trim()).unwrap()
        }

        pub fn send_frame(&mut self, f: &Frame) {
            let bytes = f.encode().unwrap();
            self.w.write_all(&bytes).unwrap();
            self.w.flush().unwrap();
        }
    }

    #[cfg(test)]
    pub(crate) fn frame_meta(
        target: &str,
        seq: u64,
        format: ds_ipc::frames::FrameFormat,
    ) -> ds_ipc::frames::FrameMeta {
        ds_ipc::frames::FrameMeta {
            target: target.into(),
            seq,
            ts_ms: 1_732_000_000_000 + seq * 33,
            format,
            w: Some(72),
            h: Some(72),
            frames: Some(1),
            fps: None,
            loop_: None,
            crc32: 0,
            max_payload_bytes: MAX_PAYLOAD_BYTES as u32,
        }
    }

    /// Integration test against ds-vdev (DS-017/DS-020 stack, in-process): a throttled
    /// virtual device, the real DS-013 stream socket served by this service, 10 frames
    /// pushed faster than the device accepts them.
    #[test]
    fn vdev_integration_latest_frame_wins_dropped_counted_telemetry_no_rev() {
        use ds_device::uri::DeviceUri;
        use ds_vdev::{HubConfig, Throttle, VdevServer};

        let tmp = tempfile::tempdir().unwrap();
        // throttled device: 320 B per write takes 320/(64*125) = 40 ms
        let vdev_sock = tmp.path().join("vdev.sock");
        let vdev = VdevServer::start(
            &vdev_sock,
            HubConfig {
                modules: ds_protocol::ModuleKind::ALL.to_vec(),
                throttle: Throttle {
                    kbps: Some(64),
                    drop_after: None,
                },
            },
        )
        .unwrap();
        let uri = DeviceUri::parse(&format!("virtual:{}", vdev_sock.display())).unwrap();
        let transport: Arc<dyn Transport> = Arc::from(ds_device::open(&uri).unwrap());
        // software-mode handshake before any command (mh-protocol safety invariant)
        for kind in [ModuleKind::Keys, ModuleKind::Dial] {
            if let Some((report, _mode)) = ds_protocol::enter_software_mode(kind) {
                let id = ds_device::transport::module_id_for_send(&*transport, kind)
                    .expect("screen module connected on the transport");
                transport.send(&id, &report).unwrap();
            }
        }

        let store = Arc::new(Store::fresh());
        let shutdown = Shutdown::manual();
        let svc = FrameService::spawn(store.clone(), Some(transport), Bus::new(), shutdown.clone());
        let config = StreamConfig {
            min_frame_interval: Duration::ZERO, // the test pushes faster than 30 fps on purpose
            idle_timeout: None,
            ..StreamConfig::default()
        };
        let (_bus, stream_sock) = svc
            .serve_stream_socket(tmp.path(), config)
            .expect("bind stream.sock");
        assert_eq!(
            std::fs::metadata(&stream_sock)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        // producer: stream-open then 10 DSF1 frames as fast as the socket takes them
        let mut client = StreamClient::connect(&stream_sock);
        client.dsctl(&json!({"op": "stream-open", "target": "key:0", "credits": 100}));
        let credit = client.recv();
        assert_eq!(credit["op"], json!("stream-credit"));
        assert_eq!(credit["credits"], json!(100));

        let target = FrameTarget::Key(0);
        let mut payloads = Vec::new();
        for seq in 0..10u64 {
            // a distinct valid wire image per frame (the frame plane's own JPEG output)
            let payload = encode::encode_solid(target, [seq as u8, 0x40, 0x80]);
            payloads.push(payload.clone());
            let frame = Frame::new(
                frame_meta("key:0", seq, ds_ipc::frames::FrameFormat::Jpeg),
                &payload,
            );
            client.send_frame(&frame);
            let ack = client.recv();
            assert_eq!(ack["op"], json!("stream-ack"), "frame {seq}: {ack}");
            assert_eq!(ack["seq"], json!(seq));
        }

        // the throttled device can only accept ~2 of the 10 frames: the final frame on the
        // device must be the LAST pushed one (latest-frame-wins), dropped > 0
        let last_wire = &payloads[9];
        let want_jpeg = encode::encode_frame(target, last_wire).unwrap();
        let want_sha = ds_vdev::sha256_hex(&want_jpeg);
        wait_until(
            || {
                ds_vdev::ctl::request(&vdev_sock, "frames key:0")
                    .ok()
                    .and_then(|l| l.first().cloned())
                    .is_some_and(|l| l.contains(&want_sha))
            },
            Duration::from_secs(30),
            "the last pushed frame to be the device's final frame",
        );
        let line = ds_vdev::ctl::request(&vdev_sock, "frames key:0")
            .unwrap()
            .remove(0);
        assert!(
            line.contains(&format!("{}B", want_jpeg.len())),
            "frame line {line:?} byte count"
        );

        // telemetry: sent + dropped == 10, dropped > 0, shape, no rev (wait for the drain)
        wait_until(
            || {
                let (in_flight, waiting, sent, dropped, _q) = svc.queue_state(target);
                in_flight.is_none() && waiting.is_none() && sent + dropped == 10
            },
            Duration::from_secs(10),
            "the queue to drain (sent + dropped == 10)",
        );
        svc.sample_now();
        let (in_flight, waiting, sent, dropped, _q) = svc.queue_state(target);
        assert_eq!(in_flight, None);
        assert_eq!(waiting, None, "queue drained");
        assert_eq!(
            sent + dropped,
            10,
            "every pushed frame is sent or dropped+counted"
        );
        assert!(
            dropped > 0,
            "dropped = {dropped} (replacements while in flight)"
        );
        assert!(sent >= 1);
        let reply = store.get("/telemetry/streams/key:0").unwrap();
        assert_eq!(reply.rev, None, "no rev on telemetry reads (S1)");
        let v = &reply.value;
        assert_eq!(v["sent"], json!(sent));
        assert_eq!(v["dropped"], json!(dropped));
        assert!(v["last_sha256"].as_str().is_some_and(|s| s == want_sha));
        assert_eq!(v["last_seq"], json!(9));
        assert!(v
            .get("fps")
            .and_then(Value::as_f64)
            .is_some_and(|f| f >= 0.0));
        assert!(v
            .get("bytes")
            .and_then(Value::as_u64)
            .is_some_and(|b| b > 0));
        assert!(v.get("sampled_at").is_some());
        assert_eq!(store.rev(), 0, "frame pushes never bump rev (S1)");
        assert!(
            !store.durable_document().to_string().contains("DSF1"),
            "frame bytes never enter the durable document"
        );

        // a stopped stream stops accepting: stream-stop removes the session server-side,
        // the next frame is nacked E_SCOPE; the service side refuses with E_SCOPE too
        client.dsctl(&json!({"op": "stream-stop", "target": "key:0"}));
        // the writer's credit re-grants may still be in flight; the stop reply is credits: 0
        let mut zero = client.recv();
        for _ in 0..8 {
            if zero["op"] == json!("stream-credit") && zero["credits"] == json!(0) {
                break;
            }
            zero = client.recv();
        }
        assert_eq!(zero["op"], json!("stream-credit"));
        assert_eq!(zero["credits"], json!(0));
        let extra = Frame::new(
            frame_meta("key:0", 10, ds_ipc::frames::FrameFormat::Jpeg),
            &payloads[0],
        );
        client.send_frame(&extra);
        let nack = client.recv();
        assert_eq!(nack["op"], json!("stream-nack"), "{nack}");
        assert_eq!(nack["error"]["code"], json!("E_SCOPE"), "{nack}");
        svc.stop_stream(target);
        let err = svc
            .push_frame(target, payloads[1].clone(), 11, false)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::EScope, "{err:?}");

        shutdown.trigger();
        svc.join();
        vdev.shutdown();
    }

    /// The writer thread is spawned once per module (keys panel + key:N share the M4315
    /// handle, so one writer serializes their chunked transfers; dial has its own).
    #[test]
    fn one_writer_thread_per_module() {
        let gate = GateTransport::new();
        let (svc, _store, shutdown) = test_service(Some(gate.clone()), FRAME_QUEUE_BUDGET_BYTES);
        gate.open();
        for t in [
            FrameTarget::Panel,
            FrameTarget::Key(0),
            FrameTarget::Key(14),
        ] {
            svc.push_frame(t, frame(0, 32), 0, true).unwrap();
        }
        svc.push_frame(FrameTarget::Dial, frame(0, 32), 0, true)
            .unwrap();
        wait_until(
            || {
                svc.queue_state(FrameTarget::Panel).2
                    + svc.queue_state(FrameTarget::Key(0)).2
                    + svc.queue_state(FrameTarget::Key(14)).2
                    + svc.queue_state(FrameTarget::Dial).2
                    == 4
            },
            Duration::from_secs(5),
            "all four targets sent",
        );
        let writers = svc.inner.writers.lock().unwrap();
        assert_eq!(writers.len(), 2, "one writer per module (Keys, Dial)");
        assert!(writers.contains_key(&ModuleKind::Keys));
        assert!(writers.contains_key(&ModuleKind::Dial));
        drop(writers);
        // all three keys-module transfers went through in order, whole
        assert_eq!(gate.transfers(0x35, 0).len(), 1, "panel 0x35");
        assert_eq!(gate.transfers(0x36, 0).len(), 1);
        assert_eq!(gate.transfers(0x36, 14).len(), 1);
        assert_eq!(gate.transfers(0x45, 0).len(), 1, "dial 0x45");
        shutdown.trigger();
        svc.join();
    }

    /// A receiver used only to keep the writer signal type honest in tests.
    #[allow(dead_code)]
    fn _rx_type(_r: Receiver<()>) {}
}
