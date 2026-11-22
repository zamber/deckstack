//! The frame-plane stream server: a Unix socket at `<dir>/stream.sock`
//! (`docs/contracts/ipc.md` §1.1) carrying **two framings on one socket**, told apart by
//! the leading magic:
//!
//! - `DSF1` — binary frames (see [`super::codec`]), producer → daemon;
//! - `DSctl\n` — NDJSON stream control lines (`stream-open`, `stream-stop`), validated
//!   against the embedded `schemas/ipc-message.json` request branch; replies are
//!   `DSctl\n` async messages (`stream-credit` / `stream-ack` / `stream-nack`), validated
//!   against the response/async branch (`docs/contracts/streaming.md` §7,
//!   `docs/contracts/ipc.md` §2).
//!
//! Flow control: `stream-open {target, credits}` opens a per-connection, per-target
//! session and grants a credit budget (`stream-credit`). Every frame spends one credit;
//! a frame arriving with an empty window is answered `stream-nack` + `E_LIMIT` **and**
//! raises a [`StreamEvent::Backpressure`] — the producer must wait for the daemon to
//! re-grant credits with `stream-credit` (the daemon grants as its queue drains;
//! [`StreamBus::grant_credits`]). Accepted frames are answered `stream-ack {seq}` after
//! the [`FrameSink`] accepted them. Per-target rate (≤ 30 fps), per-frame size (≤ 16 MB),
//! per-target queue depth (2) and global queue memory (≤ 256 MiB) are enforced with
//! `stream-nack` + the matching error code (`docs/contracts/streaming.md` §5,
//! `docs/contracts/ipc.md` §5).
//!
//! S1: this module has **no path to durable state or `rev`** — `ds-ipc` does not depend
//! on `ds-state` (asserted by test), the server keeps only in-memory per-target counters
//! (the `/telemetry/streams/<target>` stand-in; real telemetry lands with DS-015), and
//! nothing here can bump a revision.

use super::codec::{self, Frame};
use super::limits::StreamConfig;
use crate::error::{ErrorCode, IpcError};
use crate::framing;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// The daemon-side consumer of accepted frames. In this ticket (wire protocol + flow
/// control only) the sink is called synchronously on the connection thread; the queue /
/// writer threads of `docs/contracts/streaming.md` §3 land with DS-031. A sink that wants
/// to re-grant credits as it drains uses the [`StreamBus`] handle it was started with.
pub trait FrameSink: Send + Sync + 'static {
    /// Consume one accepted frame. `Ok(())` → the server sends `stream-ack`; `Err(e)` →
    /// `stream-nack` carrying `e` and the frame is counted as dropped.
    fn on_frame(&self, target: &str, frame: &Frame) -> Result<(), IpcError>;
}

/// What happened on the stream plane, reported on the event channel returned by
/// [`StreamServer::start`]. Purely observational (telemetry); no state, no rev (S1).
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A `stream-open` session was created.
    Opened {
        conn: u64,
        target: String,
        credits: u32,
    },
    /// A frame was accepted and acked.
    Ack { conn: u64, target: String, seq: u64 },
    /// A frame was rejected; `error` is the nack's envelope.
    Nack {
        conn: u64,
        target: String,
        seq: u64,
        error: IpcError,
    },
    /// The credit window for `target` was empty when a frame arrived — the producer
    /// must stop until a `stream-credit` re-grants budget (backpressure signal).
    Backpressure { conn: u64, target: String },
    /// The connection closed.
    Closed { conn: u64 },
}

/// Per-target in-memory counters — the non-durable `/telemetry/streams/<target>`
/// stand-in (`docs/contracts/streaming.md` §4/§6). **No `rev` field exists here (S1).**
#[derive(Debug, Clone, Default)]
pub struct TargetStats {
    pub sent: u64,
    pub dropped: u64,
    pub bytes: u64,
    pub last_seq: Option<u64>,
}

impl TargetStats {
    /// The `/telemetry/streams/<target>` JSON shape (subset; sampled ≤ 1 Hz by the
    /// daemon later, non-durable, no rev).
    pub fn to_value(&self) -> Value {
        json!({
            "sent": self.sent,
            "dropped": self.dropped,
            "bytes": self.bytes,
            "last_seq": self.last_seq,
        })
    }
}

