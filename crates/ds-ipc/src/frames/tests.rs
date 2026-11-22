//! Socket-level tests of the frame plane: a real `StreamServer` on a temp
//! `<dir>/stream.sock`, driven by a raw peer that speaks `DSctl\n` control lines and
//! DSF1 binary frames. Covers the ticket's acceptance list: push N frames + ack
//! accounting, credit exhaustion → backpressure, oversize → nack + E_LIMIT, rate
//! limit, and the S1 invariant (no path to state/rev).

use super::codec::{self, Frame, FrameFormat, FrameMeta};
use super::limits::{StreamConfig, MAX_PAYLOAD_BYTES};
use super::server::{FrameSink, StreamBus, StreamEvent, StreamServer};
use crate::error::{ErrorCode, IpcError};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A sink that records every accepted frame (or fails them all with a fixed error).
struct RecordingSink {
    frames: Mutex<Vec<(String, Frame)>>,
    fail: Option<IpcError>,
}

impl RecordingSink {
    fn new() -> Self {
        Self {
            frames: Mutex::new(Vec::new()),
            fail: None,
        }
    }

    fn failing(code: ErrorCode, msg: &str) -> Self {
        Self {
            frames: Mutex::new(Vec::new()),
            fail: Some(IpcError::new(code, msg)),
        }
    }

    fn len(&self) -> usize {
        self.frames.lock().unwrap().len()
    }
}

impl FrameSink for RecordingSink {
    fn on_frame(&self, target: &str, frame: &Frame) -> Result<(), IpcError> {
        if let Some(e) = &self.fail {
            return Err(e.clone());
        }
        self.frames
            .lock()
            .unwrap()
            .push((target.to_string(), frame.clone()));
        Ok(())
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    stream_sock: std::path::PathBuf,
    bus: StreamBus,
    sink: Arc<RecordingSink>,
}

fn fixture() -> Fixture {
    fixture_with(|_| {})
}

fn fixture_with(tweak: impl FnOnce(&mut StreamConfig)) -> Fixture {
    fixture_sink_with(Arc::new(RecordingSink::new()), tweak)
}

fn fixture_sink_with(sink: Arc<RecordingSink>, tweak: impl FnOnce(&mut StreamConfig)) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let stream_sock = dir.path().join("run/stream.sock");
    let mut config = StreamConfig {
        idle_timeout: None,
        min_frame_interval: Duration::ZERO, // tests push frames back-to-back
        ..StreamConfig::default()
    };
    tweak(&mut config);
    let server = StreamServer::bind(&stream_sock, config).expect("bind stream.sock");
    let bus = server.start(sink.clone()).expect("start");
    Fixture {
        _dir: dir,
        stream_sock,
        bus,
        sink,
    }
}

/// A raw producer peer on the stream socket.
struct StreamPeer {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl StreamPeer {
    fn connect(path: &Path) -> Self {
        let writer = UnixStream::connect(path).expect("connect stream.sock");
        let reader = BufReader::new(writer.try_clone().expect("clone"));
        Self { reader, writer }
    }

    fn send_dsctl(&mut self, v: &Value) {
        self.writer.write_all(codec::DSCTL_MAGIC).unwrap();
        self.writer
            .write_all(serde_json::to_string(v).unwrap().as_bytes())
            .unwrap();
        self.writer.write_all(b"\n").unwrap();
        self.writer.flush().unwrap();
    }

    /// Read one `DSctl\n` line from the server; `None` on EOF.
    fn recv_dsctl(&mut self) -> Option<Value> {
        let mut magic = [0u8; 6];
        match self.reader.read_exact(&mut magic) {
            Ok(()) => assert_eq!(&magic, codec::DSCTL_MAGIC, "server line must be DSctl"),
            Err(_) => return None,
        }
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => None,
            Ok(_) => Some(serde_json::from_str(line.trim()).expect("json DSctl line")),
            Err(_) => None,
        }
    }

    fn push_frame(&mut self, frame: &Frame) {
        self.writer.write_all(&frame.encode().unwrap()).unwrap();
        self.writer.flush().unwrap();
    }

    fn push_raw(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }
}

fn meta(target: &str, seq: u64) -> FrameMeta {
    FrameMeta {
        target: target.into(),
        seq,
        ts_ms: 1_732_000_000_000 + seq * 20,
        format: FrameFormat::Png,
        w: Some(480),
        h: Some(272),
        frames: Some(1),
        fps: None,
        loop_: None,
        crc32: 0,
        max_payload_bytes: MAX_PAYLOAD_BYTES as u32,
    }
}

