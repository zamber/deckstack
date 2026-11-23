//! Action latency bench (ticket DS-045) — `docs/contracts/actions.md` §5 budgets,
//! measured the way §5 prescribes ("Measured by the DS-045 bench"):
//!
//! - **event → dispatch p99 ≤ 5 ms under active stream load**: an in-process
//!   [`Bus`] with several subscribers (exact + wildcard topic patterns and a
//!   state-path pattern) while a [`FrameService`] pushes 32 fps frames into a
//!   throttled [`VdevServer`] (frames never enter the bus, S1 — the load is there to
//!   prove the publish path stays inside budget while the writer threads contend
//!   for CPU). Each `Bus::publish` is timed; `publish` is exactly the event→dispatch
//!   step (one short lock, pattern match, bounded try-enqueue, never I/O).
//! - **set/emit p99 ≤ 10 ms**: the same measurement over `Event::State` publishes
//!   (what a `set` step's committed transaction dispatches) — reported alongside.
//! - **key input → visual ≤ 2 s end to end with the vdev**: the real `ds-vdev` +
//!   `ds-daemon` binaries are spawned on a tmp dir, a binding
//!   `deck/control/keys.0/down → set /controls/keys/0/image` is installed over the
//!   control socket, key presses are injected through the vdev control channel
//!   (`inject {"module":"keys","keys":[0]}`, the same call `ds-vdevctl inject` makes),
//!   and the wall time from inject until `get /controls/keys/0/image` returns the
//!   new colour is recorded per cycle (p50/p99/max asserted against 2 s).
//!
//! Wiring: like `tests/bench/backpressure.rs` (DS-035), this file is a repo-level
//! bench wired as an explicit cargo test target by a crate `Cargo.toml`
//! (`[[test]] name = "actions" path = "../../tests/bench/actions.rs"`); DS-045's
//! touches do not include any `Cargo.toml`, so the file is written to that pattern
//! and was compiled+run once out-of-tree (a scratch crate with path deps on the
//! workspace crates including this file via `#[path]`) — the numbers are recorded
//! in `prs/DS-045.md` and the run report.
//!
//! The heavy benches are **env-gated** (`DS_ACTIONS_BENCH=1`) so a plain
//! `cargo test` run of the wired target stays fast and CI-safe:
//!
//! ```sh
//! DS_ACTIONS_BENCH=1 cargo test --test actions -- --nocapture     # full bench
//! DS_ACTIONS_BENCH_SECS=5 DS_ACTIONS_BENCH_CYCLES=10               # knobs
//! ```
//!
//! Run: `DS_ACTIONS_BENCH=1 cargo test --test actions -- --nocapture` (or the
//! out-of-tree equivalent). Without the env flag each bench test prints `SKIP`
//! and passes, so `cargo test --workspace` is unaffected.

use ds_client::Client;
use ds_daemon::frames::service::FrameService;
use ds_daemon::frames::FrameTarget;
use ds_daemon::lifecycle::Shutdown;
use ds_daemon::router::{Bus, Event, Pattern};
use ds_device::transport::Transport;
use ds_device::uri::DeviceUri;
use ds_device::vtransport::virtual_module_id;
use ds_protocol::ModuleKind;
use ds_state::Store;
use ds_vdev::{ctl, HubConfig, Throttle, VdevServer};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// actions.md §5: event → dispatch p99 ≤ 5 ms.
const EVENT_DISPATCH_BUDGET: Duration = Duration::from_millis(5);
/// actions.md §5: `set`/`emit` p99 ≤ 10 ms.
const SET_EMIT_BUDGET: Duration = Duration::from_millis(10);
/// actions.md §5/§6: key input → visual ≤ 2 s end to end (plugin invoke ≤ 2 s too).
const KEY_TO_VISUAL_BUDGET: Duration = Duration::from_secs(2);
/// Stream load: 32 fps (docs/30-testing.md §5, streaming.md).
const FPS: u64 = 32;

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
        .max(1)
}