/// Shared server state: per-target stats, the live connection writers (for
/// [`StreamBus::grant_credits`]), and the event sender.
/// Per-connection state: the writer plus the per-target sessions (credit windows).
struct ConnState {
    writer: Arc<Mutex<std::io::BufWriter<UnixStream>>>,
    sessions: HashMap<String, Session>,
}

struct StreamShared {
    stats: Mutex<HashMap<String, TargetStats>>,
    /// conn id → state (sessions + writer).
    conns: Mutex<HashMap<u64, ConnState>>,
    events: Sender<StreamEvent>,
    queued_bytes: AtomicUsize,
    config: StreamConfig,
}

impl StreamShared {
    fn emit(&self, ev: StreamEvent) {
        let _ = self.events.send(ev);
    }

    /// Write one `DSctl\n` NDJSON line to a connection (schema-validated async branch).
    fn send_dsctl(
        writer: &Arc<Mutex<std::io::BufWriter<UnixStream>>>,
        value: &Value,
        max_line: usize,
    ) -> Result<(), IpcError> {
        crate::message::validate_server_message(value)?;
        let line = serde_json::to_string(value).expect("Value serializes");
        if line.len() > max_line {
            return Err(IpcError::new(ErrorCode::ELimit, "DSctl line too long"));
        }
        let mut w = writer.lock().unwrap();
        w.write_all(codec::DSCTL_MAGIC)
            .and_then(|_| w.write_all(line.as_bytes()))
            .and_then(|_| w.write_all(b"\n"))
            .and_then(|_| w.flush())
            .map_err(|e| IpcError::new(ErrorCode::EInternal, format!("i/o error: {e}")))
    }
}

/// Handle for the embedder (daemon / tests) to observe and steer a running
/// [`StreamServer`]. Cloneable.
#[derive(Clone)]
pub struct StreamBus {
    shared: Arc<StreamShared>,
    events: Arc<Mutex<std::sync::mpsc::Receiver<StreamEvent>>>,
}

impl StreamBus {
    /// Per-target in-memory stats (telemetry stand-in; no rev, S1).
    pub fn stats(&self, target: &str) -> Option<Value> {
        self.shared
            .stats
            .lock()
            .unwrap()
            .get(target)
            .map(TargetStats::to_value)
    }

    /// All per-target stats.
    pub fn all_stats(&self) -> Value {
        let s = self.shared.stats.lock().unwrap();
        Value::Object(s.iter().map(|(k, v)| (k.clone(), v.to_value())).collect())
    }

    /// Re-grant `credits` to every connection with an open session for `target`, as a
    /// `DSctl\n{"op":"stream-credit","target":…,"credits":N}` line — the daemon sends
    /// this as its per-target queue drains (`docs/contracts/ipc.md` §5: the producer
    /// must respect `stream-credit` budgets).
    pub fn grant_credits(&self, target: &str, credits: u32) {
        let value = json!({"op": "stream-credit", "target": target, "credits": credits});
        let mut conns = self.shared.conns.lock().unwrap();
        let mut writers = Vec::new();
        for state in conns.values_mut() {
            if let Some(session) = state.sessions.get_mut(target) {
                session.credits = session.credits.saturating_add(credits);
                writers.push(Arc::clone(&state.writer));
            }
        }
        drop(conns);
        for writer in writers {
            let _ =
                StreamShared::send_dsctl(&writer, &value, self.shared.config.max_dsctl_line_bytes);
        }
    }

    /// Pop the next event, if one is queued.
    pub fn try_recv_event(&self) -> Option<StreamEvent> {
        self.events.lock().unwrap().try_recv().ok()
    }

    /// Block until the next event arrives.
    pub fn recv_event(&self) -> Option<StreamEvent> {
        self.events.lock().unwrap().recv().ok()
    }
}

/// Per-connection, per-target session: the credit window and the rate-limit timestamp.
struct Session {
    credits: u32,
    last_frame_at: Instant,
}

/// A bound frame-plane server at `<dir>/stream.sock`. Call [`StreamServer::start`]
/// (background accept loop, returns the [`StreamBus`]) or [`StreamServer::serve`]
/// (blocking).
pub struct StreamServer {
    listener: UnixListener,
    socket_path: PathBuf,
    config: StreamConfig,
    live: Arc<AtomicUsize>,
    next_conn_id: Arc<AtomicU64>,
}

