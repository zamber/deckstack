//! Backpressure / throttling integration tests for ds-vdev (DS-035).
//!
//! Drives the real `ds-device` [`VirtualTransport`] (and, for the frame-service assertions,
//! the real daemon `FrameService`) against an in-process [`VdevServer`], under the three
//! throttle knobs of `crates/ds-vdev/src/throttle.rs`:
//!
//! - `--throttle-kbps` paces acks in request order, exactly one ack per write;
//! - the credit window (`credits=…`) holds writes when its bucket is empty (backpressure:
//!   the write waits, nothing buffers without bound, the control socket stays fast) and
//!   recovers when credits refill;
//! - `--drop-after N` tears the connection cleanly (Timeout/Closed), the frame service
//!   drops+counts the torn frame and the transport reports `Disconnected` (what the daemon's
//!   device task uses to mark the module unhealthy), and a reconnect re-pushes the latest
//!   frame reassembled intact;
//! - the hub's write log is the oracle proving the device never sees a partial chunk
//!   sequence after a drop — every write is a whole report, every transfer starts at
//!   chunk 0 and completes with `last=1`;
//! - a 32 fps push against a throttled vdev: `dropped == sent - accepted` at the frame
//!   service, and a memory-bounded soak (RSS numbers recorded on `--nocapture`).
//!
//! Run: `cargo test -p ds-vdev --test backpressure [-- --nocapture]`
//!
//! The soak length defaults to 60 s and the 32 fps bench window to 2 s; both can be
//! shortened for CI with `DS_BACKPRESSURE_SOAK_SECS` / `DS_BACKPRESSURE_BENCH_SECS`.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use ds_daemon::frames::service::FrameService;
use ds_daemon::frames::FrameTarget;
use ds_daemon::lifecycle::Shutdown;
use ds_daemon::router::Bus;
use ds_device::error::Error;
use ds_device::frame::{kind, read_frame, write_frame, Frame};
use ds_device::module::module_info;
use ds_device::transport::{DeviceEvent, Transport};
use ds_device::vtransport::{virtual_module_id, VirtualTransport};
use ds_device::{open, DeviceUri};
use ds_protocol::{self, LedTarget, ModuleKind};
use ds_state::Store;
use ds_vdev::{ctl, sha256_hex, CreditWindow, HubConfig, Target, Throttle, VdevServer};

/// Soak length in seconds (default 60, CI sets `DS_BACKPRESSURE_SOAK_SECS`).
fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
        .max(1)
}

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ds-vdev-bp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn start(dir: &Path, name: &str, throttle: Throttle) -> VdevServer {
    VdevServer::start(
        dir.join(name),
        HubConfig {
            modules: ModuleKind::ALL.to_vec(),
            throttle,
        },
    )
    .unwrap()
}

fn keys_pid() -> u16 {
    module_info(ModuleKind::Keys).pid
}

fn handshake(transport: &dyn Transport, kind: ModuleKind) {
    let (report, _mode) = ds_protocol::enter_software_mode(kind).unwrap();
    transport.send(&virtual_module_id(kind), &report).unwrap();
}

fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration, what: &str) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out after {timeout:?} waiting for {what}");
}

/// Resident set size of this process in kB (`/proc/self/status` VmRSS) — the soak oracle.
fn rss_kb() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse::<u64>()
                .unwrap();
        }
    }
    panic!("no VmRSS in /proc/self/status");
}

/// The p-th percentile of a sample of durations (p in 0..=1).
fn percentile(samples: &[Duration], p: f64) -> Duration {
    assert!(!samples.is_empty());
    let mut v: Vec<Duration> = samples.to_vec();
    v.sort();
    let idx = ((p * v.len() as f64).ceil() as usize).clamp(1, v.len()) - 1;
    v[idx]
}

/// A small non-forbidden output report (an M0 LED write, padded to 64 B on the wire).
fn small_write() -> ds_protocol::Report {
    ds_protocol::led_color(ModuleKind::Base, LedTarget::All, [1, 2, 3]).unwrap()
}

