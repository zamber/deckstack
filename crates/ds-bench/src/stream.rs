//! The streaming bench runner (DS-033): sustained fps frame push to one target on a
//! throttled ds-vdev through the real DSF1 stream socket, while a query loop samples
//! `get /controls/keys/0` over the real control socket, and the run is checked
//! against the budgets of `docs/30-testing.md` §5 (`docs/contracts/streaming.md`).
//!
//! Measured per run: query p50/p95/p99, frames pushed/sent/dropped (telemetry),
//! push and accepted fps, `rev` delta and `/controls` events across the window (S1),
//! peak RSS of the daemon process (`/proc/<pid>/status` `VmHWM` — the bench hosts
//! the daemon tasks in-process, so the daemon pid is this process), and that the
//! last pushed frame is the device's final reassembled frame (latest-frame-wins).

use crate::budget::{evaluate, Budgets, Measurements};
use crate::report::Report;
use crate::stack::{parse_target, read_peak_rss_bytes, recv_dsctl, send_dsctl, Stack};
use crate::stats::Series;
use ds_daemon::frames::encode;
use ds_daemon::frames::FrameTarget;
use ds_ipc::frames::codec::{write_frame, Frame, FrameFormat, FrameMeta};
use ds_ipc::frames::limits::MAX_PAYLOAD_BYTES;
use serde_json::{json, Value};
use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Default run length, seconds (phase-3 budget: 60 s).
pub const DEFAULT_SECONDS: u64 = 60;
/// Default push rate, fps (phase-3 budget: 32 fps sustained).
pub const DEFAULT_FPS: u32 = 32;
/// Default vdev throttle: ~2.1 MB/s, the simulated device write cap
/// (`upstream/PROTOCOL.md:265`; 2.1e6 B/s / 125 = 16800 kbit/s).
pub const DEFAULT_THROTTLE_KBPS: u32 = 16800;
/// Read timeout on the stream socket (backpressure must resolve well inside this).
pub const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(120);
/// How long the final queue drain may take before the bench fails.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(60);

/// Bench knobs (CLI flags map onto these).
#[derive(Debug, Clone)]
pub struct BenchOptions {
    /// Streaming window length in seconds.
    pub seconds: u64,
    /// Producer push rate in frames/s.
    pub fps: u32,
    /// vdev `--throttle-kbps`; `None` = unlimited.
    pub throttle_kbps: Option<u32>,
    /// The stream target (`keys` | `key:N` | `dial`).
    pub target: FrameTarget,
    /// The control path the query loop samples.
    pub query_path: String,
    /// Budgets to evaluate against.
    pub budgets: Budgets,
}

impl Default for BenchOptions {
    fn default() -> Self {
        Self {
            seconds: DEFAULT_SECONDS,
            fps: DEFAULT_FPS,
            throttle_kbps: Some(DEFAULT_THROTTLE_KBPS),
            target: FrameTarget::Key(0),
            query_path: "/controls/keys/0".to_string(),
            budgets: Budgets::default(),
        }
    }
}

/// What the producer thread measured.
struct ProdOutcome {
    pushed: u64,
    wall: Duration,
    /// sha256 of the device JPEG of the last pushed frame.
    last_device_sha: String,
}

/// Run the streaming bench on a freshly booted stack; the stack is always cleaned up
/// (deterministic: shutdown → writers drain → vdev stop → tmp dir removed).
pub fn run(opts: &BenchOptions) -> Result<Report, String> {
    let stack = Stack::boot(&crate::stack::StackConfig {
        throttle_kbps: opts.throttle_kbps,
        drop_after: None,
    })?;
    let report = run_on(&stack, opts);
    stack.stop();
    report
}