impl StreamServer {
    /// Bind `socket_path` (parent dir created 0700, socket 0600). A stale socket file is
    /// removed; a live one is an error — same policy as the control server.
    pub fn bind(socket_path: &Path, config: StreamConfig) -> std::io::Result<Self> {
        if let Some(parent) = socket_path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            }
        }
        if socket_path.exists() {
            match UnixStream::connect(socket_path) {
                Ok(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::AddrInUse,
                        format!("a daemon is already listening on {}", socket_path.display()),
                    ))
                }
                Err(_) => fs::remove_file(socket_path)?,
            }
        }
        let listener = UnixListener::bind(socket_path)?;
        fs::set_permissions(socket_path, fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            listener,
            socket_path: socket_path.to_path_buf(),
            config,
            live: Arc::new(AtomicUsize::new(0)),
            next_conn_id: Arc::new(AtomicU64::new(1)),
        })
    }

    /// The bound socket path (`<dir>/stream.sock`).
    pub fn local_addr(&self) -> &Path {
        &self.socket_path
    }

    /// Spawn the accept loop on a background thread; returns the bus for stats,
    /// credit grants and events.
    pub fn start(self, sink: Arc<dyn FrameSink>) -> std::io::Result<StreamBus> {
        let (tx, rx) = channel::<StreamEvent>();
        let shared = Arc::new(StreamShared {
            stats: Mutex::new(HashMap::new()),
            conns: Mutex::new(HashMap::new()),
            events: tx,
            queued_bytes: AtomicUsize::new(0),
            config: self.config.clone(),
        });
        let listener = self.listener.try_clone()?;
        let config = self.config.clone();
        let live = Arc::clone(&self.live);
        let next_conn_id = Arc::clone(&self.next_conn_id);
        let shared2 = Arc::clone(&shared);
        thread::Builder::new()
            .name("ds-ipc-stream-accept".into())
            .spawn(move || {
                accept_loop(listener, config, shared2, live, next_conn_id, sink);
            })?;
        Ok(StreamBus {
            shared,
            events: Arc::new(Mutex::new(rx)),
        })
    }

    /// Run the accept loop on this thread (blocking). Returns the bus when the loop ends.
    pub fn serve(self, sink: Arc<dyn FrameSink>) -> StreamBus {
        let (tx, rx) = channel::<StreamEvent>();
        let shared = Arc::new(StreamShared {
            stats: Mutex::new(HashMap::new()),
            conns: Mutex::new(HashMap::new()),
            events: tx,
            queued_bytes: AtomicUsize::new(0),
            config: self.config.clone(),
        });
        let bus = StreamBus {
            shared: Arc::clone(&shared),
            events: Arc::new(Mutex::new(rx)),
        };
        accept_loop(
            self.listener,
            self.config,
            shared,
            self.live,
            self.next_conn_id,
            sink,
        );
        bus
    }
}

#[allow(clippy::too_many_arguments)]
fn accept_loop(
    listener: UnixListener,
    config: StreamConfig,
    shared: Arc<StreamShared>,
    live: Arc<AtomicUsize>,
    next_conn_id: Arc<AtomicU64>,
    sink: Arc<dyn FrameSink>,
) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if live.fetch_add(1, Ordering::SeqCst) >= config.max_connections {
                    live.fetch_sub(1, Ordering::SeqCst);
                    drop(stream);
                    continue;
                }
                let id = next_conn_id.fetch_add(1, Ordering::SeqCst);
                let shared = Arc::clone(&shared);
                let config = config.clone();
                let sink = Arc::clone(&sink);
                let live = Arc::clone(&live);
                let _ = thread::Builder::new()
                    .name(format!("ds-ipc-stream-{id}"))
                    .spawn(move || {
                        handle_stream_conn(stream, id, config, shared, sink);
                        live.fetch_sub(1, Ordering::SeqCst);
                    });
            }
            Err(e) => eprintln!("ds-ipc stream: accept error: {e}"),
        }
    }
}

fn handle_stream_conn(
    stream: UnixStream,
    id: u64,
    config: StreamConfig,
    shared: Arc<StreamShared>,
    sink: Arc<dyn FrameSink>,
) {
    if let Some(t) = config.idle_timeout {
        let _ = stream.set_read_timeout(Some(t));
    }
    let writer = Arc::new(Mutex::new(std::io::BufWriter::new(
        stream.try_clone().expect("clone"),
    )));
    let mut reader = BufReader::new(stream);
    shared.conns.lock().unwrap().insert(
        id,
        ConnState {
            writer: Arc::clone(&writer),
            sessions: HashMap::new(),
        },
    );
    let result = conn_loop(&mut reader, id, &config, &shared, &sink, &writer);
    shared.conns.lock().unwrap().remove(&id);
    shared.emit(StreamEvent::Closed { conn: id });
    if let Err(e) = result {
        let _ = StreamShared::send_dsctl(
            &writer,
            &json!({"ok": false, "error": e.to_value()}),
            config.max_dsctl_line_bytes,
        );
    }
}

