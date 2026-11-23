//! DS-024 vdev test: a SIGTERM during an active stream exits cleanly, hands the virtual
//! deck back (software mode off, never backlight-0, nothing forbidden) and leaks no child
//! or socket; and a `kill -9` crash is recovered by the next start (stale `daemon.pid`
//! detected, re-handshake on connect, committed state reloaded).
//!
//! This file lives in the repo-level `tests/hw/` directory (like `tests/hw/hid.rs`) but is
//! compiled into the `ds-daemon` library test binary via `#[path]` from
//! `crates/ds-daemon/src/lib.rs`: registering a `[[test]]` target would require editing a
//! `Cargo.toml`, which is outside this ticket's `touches`. `cargo test -p ds-daemon` runs
//! it; no `DS_HARDWARE` needed (the "hardware" is the in-process ds-vdev).

use ds_daemon::guard;
use ds_vdev::{HubConfig, Throttle, VdevServer};
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Path of the daemon binary built by cargo for this package's tests. Cargo does not set
/// `CARGO_BIN_EXE_*` for library unit tests, so try the env var first, then the usual
/// target locations relative to the crate manifest.
fn daemon_bin() -> PathBuf {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_ds-daemon") {
        candidates.push(PathBuf::from(p));
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for profile in ["debug", "release"] {
        candidates.push(
            manifest
                .join("../../target")
                .join(profile)
                .join("ds-daemon"),
        );
    }
    // last resort: next to the test binary (target/<profile>/deps or a build out dir)
    if let Ok(exe) = std::env::current_exe() {
        let mut p = exe.clone();
        for _ in 0..6 {
            p.pop();
            let c = p.join("ds-daemon");
            if c.is_file() {
                candidates.push(c);
                break;
            }
        }
    }
    for c in &candidates {
        if c.is_file() {
            return c.clone();
        }
    }
    panic!(
        "ds-daemon binary not found; tried: {candidates:?} (current_exe={:?})",
        std::env::current_exe()
    )
}

/// The keys module pid (0x01fe) and the dial module pid (0x0206) of the virtual deck.
const KEYS_PID: &str = "pid=0x01fe";
const DIAL_PID: &str = "pid=0x0206";

struct DaemonChild {
    child: Child,
    data_dir: PathBuf,
    stderr_log: PathBuf,
}