fn run_on(stack: &Stack, opts: &BenchOptions) -> Result<Report, String> {
    let mut client = stack.control_client()?;
    let target_name = opts.target.wire_name();

    // Make the query path exist (one set, before the measured window; it bumps rev).
    client
        .set(
            json!([{"op": "set", "path": format!("{}/label", opts.query_path), "value": "ds-bench"}]),
            None,
        )
        .map_err(|e| format!("seed set: {e}"))?;
    // Subscribe to /controls: events during the window would violate S1.
    client
        .subscribe("/controls", None)
        .map_err(|e| format!("subscribe: {e}"))?;
    let _ = client.take_pending_events();
    // Warmup queries (first-call validator compilation must not pollute the window).
    for _ in 0..10 {
        client
            .get(&opts.query_path, None)
            .map_err(|e| format!("warmup get: {e}"))?;
    }
    let rev_before = envelope_rev(
        &client
            .get("/meta", None)
            .map_err(|e| format!("get /meta: {e}"))?,
    )?;

    // Producer thread: paced DSF1 push at `fps` for `seconds`.
    let done = Arc::new(AtomicBool::new(false));
    let done2 = Arc::clone(&done);
    let producer = spawn_producer(
        stack.stream_sock.clone(),
        target_name.clone(),
        opts.target,
        opts.fps,
        opts.seconds,
        done2,
    );

    // Query loop on this thread for the whole window.
    let mut query_ms = Series::new();
    let mut controls_events = 0u64;
    while !done.load(Ordering::SeqCst) {
        let t0 = Instant::now();
        let resp = client
            .get(&opts.query_path, None)
            .map_err(|e| format!("query get /controls failed: {e}"))?;
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        if resp["ok"] != Value::Bool(true) {
            return Err(format!("query get not ok: {resp}"));
        }
        query_ms.push(ms);
        controls_events += client.take_pending_events().len() as u64;
    }
    let prod = join_producer(producer)?;

    // Drain the target queue (stream-stop was sent by the producer): every pushed
    // frame must end up sent or dropped+counted.
    let target = opts.target;
    let pushed = prod.pushed;
    wait_until(
        || {
            let (in_flight, waiting, sent, dropped, _) = stack.frames.queue_state(target);
            in_flight.is_none() && waiting.is_none() && sent + dropped == pushed
        },
        DRAIN_TIMEOUT,
        "the frame queue to drain (sent + dropped == pushed)",
    )?;
    stack.frames.sample_now();

    // Telemetry over the control plane via the `stream-stats` op (no rev, S1; the
    // ipc-message path pattern excludes ':' so `key:N` targets cannot be `get`).
    let telem = client
        .request(json!({"op": "stream-stats", "target": target_name}))
        .map_err(|e| format!("stream-stats {target_name}: {e}"))?;
    let value = &telem["result"]["value"];
    let sent = value["sent"].as_u64().ok_or("telemetry sent missing")?;
    let dropped = value["dropped"]
        .as_u64()
        .ok_or("telemetry dropped missing")?;
    let bytes = value["bytes"].as_u64().unwrap_or(0);
    let (_, _, sent_q, dropped_q, _) = stack.frames.queue_state(target);
    if (sent, dropped) != (sent_q, dropped_q) {
        return Err(format!(
            "stream-stats {target_name} (sent={sent} dropped={dropped}) != queue_state (sent={sent_q} dropped={dropped_q})"
        ));
    }

    let rev_after = envelope_rev(
        &client
            .get("/meta", None)
            .map_err(|e| format!("get /meta: {e}"))?,
    )?;
    let rev_delta = rev_after.saturating_sub(rev_before);

    // The last pushed frame must be the device's final reassembled frame.
    let last_on_device =
        device_last_frame_has(&stack.vdev_sock, &target_name, &prod.last_device_sha);

    // Peak RSS of the daemon process. The bench hosts the daemon tasks in-process,
    // so the daemon pid is this process; VmHWM is its high-water mark.
    let peak_rss_bytes = read_peak_rss_bytes(std::process::id())?;

    let wall_s = prod.wall.as_secs_f64();
    let measurements = Measurements {
        wall_s,
        pushed,
        sent,
        dropped,
        bytes,
        push_fps: pushed as f64 / wall_s.max(f64::EPSILON),
        accepted_fps: sent as f64 / wall_s.max(f64::EPSILON),
        query_ms,
        rev_delta,
        controls_events,
        peak_rss_bytes,
        last_frame_on_device: last_on_device,
    };
    let violations = evaluate(&measurements, &opts.budgets, opts.fps);
    Ok(Report::new(opts, &measurements, violations))
}

/// The envelope `rev` of a durable `get` reply.
fn envelope_rev(resp: &Value) -> Result<u64, String> {
    resp["rev"]
        .as_u64()
        .ok_or_else(|| format!("response carries no rev: {resp}"))
}

fn spawn_producer(
    stream_sock: std::path::PathBuf,
    target_name: String,
    target: FrameTarget,
    fps: u32,
    seconds: u64,
    done: Arc<AtomicBool>,
) -> JoinHandle<Result<ProdOutcome, String>> {
    std::thread::Builder::new()
        .name("ds-bench-producer".into())
        .spawn(move || {
            let r = produce(&stream_sock, &target_name, target, fps, seconds);
            done.store(true, Ordering::SeqCst);
            r
        })
        .expect("spawn producer thread")
}

fn join_producer(handle: JoinHandle<Result<ProdOutcome, String>>) -> Result<ProdOutcome, String> {
    handle
        .join()
        .map_err(|_| "producer thread panicked".to_string())?
}