/// The per-connection loop: read the 4-byte magic, dispatch to the DSF1 codec or the
/// `DSctl\n` line reader. Returns `Err` for a fatal framing error (the caller sends the
/// error line and closes).
#[allow(clippy::too_many_arguments)]
fn conn_loop(
    reader: &mut BufReader<UnixStream>,
    conn: u64,
    config: &StreamConfig,
    shared: &Arc<StreamShared>,
    sink: &Arc<dyn FrameSink>,
    writer: &Arc<Mutex<std::io::BufWriter<UnixStream>>>,
) -> Result<(), IpcError> {
    loop {
        let mut magic = [0u8; 4];
        match reader.read_exact(&mut magic) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Ok(()); // idle timeout
            }
            Err(e) => {
                return Err(IpcError::new(
                    ErrorCode::EInternal,
                    format!("i/o error: {e}"),
                ))
            }
        }
        if &magic == codec::DSF1_MAGIC {
            // Ok(true) = fatal: the nack was already sent, close the connection.
            if handle_frame(reader, conn, config, shared, sink, writer)? {
                return Ok(());
            }
        } else if magic == *b"DSct" {
            // remaining 2 bytes of the "DSctl\n" control-line magic
            let mut rest = [0u8; 2];
            match reader.read_exact(&mut rest) {
                Ok(()) if &rest == b"l\n" => {}
                Ok(()) => {
                    return Err(IpcError::new(
                        ErrorCode::ESchema,
                        "stream socket message is neither DSF1 nor DSctl",
                    ))
                }
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => {
                    return Err(IpcError::new(
                        ErrorCode::EInternal,
                        format!("i/o error: {e}"),
                    ))
                }
            }
            handle_dsctl_line(reader, conn, config, shared, writer)?;
        } else {
            return Err(IpcError::new(
                ErrorCode::ESchema,
                "stream socket message is neither DSF1 nor DSctl",
            ));
        }
    }
}

/// Handle one `DSctl\n` NDJSON control line (the magic was already consumed).
fn handle_dsctl_line(
    reader: &mut BufReader<UnixStream>,
    conn: u64,
    config: &StreamConfig,
    shared: &Arc<StreamShared>,
    writer: &Arc<Mutex<std::io::BufWriter<UnixStream>>>,
) -> Result<(), IpcError> {
    let line = match framing::read_line(reader, config.max_dsctl_line_bytes) {
        Ok(l) => l,
        Err(framing::FrameError::Eof) => return Ok(()),
        Err(e) => {
            let err = e.to_ipc_error();
            let _ = StreamShared::send_dsctl(
                writer,
                &json!({"ok": false, "error": err.to_value()}),
                config.max_dsctl_line_bytes,
            );
            return Err(err);
        }
    };
    if line.trim().is_empty() {
        return Ok(());
    }
    let value: Value = match serde_json::from_str(line.trim()) {
        Ok(v) => v,
        Err(e) => {
            return send_dsctl_error(
                writer,
                config,
                IpcError::new(
                    ErrorCode::ESchema,
                    format!("DSctl line is not one JSON object: {e}"),
                ),
            );
        }
    };
    // DSctl lines are control-plane requests (stream-open / stream-stop); they must
    // validate against schemas/ipc-message.json and carry one of the stream ops.
    if let Err(e) = crate::message::validate_request(&value) {
        return send_dsctl_error(writer, config, e);
    }
    let op = value.get("op").and_then(Value::as_str).unwrap_or("");
    let target = value.get("target").and_then(Value::as_str).unwrap_or("");
    match op {
        "stream-open" => {
            let mut credits = value
                .get("credits")
                .and_then(Value::as_u64)
                .unwrap_or(u64::from(config.max_credits_per_open))
                as u32;
            credits = credits.clamp(1, config.max_credits_per_open);
            shared
                .conns
                .lock()
                .unwrap()
                .entry(conn)
                .and_modify(|state| {
                    state.sessions.insert(
                        target.to_string(),
                        Session {
                            credits,
                            // backdated so the first frame after stream-open is not rate-limited
                            last_frame_at: Instant::now() - config.min_frame_interval,
                        },
                    );
                });
            shared.emit(StreamEvent::Opened {
                conn,
                target: target.to_string(),
                credits,
            });
            // The daemon replies with the credit budget (ipc-message.json stream-open).
            StreamShared::send_dsctl(
                writer,
                &json!({"op": "stream-credit", "target": target, "credits": credits}),
                config.max_dsctl_line_bytes,
            )
        }
        "stream-stop" => {
            shared
                .conns
                .lock()
                .unwrap()
                .entry(conn)
                .and_modify(|state| {
                    state.sessions.remove(target);
                });
            // Budget revoked; the session is gone. credits: 0 is the schema-valid way to
            // say "no more frames" (stream-stop is not an async op in ipc-message.json).
            StreamShared::send_dsctl(
                writer,
                &json!({"op": "stream-credit", "target": target, "credits": 0}),
                config.max_dsctl_line_bytes,
            )
        }
        _ => send_dsctl_error(
            writer,
            config,
            IpcError::new(
                ErrorCode::ESchema,
                format!("op '{op}' is not a stream control op (stream-open, stream-stop)"),
            ),
        ),
    }
}