fn frame(target: &str, seq: u64, payload: &[u8]) -> Frame {
    Frame::new(meta(target, seq), payload)
}

/// Open a stream session and consume the `stream-credit` grant reply.
fn stream_open(peer: &mut StreamPeer, target: &str, credits: u32) -> Value {
    peer.send_dsctl(&json!({"op": "stream-open", "target": target, "credits": credits}));
    let grant = peer.recv_dsctl().expect("stream-credit grant");
    assert_eq!(grant["op"], "stream-credit");
    assert_eq!(grant["target"], target);
    assert_eq!(grant["credits"], credits);
    grant
}

fn wait_event(bus: &StreamBus, pred: impl Fn(&StreamEvent) -> bool) -> StreamEvent {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        while let Some(ev) = bus.try_recv_event() {
            if pred(&ev) {
                return ev;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for stream event"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn push_n_frames_and_ack_accounting() {
    let fx = fixture();
    let mut peer = StreamPeer::connect(&fx.stream_sock);
    stream_open(&mut peer, "keys", 8);

    let n = 8u64;
    for seq in 0..n {
        peer.push_frame(&frame("keys", seq, format!("payload-{seq}").as_bytes()));
    }
    for seq in 0..n {
        let ack = peer.recv_dsctl().expect("ack");
        assert_eq!(ack["op"], "stream-ack", "got {ack}");
        assert_eq!(ack["target"], "keys");
        assert_eq!(ack["seq"], seq);
        crate::message::validate_server_message(&ack)
            .expect("ack validates against ipc-message.json");
    }

    // ack accounting: the sink saw all N frames, stats count sent/bytes/last_seq
    assert_eq!(fx.sink.len(), n as usize);
    let stats = fx.bus.stats("keys").expect("stats");
    assert_eq!(stats["sent"], n);
    assert_eq!(stats["dropped"], 0);
    let expected_bytes: u64 = (0..n).map(|s| format!("payload-{s}").len() as u64).sum();
    assert_eq!(stats["bytes"], expected_bytes);
    assert_eq!(stats["last_seq"], n - 1);
    for seq in 0..n {
        let ev = wait_event(
            &fx.bus,
            |e| matches!(e, StreamEvent::Ack { seq: s, .. } if *s == seq),
        );
        assert!(matches!(ev, StreamEvent::Ack { .. }));
    }
}

#[test]
fn credit_exhaustion_gives_backpressure_then_regrant_unblocks() {
    let fx = fixture();
    let mut peer = StreamPeer::connect(&fx.stream_sock);
    stream_open(&mut peer, "keys", 2);

    peer.push_frame(&frame("keys", 0, b"a"));
    peer.push_frame(&frame("keys", 1, b"b"));
    assert_eq!(peer.recv_dsctl().unwrap()["op"], "stream-ack");
    assert_eq!(peer.recv_dsctl().unwrap()["op"], "stream-ack");

    // third frame: credit window empty -> nack E_LIMIT + Backpressure event
    peer.push_frame(&frame("keys", 2, b"c"));
    let nack = peer.recv_dsctl().expect("nack");
    assert_eq!(nack["op"], "stream-nack");
    assert_eq!(nack["target"], "keys");
    assert_eq!(nack["seq"], 2);
    assert_eq!(nack["error"]["code"], "E_LIMIT");
    let ev = wait_event(
        &fx.bus,
        |e| matches!(e, StreamEvent::Backpressure { target, .. } if target == "keys"),
    );
    assert!(matches!(ev, StreamEvent::Backpressure { .. }));

    // daemon re-grants credits as its queue drains -> stream-credit line, then frames flow again
    fx.bus.grant_credits("keys", 2);
    let grant = peer.recv_dsctl().expect("stream-credit");
    assert_eq!(grant["op"], "stream-credit");
    assert_eq!(grant["credits"], 2);
    peer.push_frame(&frame("keys", 3, b"d"));
    let ack = peer.recv_dsctl().expect("ack after regrant");
    assert_eq!(ack["op"], "stream-ack");
    assert_eq!(ack["seq"], 3);

    let stats = fx.bus.stats("keys").unwrap();
    assert_eq!(stats["sent"], 3);
    assert_eq!(stats["dropped"], 0); // nacks are flow control, not sink drops
}

#[test]
fn oversize_frame_gets_nack_and_limit_error_then_close() {
    let fx = fixture();
    let mut peer = StreamPeer::connect(&fx.stream_sock);
    stream_open(&mut peer, "dial", 8);

    // hand-build a header declaring payload_len = 16 MB + 1 (no payload bytes follow)
    let mut m = meta("dial", 9);
    m.crc32 = 0;
    let meta_json = serde_json::to_vec(&m).unwrap();
    let mut raw = Vec::new();
    raw.extend_from_slice(codec::DSF1_MAGIC);
    raw.extend_from_slice(&(meta_json.len() as u32).to_le_bytes());
    raw.extend_from_slice(&meta_json);
    raw.extend_from_slice(&(MAX_PAYLOAD_BYTES as u64 + 1).to_le_bytes());
    peer.push_raw(&raw);

    let nack = peer.recv_dsctl().expect("nack for oversize frame");
    assert_eq!(nack["op"], "stream-nack");
    assert_eq!(nack["seq"], 9);
    assert_eq!(nack["error"]["code"], "E_LIMIT", "nack: {nack}");
    // oversize declared length -> framing untrusted -> connection closed after the nack
    assert!(
        peer.recv_dsctl().is_none(),
        "server must close after an oversize frame"
    );
}

#[test]
fn crc_mismatch_nacks_but_connection_stays_open() {
    let fx = fixture();
    let mut peer = StreamPeer::connect(&fx.stream_sock);
    stream_open(&mut peer, "keys", 8);

    let f = frame("keys", 0, b"good");
    peer.push_frame(&f);
    assert_eq!(peer.recv_dsctl().unwrap()["op"], "stream-ack");

    // corrupt the payload after encoding (crc no longer matches)
    let mut bad = f.clone();
    bad.meta.seq = 1;
    bad.payload = b"tampered".to_vec();
    peer.push_frame(&bad);
    let nack = peer.recv_dsctl().expect("nack");
    assert_eq!(nack["op"], "stream-nack");
    assert_eq!(nack["seq"], 1);
    assert_eq!(nack["error"]["code"], "E_SCHEMA");

    // connection still in sync: the next frame is acked
    peer.push_frame(&frame("keys", 2, b"fine"));
    assert_eq!(peer.recv_dsctl().unwrap()["op"], "stream-ack");
    let stats = fx.bus.stats("keys").unwrap();
    assert_eq!(stats["sent"], 2);
}

#[test]
fn frame_without_stream_open_is_nacked() {
    let fx = fixture();
    let mut peer = StreamPeer::connect(&fx.stream_sock);
    peer.push_frame(&frame("keys", 0, b"x"));
    let nack = peer.recv_dsctl().expect("nack");
    assert_eq!(nack["op"], "stream-nack");
    assert_eq!(nack["error"]["code"], "E_SCOPE");
    // after stream-open the same push is acked
    stream_open(&mut peer, "keys", 4);
    peer.push_frame(&frame("keys", 1, b"x"));
    assert_eq!(peer.recv_dsctl().unwrap()["op"], "stream-ack");
}

#[test]
fn per_target_rate_limit_nacks_fast_frames() {
    let fx = fixture_with(|c| c.min_frame_interval = Duration::from_millis(50));
    let mut peer = StreamPeer::connect(&fx.stream_sock);
    stream_open(&mut peer, "keys", 8);
    peer.push_frame(&frame("keys", 0, b"a"));
    assert_eq!(peer.recv_dsctl().unwrap()["op"], "stream-ack");
    // immediately again: below the 0.02 s-equivalent min interval configured here
    peer.push_frame(&frame("keys", 1, b"b"));
    let nack = peer.recv_dsctl().expect("nack");
    assert_eq!(nack["op"], "stream-nack");
    assert_eq!(nack["error"]["code"], "E_LIMIT");
    std::thread::sleep(Duration::from_millis(60));
    peer.push_frame(&frame("keys", 2, b"c"));
    assert_eq!(peer.recv_dsctl().unwrap()["op"], "stream-ack");
}

#[test]
fn stream_stop_revokes_the_session() {
    let fx = fixture();
    let mut peer = StreamPeer::connect(&fx.stream_sock);
    stream_open(&mut peer, "keys", 4);
    peer.send_dsctl(&json!({"op": "stream-stop", "target": "keys"}));
    let reply = peer.recv_dsctl().expect("stop reply");
    assert_eq!(reply["op"], "stream-credit");
    assert_eq!(reply["credits"], 0);
    peer.push_frame(&frame("keys", 5, b"x"));
    let nack = peer.recv_dsctl().expect("nack after stop");
    assert_eq!(nack["op"], "stream-nack");
    assert_eq!(nack["error"]["code"], "E_SCOPE");
}

#[test]
fn invalid_dsctl_line_gets_schema_error() {
    let fx = fixture();
    let mut peer = StreamPeer::connect(&fx.stream_sock);
    peer.send_dsctl(&json!({"op": "migrate"}));
    let err = peer.recv_dsctl().expect("error line");
    assert_eq!(err["ok"], false);
    assert_eq!(err["error"]["code"], "E_SCHEMA");
}

#[test]
fn sink_error_nacks_with_its_code_and_counts_dropped() {
    let sink = Arc::new(RecordingSink::failing(ErrorCode::EDevice, "module gone"));
    let fx = fixture_sink_with(sink, |_| {});
    let mut peer = StreamPeer::connect(&fx.stream_sock);
    stream_open(&mut peer, "keys", 4);
    peer.push_frame(&frame("keys", 0, b"x"));
    let nack = peer.recv_dsctl().expect("nack");
    assert_eq!(nack["op"], "stream-nack");
    assert_eq!(nack["error"]["code"], "E_DEVICE");
    let stats = fx.bus.stats("keys").unwrap();
    assert_eq!(stats["sent"], 0);
    assert_eq!(stats["dropped"], 1);
}

/// S1: the frame plane has no path to durable state or `rev`.
///
/// Three assertions: (a) compile-time separation — `ds-ipc`'s `Cargo.toml` does not
/// depend on `ds-state`, so no frames API can take a state handle; (b) the frames
/// module's only outputs (stats/events) carry no `rev` field; (c) end-to-end — pushing
/// frames on the stream socket leaves the control plane's revision untouched.
#[test]
fn s1_frames_never_touch_state_or_rev() {
    // (a) no ds-state dependency at the crate level
    let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .expect("read ds-ipc Cargo.toml");
    assert!(
        !manifest.contains("ds-state"),
        "ds-ipc must not depend on ds-state (S1 compile-time separation)"
    );

    // (b)+(c) end-to-end: control plane + stream plane side by side in one dir
    let dir = tempfile::tempdir().unwrap();
    let token_dir = dir.path().join("tokens");
    let install_token = crate::testing::write_token_dir(&token_dir).unwrap();
    let control_sock = dir.path().join("run/daemon.sock");
    let control = crate::server::Server::bind(
        &control_sock,
        &token_dir,
        crate::limits::ServerConfig::default(),
    )
    .expect("bind daemon.sock");
    let _control_handle = control
        .start(Arc::new(crate::testing::MockHandler::new()))
        .expect("start control");

    let rev_of_new_control_conn = |sock: &Path, token: &str| -> u64 {
        let mut w = UnixStream::connect(sock).unwrap();
        let mut r = BufReader::new(w.try_clone().unwrap());
        let send = |w: &mut UnixStream, v: Value| {
            let mut line = serde_json::to_vec(&v).unwrap();
            line.push(b'\n');
            w.write_all(&line).unwrap();
            w.flush().unwrap();
        };
        send(
            &mut w,
            json!({"id": 0, "op": "hello", "protocol": 1, "client": "t"}),
        );
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        let welcome: Value = serde_json::from_str(line.trim()).unwrap();
        send(&mut w, json!({"id": 1, "op": "auth", "token": token}));
        line.clear();
        r.read_line(&mut line).unwrap();
        welcome["result"]["rev"].as_u64().unwrap()
    };

    let rev_before = rev_of_new_control_conn(&control_sock, &install_token);

    // push frames on the stream socket
    let config = StreamConfig {
        idle_timeout: None,
        min_frame_interval: Duration::ZERO,
        ..StreamConfig::default()
    };
    let stream_sock = dir.path().join("run/stream.sock");
    let server = StreamServer::bind(&stream_sock, config).unwrap();
    let bus = server.start(Arc::new(RecordingSink::new())).unwrap();
    let mut peer = StreamPeer::connect(&stream_sock);
    stream_open(&mut peer, "keys", 16);
    for seq in 0..16u64 {
        peer.push_frame(&frame("keys", seq, b"frame-bytes"));
        assert_eq!(peer.recv_dsctl().unwrap()["op"], "stream-ack");
    }

    // (b) frames' stats carry no rev
    let stats = bus.stats("keys").expect("stats");
    assert_eq!(stats["sent"], 16);
    assert!(
        stats.get("rev").is_none(),
        "frame stats must not carry rev (S1): {stats}"
    );
    // StreamEvent has no rev-carrying variant by construction (S1)
    let ev = StreamEvent::Ack {
        conn: 1,
        target: "keys".into(),
        seq: 0,
    };
    assert!(!format!("{ev:?}").contains("rev"));

    // (c) the control plane's rev did not move
    let rev_after = rev_of_new_control_conn(&control_sock, &install_token);
    assert_eq!(
        rev_before, rev_after,
        "pushing frames must not bump rev (S1)"
    );
}