/// The producer: `DSctl stream-open`, then `fps * seconds` DSF1 frames paced at
/// `fps`, each waiting for its `stream-ack` (credits honoured), then `stream-stop`.
fn produce(
    sock: &Path,
    target_name: &str,
    target: FrameTarget,
    fps: u32,
    seconds: u64,
) -> Result<ProdOutcome, String> {
    let mut w = UnixStream::connect(sock).map_err(|e| format!("connect stream.sock: {e}"))?;
    w.set_read_timeout(Some(STREAM_READ_TIMEOUT))
        .map_err(|e| format!("set read timeout: {e}"))?;
    let mut reader = BufReader::new(
        w.try_clone()
            .map_err(|e| format!("clone stream socket: {e}"))?,
    );

    send_dsctl(
        &mut w,
        &json!({"op": "stream-open", "target": target_name, "credits": 1024}),
    )?;
    let mut credits: u64 = 0;
    loop {
        let msg = recv_dsctl(&mut reader)?;
        match msg.get("op").and_then(Value::as_str) {
            Some("stream-credit")
                if msg.get("target").and_then(Value::as_str) == Some(target_name) =>
            {
                credits += msg.get("credits").and_then(Value::as_u64).unwrap_or(0);
                break;
            }
            Some("stream-nack") | Some("error") => {
                return Err(format!("stream-open refused: {msg}"));
            }
            _ => {}
        }
    }

    let total = fps as u64 * seconds;
    let interval = Duration::from_secs_f64(1.0 / f64::from(fps.max(1)));
    let start = Instant::now();
    let (w_, h_) = (target.width(), target.height());
    let mut last_payload: Vec<u8> = Vec::new();
    for seq in 0..total {
        let next = start + interval * (seq as u32);
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
        while credits == 0 {
            let msg = recv_dsctl(&mut reader)?;
            match msg.get("op").and_then(Value::as_str) {
                Some("stream-credit") => {
                    credits += msg.get("credits").and_then(Value::as_u64).unwrap_or(0);
                }
                Some("stream-ack") => {}
                Some("stream-nack") => return Err(format!("credit wait nacked: {msg}")),
                _ => {}
            }
        }
        let payload = encode::encode_solid(target, color(seq));
        let meta = FrameMeta {
            target: target_name.to_string(),
            seq,
            ts_ms: crate::stack::now_ms(),
            format: FrameFormat::Jpeg,
            w: Some(w_),
            h: Some(h_),
            frames: Some(1),
            fps: None,
            loop_: None,
            crc32: 0, // Frame::new computes it
            max_payload_bytes: MAX_PAYLOAD_BYTES as u32,
        };
        let frame = Frame::new(meta, &payload);
        write_frame(&mut w, &frame).map_err(|e| format!("write frame {seq}: {e}"))?;
        w.flush().map_err(|e| format!("flush frame {seq}: {e}"))?;
        loop {
            let msg = recv_dsctl(&mut reader)?;
            match msg.get("op").and_then(Value::as_str) {
                Some("stream-ack") if msg.get("seq").and_then(Value::as_u64) == Some(seq) => {
                    credits = credits.saturating_sub(1);
                    break;
                }
                Some("stream-credit") => {
                    credits += msg.get("credits").and_then(Value::as_u64).unwrap_or(0);
                }
                Some("stream-nack") => return Err(format!("frame {seq} nacked: {msg}")),
                _ => {}
            }
        }
        last_payload = payload;
    }
    let wall = start.elapsed();

    let last_device_sha = encode::encode_frame(target, &last_payload)
        .map(|jpeg| ds_vdev::sha256_hex(&jpeg))
        .map_err(|e| format!("encode last frame: {e}"))?;

    // stream-stop: the session is removed server-side; the writer drains the queue.
    send_dsctl(&mut w, &json!({"op": "stream-stop", "target": target_name}))?;
    let _ = w.set_read_timeout(Some(Duration::from_millis(500)));
    while recv_dsctl(&mut reader).is_ok() {}
    Ok(ProdOutcome {
        pushed: total,
        wall,
        last_device_sha,
    })
}

/// A distinct solid colour per frame seq (deterministic payload variety).
fn color(seq: u64) -> [u8; 3] {
    [
        (seq.wrapping_mul(37) % 256) as u8,
        (seq.wrapping_mul(91) % 256) as u8,
        (seq.wrapping_mul(53) % 256) as u8,
    ]
}

/// Does the vdev's final reassembled frame for `target` carry `sha`?
fn device_last_frame_has(vdev_sock: &Path, target_name: &str, sha: &str) -> bool {
    ds_vdev::ctl::request(vdev_sock, &format!("frames {target_name}"))
        .map(|lines| {
            lines
                .iter()
                .rev()
                .find(|l| l.starts_with(&format!("{target_name} ")))
                .is_some_and(|l| l.contains(sha))
        })
        .unwrap_or(false)
}

/// Poll `cond` until true or `timeout`; Err with `what` on timeout.
pub fn wait_until(
    mut cond: impl FnMut() -> bool,
    timeout: Duration,
    what: &str,
) -> Result<(), String> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(format!("timed out after {timeout:?} waiting for {what}"))
}

/// Parse `--target` for the CLI (delegates to the S6 vocabulary).
pub fn cli_target(s: &str) -> Result<FrameTarget, String> {
    parse_target(s)
}
