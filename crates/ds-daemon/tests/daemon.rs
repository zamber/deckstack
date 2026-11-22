//! Integration test for DS-015: builds nothing, but runs the real `ds-daemon` binary
//! (via `CARGO_BIN_EXE_ds-daemon`) on a temp data dir against a ds-vdev virtual device
//! socket, speaks raw NDJSON (hello/auth/ping/get/set) over the control socket, asserts
//! the permission bits and the empty initial state, pushes a color to the device, and
//! finally sends SIGTERM and asserts a clean exit 0 with socket + pid cleanup.

use ds_vdev::{HubConfig, VdevServer};
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Path of the daemon binary built by cargo for this package's integration tests.
fn daemon_bin() -> &'static str {
    env!("CARGO_BIN_EXE_ds-daemon")
}

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

    /// Wait until the control socket accepts connections (or fail after `timeout`).
    fn wait_ready(&mut self, timeout: Duration) {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if UnixStream::connect(self.socket()).is_ok() {
                return;
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!(
                    "ds-daemon exited early with {status}; stderr:\n{}",
                    fs::read_to_string(&self.stderr_log).unwrap_or_default()
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!(
            "ds-daemon did not bind {} within {:?}; stderr:\n{}",
            self.socket().display(),
            timeout,
            fs::read_to_string(&self.stderr_log).unwrap_or_default()
        );
    }

    /// Send SIGTERM to the daemon and wait for it to exit; returns the exit code.
    fn sigterm_and_wait(&mut self, timeout: Duration) -> Option<i32> {
        let pid = self.child.id();
        let status = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .expect("run kill -TERM (argv-only, no shell)");
        assert!(status.success(), "kill -TERM {pid} failed");
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code();
            }
            if start.elapsed() > timeout {
                let _ = self.child.kill();
                panic!("ds-daemon did not exit within {timeout:?} after SIGTERM");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn stderr_text(&self) -> String {
        fs::read_to_string(&self.stderr_log).unwrap_or_default()
    }
}

/// One NDJSON request/response round on a control connection.
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

    fn send(&mut self, v: Value) {
        let mut line = serde_json::to_vec(&v).unwrap();
        line.push(b'\n');
        self.writer.write_all(&line).unwrap();
        self.writer.flush().unwrap();
    }

    fn recv(&mut self) -> Value {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).unwrap();
        assert!(n > 0, "daemon closed the connection");
        serde_json::from_str(line.trim()).expect("one JSON object per line")
    }

    fn round(&mut self, v: Value) -> Value {
        self.send(v);
        self.recv()
    }

    fn hello_auth(&mut self, token: &str) {
        let r = self
            .round(json!({"id": 0, "op": "hello", "protocol": 1, "client": "ds-daemon-itest/1"}));
        assert_eq!(r["ok"], json!(true), "welcome: {r}");
        assert_eq!(r["result"]["protocol"], json!(1));
        assert_eq!(
            r["result"]["server"],
            json!(format!("ds-daemon/{}", ds_daemon::version()))
        );
        let r = self.round(json!({"id": 1, "op": "auth", "token": token, "scope": "admin"}));
        assert_eq!(r["ok"], json!(true), "auth: {r}");
        assert_eq!(r["result"]["scope"], json!("admin"));
    }
}

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn daemon_end_to_end_on_virtual_device() {
    let tmp = tempfile::tempdir().unwrap();
    // The virtual MasterHub (ds-vdev library, DS-017) on a temp socket.
    let vdev_sock = tmp.path().join("vdev.sock");
    let _vdev = VdevServer::start(&vdev_sock, HubConfig::default()).unwrap();

    let data_dir = tmp.path().join("data");
    let mut daemon = DaemonChild::start(&data_dir, &format!("virtual:{}", vdev_sock.display()));
    daemon.wait_ready(Duration::from_secs(10));

    // --- permission bits -------------------------------------------------------
    assert_eq!(mode_of(&data_dir), 0o700, "data dir must be 0700");
    assert_eq!(
        mode_of(&daemon.socket()),
        0o600,
        "control socket must be 0600"
    );
    let token_path = data_dir.join("token");
    assert_eq!(mode_of(&token_path), 0o600, "token file must be 0600");
    let token = fs::read_to_string(&token_path).unwrap().trim().to_string();
    assert_eq!(token.len(), 64, "random 32-byte hex token");

    // --- raw NDJSON session: hello/auth + get/set ------------------------------
    let mut conn = ControlConn::connect(&daemon.socket());
    let welcome =
        conn.round(json!({"id": 0, "op": "hello", "protocol": 1, "client": "ds-daemon-itest/1"}));
    assert_eq!(welcome["ok"], json!(true));
    assert_eq!(
        welcome["result"]["rev"],
        json!(0),
        "empty state at start: rev 0 in welcome"
    );
    // bad token is refused
    let r = conn.round(json!({"id": 1, "op": "auth", "token": "0".repeat(64)}));
    assert_eq!(r["ok"], json!(false));
    assert_eq!(r["error"]["code"], json!("E_AUTH"));
    // install token authenticates as admin
    let r = conn.round(json!({"id": 2, "op": "auth", "token": token, "scope": "admin"}));
    assert_eq!(r["ok"], json!(true));
    assert_eq!(r["result"]["scope"], json!("admin"));
    // ping (read-only status op)
    let r = conn.round(json!({"id": 3, "op": "ping"}));
    assert_eq!(r["result"], json!({"pong": true}));
    // state doc starts empty
    let r = conn.round(json!({"id": 4, "op": "get", "path": "/"}));
    assert_eq!(r["ok"], json!(true));
    assert_eq!(r["rev"], json!(0));
    assert_eq!(r["result"]["value"]["controls"], json!({}));
    assert_eq!(r["result"]["value"]["bindings"], json!([]));
    assert_eq!(r["result"]["value"]["plugins"], json!({}));
    // set: one color on key 0
    let r = conn.round(json!({
        "id": 5, "op": "set",
        "ops": [{"op": "set", "path": "/controls/keys/0/image",
                 "value": {"kind": "color", "value": "#3cc86e"}}]
    }));
    assert_eq!(r["ok"], json!(true), "set: {r}");
    assert_eq!(r["rev"], json!(1));
    // get it back
    let r = conn.round(json!({"id": 6, "op": "get", "path": "/controls/keys/0/image"}));
    assert_eq!(r["ok"], json!(true));
    assert_eq!(r["rev"], json!(1));
    assert_eq!(
        r["result"]["value"],
        json!({"kind": "color", "value": "#3cc86e"})
    );
    // stale if_rev -> E_CONFLICT with current_rev
    let r = conn.round(json!({
        "id": 7, "op": "set", "if_rev": 0,
        "ops": [{"op": "set", "path": "/settings/x", "value": 1}]
    }));
    assert_eq!(r["ok"], json!(false));
    assert_eq!(r["error"]["code"], json!("E_CONFLICT"));
    assert_eq!(r["error"]["current_rev"], json!(1));
    // daemon status (read-only)
    let r = conn.round(json!({"id": 8, "op": "daemon", "action": "status"}));
    assert_eq!(r["ok"], json!(true));
    assert_eq!(r["result"]["running"], json!(true));
    assert_eq!(r["result"]["rev"], json!(1));
    assert!(r["result"]["device_status"]
        .as_str()
        .unwrap()
        .starts_with("connected"));

    // --- the color push reached the virtual device ------------------------------
    // The device task sends the software-mode handshake on connect and the fill_key
    // feature report (03 37 00 3c c8 6e) after the set — both are FEATURE_SET (kind=0x02)
    // writes to the keys pid 0x01fe, so wait for a second write to that pid.
    let mut key_writes = 0;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if let Ok(lines) = ds_vdev::ctl::request(&vdev_sock, "writes 50") {
            key_writes = lines
                .iter()
                .filter(|l| l.contains("pid=0x01fe") && l.contains("kind=0x02"))
                .count();
            if key_writes >= 2 {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        key_writes >= 2,
        "expected the handshake + fill_key writes on pid 0x01fe; writes: {:?}",
        ds_vdev::ctl::request(&vdev_sock, "writes 50")
    );
    // the handshake put the keys module into software mode
    let reports = ds_vdev::ctl::request(&vdev_sock, "reports").unwrap();
    assert!(
        reports
            .iter()
            .any(|r| r.contains("pid=0x01fe") && r.contains("software_mode=true")),
        "keys module should be in software mode: {reports:?}"
    );

    // --- SIGTERM exits 0 and cleans up ------------------------------------------
    let code = daemon.sigterm_and_wait(Duration::from_secs(10));
    assert_eq!(
        code,
        Some(0),
        "SIGTERM must exit 0; stderr:\n{}",
        daemon.stderr_text()
    );
    assert!(!daemon.socket().exists(), "control socket removed on exit");
    assert!(
        !data_dir.join("daemon.pid").exists(),
        "pid file removed on exit"
    );
    // structured lifecycle logs went to stderr
    let log = daemon.stderr_text();
    assert!(log.contains("\"listening on"), "log:\n{log}");
    assert!(log.contains("shutdown requested"), "log:\n{log}");
    for line in log.lines() {
        let v: Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("log line not JSON ({e}): {line}"));
        assert!(v["level"].is_string() && v["target"].is_string() && v["msg"].is_string());
    }
}