/// Write one raw WRITE frame of `len` bytes (padded LED payload) on `s`.
fn send_write(s: &mut UnixStream, pid: u16, len: usize) {
    let mut payload = small_write().bytes().to_vec();
    payload.resize(len, 0);
    write_frame(s, &Frame::new(kind::WRITE, pid, payload).unwrap()).unwrap();
}

/// Read one ack and return the accepted byte count.
fn read_ack(s: &mut UnixStream) -> u32 {
    let f = read_frame(s).unwrap();
    assert_eq!(f.kind, kind::ACK, "expected an ack, got {f:?}");
    u32::from_le_bytes(f.payload[..4].try_into().unwrap())
}

// ---------------------------------------------------------------------------
// 1. kbps shaping: acks are delayed, never reordered, exactly one ack per write
// ---------------------------------------------------------------------------

#[test]
fn throttle_delays_acks_in_order_exactly_one_ack_per_write() {
    let dir = test_dir("order");
    // 256 kbit/s = 32 kB/s: a 320 B write shapes to 10 ms, a 64 B write to 2 ms
    let server = start(
        &dir,
        "vdev.sock",
        Throttle {
            kbps: Some(256),
            drop_after: None,
        },
    );
    let mut s = UnixStream::connect(server.socket_path()).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let pid = keys_pid();

    // alternate 320 B and 64 B writes; the ack's byte count identifies which write it acks
    let mut expected: Vec<u32> = Vec::new();
    let start = Instant::now();
    for i in 0..8u8 {
        let len = if i % 2 == 0 { 320 } else { 64 };
        let mut payload = vec![0x02u8, 0x2D, 0x00, 0x00, 0x01, 0x02, 0x03, 0x00];
        payload.resize(len, 0);
        write_frame(&mut s, &Frame::new(kind::WRITE, pid, payload).unwrap()).unwrap();
        expected.push(len as u32);
    }
    // the acks arrive in request order, each carrying its own write's byte count
    for want in &expected {
        let f = read_frame(&mut s).unwrap();
        assert_eq!(f.kind, kind::ACK, "reply must be an ack: {f:?}");
        assert_eq!(f.pid, pid);
        assert_eq!(
            u32::from_le_bytes(f.payload[..4].try_into().unwrap()),
            *want,
            "acks reordered under throttle"
        );
    }
    let shaped: Duration = expected
        .iter()
        .map(|&l| Duration::from_secs_f64(f64::from(l) / (256.0 * 125.0)))
        .sum();
    assert!(
        start.elapsed() >= shaped - Duration::from_millis(5),
        "acks were not delayed by the kbps cap: shaped {shaped:?}, took {:?}",
        start.elapsed()
    );
    // exactly one ack per write: nothing else arrives
    s.set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    assert!(
        read_frame(&mut s).is_err(),
        "a second ack arrived for a single write"
    );
    server.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 2. credit exhaustion: writes wait (backpressure), control plane stays fast,
//    and the window recovers when credits refill
// ---------------------------------------------------------------------------

#[test]
fn credit_exhaustion_holds_writes_then_recovers_when_credits_return() {
    let dir = test_dir("credits");
    let server = start(&dir, "vdev.sock", Throttle::none());
    let path = server.socket_path().to_path_buf();
    // arm the credit window through the control socket, exactly like ds-vdevctl does
    let lines = ctl::request(&path, "throttle off off credits=3/3/150").unwrap();
    assert!(
        lines[0].contains("credits=3/3/150") && lines[0].contains("remaining=3"),
        "{}",
        lines[0]
    );
    assert_eq!(
        server.hub().credits(),
        Some(CreditWindow::fixed(3, 3, Duration::from_millis(150)))
    );

    let mut s = UnixStream::connect(&path).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let pid = module_info(ModuleKind::Base).pid; // M0: output reports are 64 B

    // burst = 3: the first three output writes go through immediately
    for _ in 0..3 {
        let t0 = Instant::now();
        send_write(&mut s, pid, 64);
        assert_eq!(read_ack(&mut s), 64);
        assert!(
            t0.elapsed() < Duration::from_millis(60),
            "a burst write was held: {:?}",
            t0.elapsed()
        );
    }
    assert_eq!(server.hub().credit_remaining(), Some(0));

    // the 4th write finds the bucket empty: its ack is held until the next refill; while
    // it is held, the control socket (the control plane) still answers immediately
    let t0 = Instant::now();
    send_write(&mut s, pid, 64);
    thread::sleep(Duration::from_millis(30));
    let p = Instant::now();
    let pong = ctl::request(&path, "ping").unwrap();
    let ctl_lat = p.elapsed();
    assert_eq!(pong, vec!["pong".to_string()]);
    assert!(
        ctl_lat < Duration::from_millis(100),
        "control plane blocked by a held write: {ctl_lat:?}"
    );
    assert_eq!(
        read_ack(&mut s),
        64,
        "the held write is acked once the credits return"
    );
    let held = t0.elapsed();
    assert!(
        held >= Duration::from_millis(100),
        "the exhausted write was not held (backpressure missing): {held:?}"
    );
    assert!(
        held < Duration::from_secs(2),
        "held absurdly long: {held:?}"
    );

    // recovery: the refill granted 3 credits, one was charged to the held write -> two fast
    for _ in 0..2 {
        let t0 = Instant::now();
        send_write(&mut s, pid, 64);
        assert_eq!(read_ack(&mut s), 64);
        assert!(
            t0.elapsed() < Duration::from_millis(60),
            "write after the refill was held: {:?}",
            t0.elapsed()
        );
    }
    // empty again -> held again (the schedule repeats deterministically)
    let t0 = Instant::now();
    send_write(&mut s, pid, 64);
    assert_eq!(read_ack(&mut s), 64);
    assert!(
        t0.elapsed() >= Duration::from_millis(100),
        "the second exhaustion was not held: {:?}",
        t0.elapsed()
    );
    // and after an idle gap the bucket refills: fast writes again
    thread::sleep(Duration::from_millis(200));
    for _ in 0..3 {
        let t0 = Instant::now();
        send_write(&mut s, pid, 64);
        assert_eq!(read_ack(&mut s), 64);
        assert!(
            t0.elapsed() < Duration::from_millis(60),
            "write after the idle refill was held: {:?}",
            t0.elapsed()
        );
    }

    // no unbounded buffering: the hub processed every write exactly once, none refused
    let writes = ctl::request(&path, "writes 1000").unwrap();
    assert_eq!(writes.len(), 3 + 1 + 2 + 1 + 3, "write log: {writes:?}");
    assert!(writes.iter().all(|w| !w.contains("refused")));
    // the window can be disarmed again through the same command
    let lines = ctl::request(&path, "throttle off off credits=off").unwrap();
    assert!(lines[0].contains("credits=off"), "{}", lines[0]);
    assert_eq!(server.hub().credits(), None);
    server.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 3. drop-after: clean teardown, frame service counts the drop, reconnect re-pushes
// ---------------------------------------------------------------------------

#[test]
fn drop_after_tears_connection_cleanly_frame_service_counts_drop_and_reconnect_repushes_intact() {
    let dir = test_dir("drop");
    // budget of 4 accepted writes per connection: conn 1 burns 2 on handshakes, so the
    // unplug lands mid-frame (chunk 2 of 3); a reconnect gets a fresh budget of 4, which
    // fits one handshake + the whole 3-chunk frame
    let server = start(
        &dir,
        "vdev.sock",
        Throttle {
            kbps: None,
            drop_after: Some(4),
        },
    );
    let path = server.socket_path().to_path_buf();
    let uri = DeviceUri::parse(&format!("virtual:{}", path.display())).unwrap();

    let transport: Arc<dyn Transport> = Arc::from(open(&uri).unwrap());
    for kind in [ModuleKind::Keys, ModuleKind::Dial] {
        handshake(transport.as_ref(), kind);
    }
    let store = Arc::new(Store::fresh());
    let shutdown = Shutdown::manual();
    let svc = FrameService::spawn(
        store.clone(),
        Some(transport.clone()),
        Bus::new(),
        shutdown.clone(),
    );
    let target = FrameTarget::Key(0);
    let payload: Vec<u8> = (0..700u32).map(|i| (i % 251) as u8).collect(); // 3 chunks
    svc.push_frame(target, payload.clone(), 1, true).unwrap();

    // the unplug tears the connection mid-frame: the writer's send fails, the frame is
    // dropped+counted (the frame service marking the target's transfer failed), never torn
    wait_until(
        || svc.queue_state(target).3 >= 1,
        Duration::from_secs(10),
        "the torn frame to be dropped+counted",
    );
    let (_inf, _wait, sent, dropped, _q) = svc.queue_state(target);
    assert_eq!(sent, 0, "the torn frame was never completed");
    assert_eq!(dropped, 1, "the drop must be counted exactly once");
    // the transport surfaces the disconnect — the signal the daemon's device task uses to
    // mark the module unhealthy and stop sending to it (crates/ds-daemon/src/device_task.rs)
    match transport.recv(Duration::from_secs(2)) {
        Ok(DeviceEvent::Disconnected) | Err(Error::Closed) => {}
        other => panic!("expected Disconnected/Closed after the unplug, got {other:?}"),
    }
    // no partial frame was ever stored on the device
    assert!(server.hub().frame(Target::Key(0)).is_none());
    shutdown.trigger();
    svc.join();

    // reconnect: a fresh device connection gets a fresh write budget and re-pushes the
    // latest frame, which arrives whole (reassembled sha intact)
    let transport2: Arc<dyn Transport> = Arc::from(open(&uri).unwrap());
    handshake(transport2.as_ref(), ModuleKind::Keys); // fresh budget: 1 handshake + 3 chunks
    let shutdown2 = Shutdown::manual();
    let svc2 = FrameService::spawn(
        store.clone(),
        Some(transport2.clone()),
        Bus::new(),
        shutdown2.clone(),
    );
    svc2.push_frame(target, payload.clone(), 2, true).unwrap();
    wait_until(
        || svc2.queue_state(target).2 == 1,
        Duration::from_secs(10),
        "the re-pushed frame to be sent whole",
    );
    // dropped == pushed - sent across the unplug: 2 pushed, 1 dropped, 1 sent
    let (_, _, sent2, dropped2, _) = svc2.queue_state(target);
    assert_eq!(sent2, 1);
    assert_eq!(dropped + dropped2, 1, "only the torn frame was dropped");
    assert_eq!(
        sent + sent2 + dropped + dropped2,
        2,
        "dropped == pushed - sent"
    );
    let entry = server
        .hub()
        .frame(Target::Key(0))
        .expect("frame stored after the re-push");
    assert_eq!(
        entry.sha256,
        sha256_hex(&payload),
        "reassembled vdev sha intact"
    );
    assert_eq!(entry.bytes, payload.len());
    let line = ctl::request(&path, "frames key:0").unwrap().remove(0);
    assert!(line.contains(&entry.sha256), "{line}");
    shutdown2.trigger();
    svc2.join();
    server.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 4. in-flight never torn: the hub's write log is the oracle
// ---------------------------------------------------------------------------

#[test]
fn in_flight_frame_is_never_torn_after_a_drop_the_write_log_is_the_oracle() {
    let dir = test_dir("torn");
    let server = start(
        &dir,
        "vdev.sock",
        Throttle {
            kbps: None,
            drop_after: Some(4),
        },
    );
    let path = server.socket_path().to_path_buf();
    let t = VirtualTransport::connect(&path)
        .unwrap()
        .with_write_timeout(Duration::from_secs(2));
    let keys = virtual_module_id(ModuleKind::Keys);
    let (hs, _) = ds_protocol::enter_software_mode(ModuleKind::Keys).unwrap();
    assert_eq!(t.send(&keys, &hs).unwrap(), 32); // accepted write 1

    // burn one write of the budget on the dial handshake, then push a 700 B key image
    // (3 chunks): accepted writes 1..=4 are the two handshakes + chunks 0..=1, the 3rd
    // chunk write (accepted 5 > budget 4) is the unplug
    let (hs_dial, _) = ds_protocol::enter_software_mode(ModuleKind::Dial).unwrap();
    let dial = virtual_module_id(ModuleKind::Dial);
    assert_eq!(t.send(&dial, &hs_dial).unwrap(), 32); // accepted write 2
    let jpeg: Vec<u8> = (0..700u32).map(|i| (i % 247) as u8).collect();
    let reports = ds_protocol::key_image(0, &jpeg, 320);
    assert_eq!(reports.len(), 3);
    let mut torn = None;
    for (i, r) in reports.iter().enumerate() {
        match t.send(&keys, r) {
            Ok(n) => assert_eq!(n, 320),
            Err(e) => {
                assert!(
                    matches!(e, Error::Closed | Error::Timeout(_) | Error::Io(_)),
                    "the unplug must surface as Timeout/Closed, got {e:?}"
                );
                torn = Some(i);
                break;
            }
        }
    }
    let torn = torn.expect("the unplug must land mid-frame");
    assert!(
        torn >= 1 && torn < reports.len(),
        "drop landed at chunk {torn}, not mid-frame"
    );
    assert!(matches!(
        t.recv(Duration::from_secs(2)),
        Ok(DeviceEvent::Disconnected) | Err(Error::Closed)
    ));
    // no partial frame was ever stored
    assert!(server.hub().frame(Target::Key(0)).is_none());
    // write-log oracle, first connection: exactly the budget's worth of records, the chunk
    // sequence is a contiguous prefix 0..torn with no `last`
    let w1 = server.hub().writes(1000);
    assert_eq!(
        w1.len(),
        4,
        "two handshakes + 2 accepted chunks; the unplugging write is not logged"
    );
    let chunks1: Vec<(u16, bool)> = w1.iter().filter_map(|w| w.chunk).collect();
    assert_eq!(
        chunks1,
        (0..torn as u16).map(|c| (c, false)).collect::<Vec<_>>(),
        "torn prefix must be contiguous from chunk 0"
    );

    // reconnect and re-push the same frame whole
    let t2 = VirtualTransport::connect(&path)
        .unwrap()
        .with_write_timeout(Duration::from_secs(5));
    let (hs2, _) = ds_protocol::enter_software_mode(ModuleKind::Keys).unwrap();
    assert_eq!(t2.send(&keys, &hs2).unwrap(), 32);
    for r in &reports {
        assert_eq!(t2.send(&keys, r).unwrap(), 320);
    }
    let entry = server.hub().frame(Target::Key(0)).unwrap();
    assert_eq!(
        entry.sha256,
        sha256_hex(&jpeg),
        "the re-pushed frame reassembles to its full sha (in-flight never torn)"
    );

    // write-log oracle over the whole log: every chunk sequence starts at 0, is contiguous,
    // and only the post-drop transfer carries last=1 — the device never saw a partial chunk
    // sequence after the drop
    let all = server.hub().writes(10_000);
    assert_eq!(
        all.len(),
        4 + 1 + 3,
        "every write logged exactly once (no re-sends)"
    );
    assert!(all.iter().all(|w| !w.refused), "no refusals: {all:?}");
    let seqs: Vec<(u16, bool)> = all.iter().filter_map(|w| w.chunk).collect();
    assert_eq!(seqs.len(), torn + 3);
    assert_eq!(&seqs[..torn], &chunks1[..]);
    let second = &seqs[torn..];
    for (i, (c, last)) in second.iter().enumerate() {
        assert_eq!(*c, i as u16, "post-drop transfer must restart at chunk 0");
        assert_eq!(*last, i == 2, "only the final chunk carries last=1");
    }
    t.close().unwrap();
    t2.close().unwrap();
    server.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 5. bench: 32 fps against a throttled vdev — dropped == sent - accepted
// ---------------------------------------------------------------------------

#[test]
fn bench_32fps_against_throttled_vdev_dropped_equals_sent_minus_accepted() {
    let secs = env_u64("DS_BACKPRESSURE_BENCH_SECS", 2);
    let dir = test_dir("bench");
    // 64 kbit/s = 8 kB/s: a 700 B frame = 3 chunks x 320 B = 960 B takes 120 ms to accept,
    // so 32 fps offered load is backpressured and the queue must drop+count
    let server = start(
        &dir,
        "vdev.sock",
        Throttle {
            kbps: Some(64),
            drop_after: None,
        },
    );
    let path = server.socket_path().to_path_buf();
    let uri = DeviceUri::parse(&format!("virtual:{}", path.display())).unwrap();
    let transport: Arc<dyn Transport> = Arc::from(open(&uri).unwrap());
    for kind in [ModuleKind::Keys, ModuleKind::Dial] {
        handshake(transport.as_ref(), kind);
    }
    let store = Arc::new(Store::fresh());
    let shutdown = Shutdown::manual();
    let svc = FrameService::spawn(
        store.clone(),
        Some(transport.clone()),
        Bus::new(),
        shutdown.clone(),
    );
    let target = FrameTarget::Key(1);
    let fps = 32u64;
    let period = Duration::from_secs_f64(1.0 / fps as f64);

    let mut pushed = 0u64;
    let mut max_queued = 0usize;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        let payload = vec![pushed as u8; 700];
        svc.push_frame(target, payload, pushed, true).unwrap();
        pushed += 1;
        max_queued = max_queued.max(svc.queue_state(target).4);
        let frame_start = Instant::now();
        while frame_start.elapsed() < period {
            thread::sleep(Duration::from_millis(2));
        }
    }
    let last_payload = vec![(pushed - 1) as u8; 700];

    // drain: every pushed frame is either sent whole or dropped+counted
    wait_until(
        || {
            let (_, _, sent, dropped, q) = svc.queue_state(target);
            q == 0 && sent + dropped == pushed
        },
        Duration::from_secs(30),
        "the frame queue to drain",
    );
    let (_, _, sent, dropped, q) = svc.queue_state(target);
    assert_eq!(q, 0);
    assert_eq!(
        sent + dropped,
        pushed,
        "every pushed frame is sent or dropped+counted"
    );
    assert_eq!(
        dropped,
        pushed - sent,
        "dropped == sent - accepted (pushed - sent)"
    );
    assert!(
        dropped > 0,
        "32 fps against an 8 kB/s device must drop frames"
    );
    assert!(
        max_queued <= 2 * 700,
        "the queue is bounded (1 in flight + 1 waiting = 1400 B), saw {max_queued} B"
    );
    // latest-frame-wins: the device's final frame is the last pushed one, reassembled intact
    let entry = server.hub().frame(Target::Key(1)).unwrap();
    assert_eq!(entry.sha256, sha256_hex(&last_payload));
    eprintln!(
        "bench: pushed {pushed} frames at {fps} fps for {secs}s against a 64 kbit/s vdev: \
         sent={sent} dropped={dropped} (dropped == pushed - sent), max_queued={max_queued}B, \
         device sha {}",
        &entry.sha256[..16]
    );
    shutdown.trigger();
    svc.join();
    server.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 6. soak: throttled 32 fps for DS_BACKPRESSURE_SOAK_SECS (default 60) with
//    bounded memory (RSS recorded) and a fast control plane
// ---------------------------------------------------------------------------

#[test]
fn soak_throttled_32fps_memory_bounded_rss_recorded() {
    let secs = env_u64("DS_BACKPRESSURE_SOAK_SECS", 60);
    let dir = test_dir("soak");
    // 2048 kbit/s = 256 kB/s simulated device cap (the real cap is ~2.1 MB/s)
    let server = start(
        &dir,
        "vdev.sock",
        Throttle {
            kbps: Some(2048),
            drop_after: None,
        },
    );
    let path = server.socket_path().to_path_buf();
    let t = VirtualTransport::connect(&path)
        .unwrap()
        .with_write_timeout(Duration::from_secs(30));
    let keys = virtual_module_id(ModuleKind::Keys);
    handshake(&t, ModuleKind::Keys);

    // producer: 32 fps panel pushes; against the throttled device each send waits for its
    // ack, so the offered 32 fps is backpressured into the device's accept rate
    let producer = thread::spawn(move || {
        let period = Duration::from_secs_f64(1.0 / 32.0);
        let mut frames = 0u64;
        let mut chunks = 0u64;
        let mut latencies: Vec<Duration> = Vec::new();
        let mut last_jpeg: Vec<u8> = Vec::new();
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(secs) {
            let jpeg: Vec<u8> = (0..40_960u32)
                .map(|i| ((i.wrapping_mul(31).wrapping_add(frames as u32)) % 256) as u8)
                .collect();
            let reports = ds_protocol::panel_image(&jpeg, 320);
            chunks += reports.len() as u64;
            let fs = Instant::now();
            for r in &reports {
                assert_eq!(t.send(&keys, r).unwrap(), 320);
            }
            latencies.push(fs.elapsed());
            last_jpeg = jpeg;
            frames += 1;
            let frame_start = Instant::now();
            while frame_start.elapsed() < period {
                thread::sleep(Duration::from_millis(1));
            }
        }
        t.close().unwrap();
        (frames, chunks, latencies, last_jpeg)
    });

    // observer: RSS + control-plane (ctl socket) latency while the producer runs
    let mut rss_first = 0u64;
    let mut rss_max = 0u64;
    let mut pings: Vec<Duration> = Vec::new();
    let obs_start = Instant::now();
    while obs_start.elapsed() < Duration::from_secs(secs) {
        let rss = rss_kb();
        if rss_first == 0 {
            rss_first = rss;
        }
        rss_max = rss_max.max(rss);
        let p = Instant::now();
        let pong = ctl::request(&path, "ping").unwrap();
        assert_eq!(pong, vec!["pong".to_string()]);
        pings.push(p.elapsed());
        thread::sleep(Duration::from_millis(100));
    }
    let (frames, chunks, latencies, last_jpeg) = producer.join().unwrap();
    rss_max = rss_max.max(rss_kb());
    let growth_kb = rss_max.saturating_sub(rss_first);
    let p99_ping = percentile(&pings, 0.99);
    let p99_send = percentile(&latencies, 0.99);
    eprintln!(
        "soak: {secs}s offering 32 fps against a 2048 kbit/s vdev: frames={frames} \
         chunks={chunks} frame_send_p99={p99_send:?} ctl_ping_p99={p99_ping:?} \
         ctl_ping_max={:?} rss={rss_first}..{rss_max} kB growth={growth_kb} kB",
        percentile(&pings, 1.0)
    );

    assert!(
        growth_kb < 100 * 1024,
        "RSS growth {growth_kb} kB exceeds the 100 MB soak budget (rss {rss_first}..{rss_max} kB)"
    );
    assert!(
        p99_ping <= Duration::from_millis(10),
        "control-plane ping p99 {p99_ping:?} exceeds 10 ms under throttled stream load"
    );
    // every push was acked whole, exactly once: the write log has handshake + chunks entries
    let writes = server.hub().writes(1_000_000);
    assert_eq!(
        writes.len() as u64,
        1 + chunks,
        "no re-sends, no unbounded buffering"
    );
    assert!(writes.iter().all(|w| !w.refused));
    // the device's latest frame is the last pushed one, reassembled intact
    let entry = server.hub().frame(Target::Panel).unwrap();
    assert_eq!(entry.sha256, sha256_hex(&last_jpeg));
    server.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}