impl DaemonChild {
    fn start(data_dir: &Path, device: &str) -> Self {
        fs::create_dir_all(data_dir).unwrap();
        let stderr_log = data_dir.join("daemon.stderr.log");
        let stderr = fs::File::create(&stderr_log).unwrap();
        let child = Command::new(daemon_bin())
            .args([
                "run",
                "--device",
                device,
                "--data-dir",
                &data_dir.display().to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("spawn ds-daemon");
        Self {
            child,
            data_dir: data_dir.to_path_buf(),
            stderr_log,
        }
    }

    fn socket(&self) -> PathBuf {
        self.data_dir.join("daemon.sock")
    }

    fn wait_ready(&mut self, timeout: Duration) {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if UnixStream::connect(self.socket()).is_ok() {
                return;
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!(
                    "ds-daemon exited early with {status}; stderr:\n{}",
                    self.stderr_text()
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!(
            "ds-daemon did not bind {} within {timeout:?}; stderr:\n{}",
            self.socket().display(),
            self.stderr_text()
        );
    }

    /// Send a signal and wait for exit; returns `(exit code, signal number)` — a signal
    /// death has `code() == None` and `signal() == Some(n)`.
    fn signal_and_wait(&mut self, sig: &str, timeout: Duration) -> (Option<i32>, Option<i32>) {
        use std::os::unix::process::ExitStatusExt;
        let pid = self.child.id();
        let status = Command::new("kill")
            .args([sig, &pid.to_string()])
            .status()
            .expect("run kill (argv-only, no shell)");
        assert!(status.success(), "kill {sig} {pid} failed");
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return (status.code(), status.signal());
            }
            if start.elapsed() > timeout {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!(
                    "ds-daemon did not exit within {timeout:?} after {sig}; stderr:\n{}",
                    self.stderr_text()
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn stderr_text(&self) -> String {
        fs::read_to_string(&self.stderr_log).unwrap_or_default()
    }
}

/// One NDJSON request/response round on the control socket.
struct ControlConn {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl ControlConn {
    fn connect(socket: &Path) -> Self {
        let writer = UnixStream::connect(socket).expect("connect to daemon.sock");
        let reader = BufReader::new(writer.try_clone().unwrap());
        Self { reader, writer }
    }

    fn round(&mut self, v: Value) -> Value {
        let mut line = serde_json::to_vec(&v).unwrap();
        line.push(b'\n');
        self.writer.write_all(&line).unwrap();
        self.writer.flush().unwrap();
        let mut line = String::new();
        assert!(self.reader.read_line(&mut line).unwrap() > 0);
        serde_json::from_str(line.trim()).expect("one JSON object per line")
    }

    fn hello_auth(&mut self, token: &str) {
        let r = self.round(json!({"id": 0, "op": "hello", "protocol": 1, "client": "ds-024-hw/1"}));
        assert_eq!(r["ok"], json!(true), "welcome: {r}");
        let r = self.round(json!({"id": 1, "op": "auth", "token": token, "scope": "admin"}));
        assert_eq!(r["ok"], json!(true), "auth: {r}");
    }
}

/// Poll `reports` on the vdev control socket until the keys module is in software mode.
fn wait_software_mode_on(vdev_sock: &Path, timeout: Duration) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(lines) = ds_vdev::ctl::request(vdev_sock, "reports") {
            if lines
                .iter()
                .any(|l| l.contains(KEYS_PID) && l.contains("software_mode=true"))
            {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "keys module never entered software mode (no handshake observed): {:?}",
        ds_vdev::ctl::request(vdev_sock, "reports")
    );
}

/// The `reports` line of one module pid.
fn report_of(vdev_sock: &Path, pid: &str) -> String {
    ds_vdev::ctl::request(vdev_sock, "reports")
        .unwrap()
        .into_iter()
        .find(|l| l.contains(pid))
        .unwrap_or_else(|| panic!("no report line for {pid}"))
}

/// The backlight percent in a `reports` line (`backlight=<n>`).
fn backlight_of(report: &str) -> u32 {
    let at = report.find("backlight=").expect("backlight field") + "backlight=".len();
    let rest = &report[at..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().expect("numeric backlight")
}

/// A large-ish PNG (noise) so the panel push has many chunks in flight when SIGTERM lands.
fn big_png(path: &Path) {
    let (w, h) = (640u32, 480u32);
    let mut raw = Vec::with_capacity((w * h * 3) as usize);
    let mut x: u32 = 0x12345678;
    for _ in 0..w * h {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        raw.extend_from_slice(&[
            (x & 0xff) as u8,
            ((x >> 8) & 0xff) as u8,
            ((x >> 16) & 0xff) as u8,
        ]);
    }
    let file = fs::File::create(path).unwrap();
    let mut enc = png::Encoder::new(file, w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().unwrap();
    w.write_image_data(&raw).unwrap();
}

#[test]
fn sigterm_during_active_stream_hands_back_and_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let vdev_sock = tmp.path().join("vdev.sock");
    // throttle the virtual deck so the panel push is still in flight when SIGTERM lands
    let _vdev = VdevServer::start(
        &vdev_sock,
        HubConfig {
            modules: ds_protocol::ModuleKind::ALL.to_vec(),
            throttle: Throttle {
                kbps: Some(2000),
                drop_after: None,
            },
        },
    )
    .unwrap();

    let data_dir = tmp.path().join("data");
    let mut daemon = DaemonChild::start(&data_dir, &format!("virtual:{}", vdev_sock.display()));
    daemon.wait_ready(Duration::from_secs(10));
    let token = fs::read_to_string(data_dir.join("token"))
        .unwrap()
        .trim()
        .to_string();
    let mut conn = ControlConn::connect(&daemon.socket());
    conn.hello_auth(&token);

    // the daemon's connect handshake put keys/dial into software mode
    wait_software_mode_on(&vdev_sock, Duration::from_secs(5));
    assert!(report_of(&vdev_sock, DIAL_PID).contains("software_mode=true"));

    // start a big dial-screen push (many chunks, throttled) …
    let png = tmp.path().join("dial.png");
    big_png(&png);
    let r = conn.round(json!({
        "id": 2, "op": "set",
        "ops": [{"op": "set", "path": "/controls/dial/image",
                 "value": {"kind": "file", "path": png.display().to_string()}}]
    }));
    assert_eq!(r["ok"], json!(true), "set: {r}");
    // … and SIGTERM while the stream is active
    let (code, sig) = daemon.signal_and_wait("-TERM", Duration::from_secs(15));
    assert_eq!(
        (code, sig),
        (Some(0), None),
        "SIGTERM during an active stream must exit 0; stderr:\n{}",
        daemon.stderr_text()
    );

    // no leaked child (reaped above) and no leaked socket/pid files
    assert!(!daemon.socket().exists(), "control socket removed on exit");
    assert!(
        !data_dir.join("daemon.pid").exists(),
        "pid file removed on exit"
    );
    assert!(
        vdev_sock.exists(),
        "the vdev socket belongs to the test, not the daemon"
    );

    // the handback was observed on the device: keys and dial are back in firmware mode
    // (software_mode=false), and nothing was ever refused (no forbidden reports)
    let keys = report_of(&vdev_sock, KEYS_PID);
    let dial = report_of(&vdev_sock, DIAL_PID);
    assert!(
        keys.contains("software_mode=false"),
        "handback must leave software mode on keys: {keys}"
    );
    assert!(
        dial.contains("software_mode=false"),
        "handback must leave software mode on dial: {dial}"
    );
    // never backlight-0 (invariant): the backlight field stays at a policy value
    for line in [keys, dial] {
        assert!(backlight_of(&line) > 0, "backlight must never be 0: {line}");
    }
    let writes = ds_vdev::ctl::request(&vdev_sock, "writes 500").unwrap();
    assert!(
        writes.iter().all(|l| !l.contains("refused")),
        "no report may be refused (forbidden) during handback: {writes:?}"
    );
    // the guard logged its handback steps, every stderr line is structured JSON
    let log = daemon.stderr_text();
    assert!(
        log.contains(guard::GUARD_TARGET),
        "guard log lines present:\n{log}"
    );
    assert!(log.contains("handback"), "handback step logged:\n{log}");
    for line in log.lines() {
        let v: Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("log line not JSON ({e}): {line}"));
        assert!(v["level"].is_string() && v["target"].is_string() && v["msg"].is_string());
    }
}

#[test]
fn kill9_leaves_stale_pid_and_the_next_start_recovers() {
    let tmp = tempfile::tempdir().unwrap();
    let vdev_sock = tmp.path().join("vdev.sock");
    let _vdev = VdevServer::start(&vdev_sock, HubConfig::default()).unwrap();
    let data_dir = tmp.path().join("data");

    // first run: commit a change, confirm the handshake, then crash hard (kill -9)
    let mut first = DaemonChild::start(&data_dir, &format!("virtual:{}", vdev_sock.display()));
    first.wait_ready(Duration::from_secs(10));
    let token = fs::read_to_string(data_dir.join("token"))
        .unwrap()
        .trim()
        .to_string();
    let mut conn = ControlConn::connect(&first.socket());
    conn.hello_auth(&token);
    let r = conn.round(json!({
        "id": 2, "op": "set",
        "ops": [{"op": "set", "path": "/controls/keys/0/image",
                 "value": {"kind": "color", "value": "#3cc86e"}}]
    }));
    assert_eq!(r["ok"], json!(true), "set: {r}");
    assert_eq!(r["rev"], json!(1));
    wait_software_mode_on(&vdev_sock, Duration::from_secs(5));
    let (code, sig) = first.signal_and_wait("-KILL", Duration::from_secs(10));
    assert_eq!(
        (code, sig),
        (None, Some(9)),
        "kill -9 must kill by signal 9: {code:?} {sig:?}"
    );
    // the crash left the pid file and socket behind — that is the recovery evidence
    assert!(
        data_dir.join("daemon.pid").exists(),
        "a killed daemon leaves daemon.pid behind (no cleanup on crash)"
    );
    assert!(
        first.socket().exists(),
        "a killed daemon leaves daemon.sock behind"
    );

    // second run on the same data dir: detects the stale run, re-handshakes, reloads state
    let mut second = DaemonChild::start(&data_dir, &format!("virtual:{}", vdev_sock.display()));
    second.wait_ready(Duration::from_secs(10));
    let log = second.stderr_text();
    assert!(
        log.contains("did not exit cleanly"),
        "the next start must log the stale previous run:\n{log}"
    );
    let mut conn = ControlConn::connect(&second.socket());
    conn.hello_auth(&token);
    let r = conn.round(json!({"id": 3, "op": "get", "path": "/"}));
    assert_eq!(r["ok"], json!(true));
    assert_eq!(
        r["rev"],
        json!(1),
        "the atomically committed state survived the crash: {r}"
    );
    // re-handshake on connect: keys back in software mode
    wait_software_mode_on(&vdev_sock, Duration::from_secs(5));

    // and a clean SIGTERM on the recovered run still hands back and exits 0
    let (code, sig) = second.signal_and_wait("-TERM", Duration::from_secs(10));
    assert_eq!(
        (code, sig),
        (Some(0), None),
        "stderr:\n{}",
        second.stderr_text()
    );
    assert!(!second.socket().exists());
    assert!(!data_dir.join("daemon.pid").exists());
    assert!(report_of(&vdev_sock, KEYS_PID).contains("software_mode=false"));
}