#[test]
fn daemon_runs_with_device_none_and_exits_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let mut daemon = DaemonChild::start(&data_dir, "none");
    daemon.wait_ready(Duration::from_secs(10));

    assert_eq!(mode_of(&data_dir), 0o700);
    assert_eq!(mode_of(&daemon.socket()), 0o600);
    let token = fs::read_to_string(data_dir.join("token"))
        .unwrap()
        .trim()
        .to_string();

    let mut conn = ControlConn::connect(&daemon.socket());
    conn.hello_auth(&token);
    let r = conn.round(json!({"id": 2, "op": "ping"}));
    assert_eq!(r["result"], json!({"pong": true}));
    let r = conn.round(json!({"id": 3, "op": "get", "path": "/meta"}));
    assert_eq!(r["ok"], json!(true));
    assert_eq!(
        r["result"]["value"]["rev"],
        json!(0),
        "empty state at start"
    );
    let r = conn.round(json!({
        "id": 4, "op": "set",
        "ops": [{"op": "set", "path": "/controls/dial/image",
                 "value": {"kind": "color", "value": "#0a0b0c"}}]
    }));
    assert_eq!(r["rev"], json!(1));
    let r = conn.round(json!({"id": 5, "op": "daemon", "action": "status"}));
    assert_eq!(r["result"]["device"], json!("none"));
    assert_eq!(r["result"]["device_status"], json!("none"));

    let code = daemon.sigterm_and_wait(Duration::from_secs(10));
    assert_eq!(code, Some(0), "stderr:\n{}", daemon.stderr_text());
    assert!(!daemon.socket().exists());
    assert!(!data_dir.join("daemon.pid").exists());
}

#[test]
fn version_and_guard_and_usage() {
    let out = Command::new(daemon_bin())
        .arg("--version")
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.trim(), format!("ds-daemon {}", ds_daemon::version()));

    let out = Command::new(daemon_bin()).arg("--guard").output().unwrap();
    assert!(out.status.success(), "guard stub exits 0");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("DS-024"),
        "guard stub logs the DS-024 pointer: {stderr}"
    );

    let out = Command::new(daemon_bin()).arg("--bogus").output().unwrap();
    assert_eq!(out.status.code(), Some(1), "usage error exits 1");
}