fn send_dsctl_error(
    writer: &Arc<Mutex<std::io::BufWriter<UnixStream>>>,
    config: &StreamConfig,
    err: IpcError,
) -> Result<(), IpcError> {
    StreamShared::send_dsctl(
        writer,
        &json!({"ok": false, "error": err.to_value()}),
        config.max_dsctl_line_bytes,
    )
}

/// Handle one DSF1 frame (magic already consumed). Returns `Ok(true)` when the
/// connection must be closed after the nack (header error / oversize declared payload —
/// the framing can no longer be trusted, same policy as the control plane's over-cap
/// line); `Ok(false)` to keep serving the connection.
#[allow(clippy::too_many_arguments)]
fn handle_frame(
    reader: &mut BufReader<UnixStream>,
    conn: u64,
    config: &StreamConfig,
    shared: &Arc<StreamShared>,
    sink: &Arc<dyn FrameSink>,
    writer: &Arc<Mutex<std::io::BufWriter<UnixStream>>>,
) -> Result<bool, IpcError> {
    let max_payload = config.max_payload_bytes as u64;
    let header = match codec::read_frame_header(reader, max_payload) {
        Ok(h) => h,
        Err(e) => {
            // Header errors are always fatal: the payload length is only known after a
            // valid meta, so the stream cannot be resynced. Nack (seq 0 — the meta was
            // not trustworthy) and close, same policy as an over-cap control line.
            let seq = match &e {
                codec::DecodeError::PayloadTooLong(seq) => *seq,
                _ => 0,
            };
            let err = e.to_ipc_error();
            let _ = nack(writer, config, conn, shared, "", seq, err.clone());
            return Ok(true);
        }
    };
    let target = header.meta.target.clone();
    let seq = header.meta.seq;

    // 1–3. session, credit window (backpressure), per-target rate limit — all under the
    // connection registry lock; the lock is dropped before the sink runs (the sink may
    // call StreamBus::grant_credits, which takes the same lock).
    enum Gate {
        Pass,
        Nack(IpcError, bool), // error, backpressure
    }
    let gate = {
        let mut conns = shared.conns.lock().unwrap();
        let Some(state) = conns.get_mut(&conn) else {
            return Ok(true); // connection vanished; close quietly
        };
        match state.sessions.get_mut(&target) {
            None => Gate::Nack(
                IpcError::new(
                    ErrorCode::EScope,
                    format!("no stream-open for target '{target}': send DSctl stream-open first"),
                ),
                false,
            ),
            Some(session) => {
                if session.credits == 0 {
                    Gate::Nack(
                        IpcError::new(
                            ErrorCode::ELimit,
                            format!("credit window for target '{target}' exhausted: wait for stream-credit"),
                        ),
                        true,
                    )
                } else if config.min_frame_interval > Duration::ZERO
                    && session.last_frame_at.elapsed() < config.min_frame_interval
                {
                    Gate::Nack(
                        IpcError::new(
                            ErrorCode::ELimit,
                            format!(
                                "target '{target}' rate limit: min frame interval 0.02 s (30 fps)"
                            ),
                        ),
                        false,
                    )
                } else {
                    session.credits -= 1;
                    session.last_frame_at = Instant::now();
                    Gate::Pass
                }
            }
        }
    };
    match gate {
        Gate::Pass => {}
        Gate::Nack(err, backpressure) => {
            if backpressure {
                shared.emit(StreamEvent::Backpressure {
                    conn,
                    target: target.clone(),
                });
            }
            let _ = nack(writer, config, conn, shared, &target, seq, err);
            return read_and_continue(reader, header.payload_len, max_payload);
        }
    }
    // 4. global frame-queue memory cap
    let new_queued = shared
        .queued_bytes
        .fetch_add(header.payload_len as usize, Ordering::SeqCst)
        + header.payload_len as usize;
    if new_queued > config.max_queue_memory_bytes {
        shared
            .queued_bytes
            .fetch_sub(header.payload_len as usize, Ordering::SeqCst);
        let err = IpcError::new(
            ErrorCode::ELimit,
            "global frame-queue memory cap (256 MiB) exceeded",
        );
        let _ = nack(writer, config, conn, shared, &target, seq, err);
        return read_and_continue(reader, header.payload_len, max_payload);
    }
    // 5. read + CRC-check the payload
    let payload = match codec::read_payload(reader, header.payload_len, header.meta.crc32) {
        Ok(p) => p,
        Err(e) => {
            shared
                .queued_bytes
                .fetch_sub(header.payload_len as usize, Ordering::SeqCst);
            let err = e.to_ipc_error();
            let _ = nack(writer, config, conn, shared, &target, seq, err);
            return Ok(false);
        }
    };
    let frame = Frame {
        meta: header.meta.clone(),
        payload,
    };
    // 6. deliver to the sink, then ack/nack
    match sink.on_frame(&target, &frame) {
        Ok(()) => {
            shared
                .queued_bytes
                .fetch_sub(header.payload_len as usize, Ordering::SeqCst);
            let mut stats = shared.stats.lock().unwrap();
            let s = stats.entry(target.clone()).or_default();
            s.sent += 1;
            s.bytes += frame.payload.len() as u64;
            s.last_seq = Some(seq);
            drop(stats);
            shared.emit(StreamEvent::Ack {
                conn,
                target: target.clone(),
                seq,
            });
            match StreamShared::send_dsctl(
                writer,
                &json!({"op": "stream-ack", "target": target, "seq": seq}),
                config.max_dsctl_line_bytes,
            ) {
                Ok(()) => Ok(false),
                Err(_) => Ok(true),
            }
        }
        Err(e) => {
            shared
                .queued_bytes
                .fetch_sub(header.payload_len as usize, Ordering::SeqCst);
            let mut stats = shared.stats.lock().unwrap();
            let s = stats.entry(target.clone()).or_default();
            s.dropped += 1;
            s.last_seq = Some(seq);
            drop(stats);
            let _ = nack(writer, config, conn, shared, &target, seq, e);
            Ok(false)
        }
    }
}