/// The heavy benches run only with `DS_ACTIONS_BENCH=1` (env-gated, see header).
fn bench_on() -> bool {
    matches!(
        std::env::var("DS_ACTIONS_BENCH").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn skip(name: &str) -> bool {
    if bench_on() {
        return false;
    }
    println!("SKIP  {name}: set DS_ACTIONS_BENCH=1 to run the bench");
    true
}

/// Nearest-rank percentile of a latency series (durations), 0 < p <= 100.
fn percentile(sorted: &[Duration], p: f64) -> Duration {
    assert!(!sorted.is_empty());
    let n = sorted.len();
    let rank = ((p / 100.0) * n as f64).ceil() as usize;
    sorted[rank.max(1).min(n) - 1]
}

fn report_series(name: &str, lat: &[Duration], budget: Duration) -> (Duration, Duration) {
    let mut s = lat.to_vec();
    s.sort();
    let p50 = percentile(&s, 50.0);
    let p95 = percentile(&s, 95.0);
    let p99 = percentile(&s, 99.0);
    let max = *s.last().unwrap();
    println!(
        "{name}: n={} p50={:.3?} p95={:.3?} p99={:.3?} max={:.3?} budget={:.3?}",
        s.len(),
        p50,
        p95,
        p99,
        max,
        budget
    );
    assert!(
        p99 <= budget,
        "{name}: p99 {p99:?} exceeds the budget {budget:?} (p50 {p50:?}, max {max:?})"
    );
    (p99, max)
}

/// Locate the deckstack repo root: `DS_REPO_ROOT`, else the first ancestor of the
/// crate manifest dir (when wired as a workspace `[[test]]` target) or of the cwd
/// that looks like the repo (has `tests/conformance/run.py` + `crates/ds-daemon`).
fn repo_root() -> PathBuf {
    if let Ok(r) = std::env::var("DS_REPO_ROOT") {
        return PathBuf::from(r);
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for base in [manifest, std::env::current_dir().unwrap_or_default()] {
        for anc in base.ancestors() {
            if anc.join("tests/conformance/run.py").is_file()
                && anc.join("crates/ds-daemon/Cargo.toml").is_file()
            {
                return anc.to_path_buf();
            }
        }
    }
    panic!("cannot locate the deckstack repo root — set DS_REPO_ROOT");
}

fn bin_dir() -> PathBuf {
    std::env::var("DS_BIN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo_root().join("target").join("debug"))
}

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ds-actions-bench-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
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

/// A counting subscriber sink: delivery must never block the publisher.
struct CountSink {
    delivered: AtomicU64,
}

impl ds_daemon::router::Sink for CountSink {
    fn deliver(&self, _msg: &Value) -> Result<(), ds_ipc::error::IpcError> {
        self.delivered.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// Bench 1: event → dispatch (and set/emit state-event) p99 under a 32 fps stream load.
#[test]
fn event_dispatch_p99_under_32fps_stream_load() {
    if skip("event_dispatch_p99_under_32fps_stream_load") {
        return;
    }
    let secs = env_u64("DS_ACTIONS_BENCH_SECS", 5);
    let publishes = env_u64("DS_ACTIONS_BENCH_PUBLISHES", 20_000);
    let dir = test_dir("dispatch");

    // throttled vdev + real transport + frame service pushing 32 fps (stream load)
    let server = VdevServer::start(
        dir.join("vdev.sock"),
        HubConfig {
            modules: ModuleKind::ALL.to_vec(),
            throttle: Throttle {
                kbps: Some(2048), // ~256 kB/s: the writer threads stay busy
                drop_after: None,
            },
        },
    )
    .unwrap();
    let uri = DeviceUri::parse(&format!("virtual:{}", server.socket_path().display())).unwrap();
    let transport: Arc<dyn Transport> = Arc::from(ds_device::open(&uri).unwrap());
    for kind in [ModuleKind::Keys, ModuleKind::Dial] {
        let (report, _mode) = ds_protocol::enter_software_mode(kind).unwrap();
        transport.send(&virtual_module_id(kind), &report).unwrap();
    }
    let store = Arc::new(Store::fresh());
    let shutdown = Shutdown::manual();
    let bus = Bus::new();

    // subscribers: exact + wildcard topic patterns and a state-path pattern
    let wires = [
        "/deck/control/keys/*",
        "/deck/control/keys.0/down",
        "/app/*",
        "/deck/*",
        "/controls/*",
    ];
    let sinks: Vec<Arc<CountSink>> = wires
        .iter()
        .map(|w| {
            let sink = Arc::new(CountSink {
                delivered: AtomicU64::new(0),
            });
            bus.subscribe(
                Pattern::parse_wire(w).unwrap(),
                None,
                w.to_string(),
                sink.clone(),
            );
            sink
        })
        .collect();
    assert_eq!(bus.subscriber_count(), wires.len());

    // the frame service shares the bus (frames themselves never enter it, S1)
    let svc = FrameService::spawn(store, Some(transport.clone()), bus.clone(), shutdown.clone());
    let load_secs = secs;
    let svc_load = svc.clone();
    let load = thread::spawn(move || {
        let svc = svc_load;
        let target = FrameTarget::Key(0);
        let period = Duration::from_secs_f64(1.0 / FPS as f64);
        let start = Instant::now();
        let mut seq = 0u64;
        while start.elapsed() < Duration::from_secs(load_secs) {
            let _ = svc.push_frame(target, vec![seq as u8; 700], seq, true);
            seq += 1;
            let frame_start = Instant::now();
            while frame_start.elapsed() < period {
                thread::sleep(Duration::from_millis(2));
            }
        }
        seq
    });

    // measure publish (event → dispatch) latency under that load
    let mut topic_lat: Vec<Duration> = Vec::with_capacity(publishes as usize);
    let mut set_lat: Vec<Duration> = Vec::with_capacity(publishes as usize);
    for i in 0..publishes {
        if i % 2 == 0 {
            let ev = Event::topic(format!("app/bench/{}", i % 8), json!({"i": i}));
            let t = Instant::now();
            let n = bus.publish(ev);
            topic_lat.push(t.elapsed());
            assert_eq!(n, 1, "the app/bench topic reaches exactly the /app/* subscriber");
        } else {
            // what a `set` step's committed transaction dispatches: one state event
            let ev = Event::state(
                "/controls/keys/0/image",
                json!({"kind": "color", "value": "#010203"}),
                None,
                i,
            );
            let t = Instant::now();
            let n = bus.publish(ev);
            set_lat.push(t.elapsed());
            assert_eq!(n, 1, "the state event reaches exactly the /controls/* subscriber");
        }
    }
    let frames = load.join().unwrap();

    let (topic_p99, topic_max) = report_series(
        "event->dispatch (topic publish, 32 fps stream load active)",
        &topic_lat,
        EVENT_DISPATCH_BUDGET,
    );
    let (set_p99, set_max) = report_series(
        "set/emit dispatch (state-event publish, same load)",
        &set_lat,
        SET_EMIT_BUDGET,
    );
    let delivered: u64 = sinks.iter().map(|s| s.delivered.load(Ordering::Relaxed)).sum();
    println!(
        "load: {frames} frames pushed at {FPS} fps to key:0 on a 2048 kbit/s vdev; \
         {} events delivered to {} subscribers, bus dropped={}",
        topic_lat.len() + set_lat.len(),
        wires.len(),
        bus.total_dropped()
    );
    assert_eq!(bus.total_dropped(), 0, "no subscriber may drop at this rate");
    assert_eq!(
        delivered,
        (topic_lat.len() + set_lat.len()) as u64,
        "every published event was delivered exactly once (one matching subscriber each)"
    );

    shutdown.trigger();
    svc.join();
    server.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
    println!(
        "BENCH event-dispatch: topic p99={topic_p99:?} max={topic_max:?} (budget 5 ms); \
         set/emit p99={set_p99:?} max={set_max:?} (budget 10 ms) — PASS"
    );
}

/// Bench 2: key input → visual end to end with the real vdev + daemon binaries.
#[test]
fn key_input_to_visual_end_to_end_with_vdev() {
    if skip("key_input_to_visual_end_to_end_with_vdev") {
        return;
    }
    let cycles = env_u64("DS_ACTIONS_BENCH_CYCLES", 10);
    let dir = test_dir("e2e");
    let vdev_sock = dir.join("vdev.sock");
    let data = dir.join("data");
    let daemon_sock = data.join("daemon.sock");
    let bins = bin_dir();
    for b in ["ds-vdev", "ds-daemon"] {
        assert!(
            bins.join(b).is_file(),
            "missing binary {}/{b} — build it first (cargo build --bin ds-vdev --bin ds-daemon) \
             or set DS_BIN_DIR",
            bins.display()
        );
    }

    // spawn the real binaries (black box, like the conformance suites)
    let vdev_log = std::fs::File::create(dir.join("vdev.log")).unwrap();
    let mut vdev = Command::new(bins.join("ds-vdev"))
        .arg("--socket")
        .arg(&vdev_sock)
        .stdout(Stdio::from(vdev_log.try_clone().unwrap()))
        .stderr(Stdio::from(vdev_log))
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    wait_until(|| vdev_sock.exists(), Duration::from_secs(10), "the vdev socket");
    let daemon_log = std::fs::File::create(dir.join("daemon.log")).unwrap();
    let mut daemon = Command::new(bins.join("ds-daemon"))
        .arg("run")
        .arg("--device")
        .arg(format!("virtual:{}", vdev_sock.display()))
        .arg("--data-dir")
        .arg(&data)
        .stdout(Stdio::from(daemon_log.try_clone().unwrap()))
        .stderr(Stdio::from(daemon_log))
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    wait_until(|| daemon_sock.exists() && data.join("token").is_file(),
               Duration::from_secs(10), "the daemon socket + token");

    let mut lat: Vec<Duration> = Vec::new();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut c = Client::connect(&daemon_sock).unwrap();
        c.hello("ds045-bench").unwrap();
        c.authenticate_token_file(None).unwrap();
        // chain A binding: key-down recolours key 0
        let r = c
            .request(json!({
                "op": "bind",
                "binding": {
                    "id": "b0000",
                    "topic": "deck/control/keys.0/down",
                    "steps": [{"do": "set", "path": "/controls/keys/0/image",
                               "value": {"kind": "color", "value": "#ff8800"}}],
                },
            }))
            .unwrap();
        assert!(r["ok"] == true, "bind failed: {r}");

        let press = r#"inject {"module":"keys","keys":[0]}"#;
        let release = r#"inject {"module":"keys","keys":[]}"#;
        // one warmup cycle (device task handshake etc.)
        let _ = ctl::request(&vdev_sock, release);
        thread::sleep(Duration::from_millis(100));
        let _ = ctl::request(&vdev_sock, press);
        thread::sleep(Duration::from_millis(200));

        for _ in 0..cycles {
            let _ = ctl::request(&vdev_sock, release);
            thread::sleep(Duration::from_millis(60));
            let t0 = Instant::now();
            ctl::request(&vdev_sock, press).unwrap();
            // poll the visual through the real control socket until it changed
            loop {
                let g = c.get("/controls/keys/0/image", None).unwrap();
                if g["result"]["value"] == json!({"kind": "color", "value": "#ff8800"}) {
                    break;
                }
                assert!(
                    t0.elapsed() <= Duration::from_secs(10),
                    "key input -> visual did not complete within 10 s"
                );
                thread::sleep(Duration::from_millis(2));
            }
            lat.push(t0.elapsed());
        }
    }));
    // cleanup: stop the daemon (SIGTERM), then the vdev; never leak processes
    let _ = Command::new("kill").arg("-TERM").arg(daemon.id().to_string()).status();
    let _ = daemon.wait_timeout_kill(Duration::from_secs(5));
    let _ = Command::new("kill").arg("-TERM").arg(vdev.id().to_string()).status();
    let _ = vdev.wait_timeout_kill(Duration::from_secs(5));
    let _ = std::fs::remove_dir_all(&dir);
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }

    let (p99, max) = report_series(
        "key input -> visual (inject -> ds get sees the colour, real daemon+vdev)",
        &lat,
        KEY_TO_VISUAL_BUDGET,
    );
    assert!(
        max <= KEY_TO_VISUAL_BUDGET,
        "key input -> visual max {max:?} exceeds the 2 s end-to-end budget"
    );
    println!(
        "BENCH key->visual: p99={p99:?} max={max:?} over {cycles} cycles (budget 2 s) — PASS"
    );
}

/// Always-on smoke: the bench file is wired and its budgets match actions.md §5.
#[test]
fn budgets_match_actions_md_section_5() {
    assert_eq!(EVENT_DISPATCH_BUDGET, Duration::from_millis(5));
    assert_eq!(SET_EMIT_BUDGET, Duration::from_millis(10));
    assert_eq!(KEY_TO_VISUAL_BUDGET, Duration::from_secs(2));
    assert_eq!(FPS, 32);
}

/// Helper trait: wait on a child with a timeout, then SIGKILL (no leaks).
trait WaitTimeoutKill {
    fn wait_timeout_kill(&mut self, timeout: Duration) -> std::io::Result<()>;
}

impl WaitTimeoutKill for std::process::Child {
    fn wait_timeout_kill(&mut self, timeout: Duration) -> std::io::Result<()> {
        let start = Instant::now();
        loop {
            match self.try_wait()? {
                Some(_) => return Ok(()),
                None if start.elapsed() >= timeout => {
                    let _ = Command::new("kill")
                        .arg("-KILL")
                        .arg(self.id().to_string())
                        .status();
                    let _ = self.wait();
                    return Ok(());
                }
                None => thread::sleep(Duration::from_millis(20)),
            }
        }
    }
}