/// After a nack, consume the frame's payload (if in-cap) so the stream stays in sync.
/// An over-cap declared length is fatal: the framing can no longer be trusted.
fn read_and_continue(
    reader: &mut BufReader<UnixStream>,
    payload_len: u64,
    max_payload: u64,
) -> Result<bool, IpcError> {
    if payload_len > max_payload {
        return Ok(true);
    }
    // Discard exactly payload_len bytes to keep the stream framing in sync.
    let mut discarded = Vec::new();
    match reader
        .by_ref()
        .take(payload_len)
        .read_to_end(&mut discarded)
    {
        Ok(_) => Ok(false),
        Err(_) => Ok(true), // could not resync the stream: close
    }
}

/// Send `DSctl\n{"op":"stream-nack","target":…,"seq":…,"error":{…}}` and emit the event.
fn nack(
    writer: &Arc<Mutex<std::io::BufWriter<UnixStream>>>,
    config: &StreamConfig,
    conn: u64,
    shared: &Arc<StreamShared>,
    target: &str,
    seq: u64,
    error: IpcError,
) -> Result<(), IpcError> {
    shared.emit(StreamEvent::Nack {
        conn,
        target: target.to_string(),
        seq,
        error: error.clone(),
    });
    StreamShared::send_dsctl(
        writer,
        &json!({"op": "stream-nack", "target": target, "seq": seq, "error": error.to_value()}),
        config.max_dsctl_line_bytes,
    )
}
