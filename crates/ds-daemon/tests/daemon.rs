//! Integration test for DS-015: builds nothing, but runs the real `ds-daemon` binary
//! (via `CARGO_BIN_EXE_ds-daemon`) on a temp data dir against a ds-vdev virtual device
//! socket, speaks raw NDJSON (hello/auth/ping/get/set) over the control socket, asserts
//! the permission bits and the empty initial state, pushes a color to the device, and
//! finally sends SIGTERM and asserts a clean exit 0 with socket + pid cleanup.

use ds_vdev::{HubConfig, VdevServer};
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
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

/// Poll `frames <target>` on the vdev control socket until a frame exists; returns the
/// `target <sha256> <n>B seq=<k>` line, or `None` on timeout.
fn wait_for_frame(vdev_sock: &Path, target: &str, timeout: Duration) -> Option<String> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(lines) = ds_vdev::ctl::request(vdev_sock, &format!("frames {target}")) {
            if let Some(line) = lines.first() {
                return Some(line.clone());
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// Boot a vdev + daemon pair on a temp dir and return everything a test needs.
struct Stack {
    _tmp: tempfile::TempDir,
    vdev_sock: PathBuf,
    _vdev: ds_vdev::VdevServer,
    daemon: DaemonChild,
}

impl Stack {
    fn start() -> (Self, ControlConn, String) {
        let tmp = tempfile::tempdir().unwrap();
        let vdev_sock = tmp.path().join("vdev.sock");
        let vdev = VdevServer::start(&vdev_sock, HubConfig::default()).unwrap();
        let data_dir = tmp.path().join("data");
        let mut daemon = DaemonChild::start(&data_dir, &format!("virtual:{}", vdev_sock.display()));
        daemon.wait_ready(Duration::from_secs(10));
        let token = fs::read_to_string(data_dir.join("token"))
            .unwrap()
            .trim()
            .to_string();
        let mut conn = ControlConn::connect(&daemon.socket());
        conn.hello_auth(&token);
        let stack = Stack {
            _tmp: tmp,
            vdev_sock,
            _vdev: vdev,
            daemon,
        };
        (stack, conn, token)
    }

    fn set(&mut self, conn: &mut ControlConn, id: u64, path: &str, value: Value) -> Value {
        let r = conn.round(json!({
            "id": id, "op": "set",
            "ops": [{"op": "set", "path": path, "value": value}]
        }));
        assert_eq!(r["ok"], json!(true), "set {path}: {r}");
        r
    }
}

#[test]
fn file_image_push_reassembles_on_vdev_with_the_encoded_frames_sha256() {
    let (mut stack, mut conn, _token) = Stack::start();
    // allow a dedicated dir as an fs root, then set a key image from a file inside it
    let allowed = stack._tmp.path().join("allowed");
    fs::create_dir_all(&allowed).unwrap();
    let img = allowed.join("key-image.jpg");
    // a real wire image (a JPEG the frame plane itself produced): DS-030 re-encodes it to
    // the device wire format, so the vdev frame sha is the ENCODED frame's sha, not the file's
    let source = ds_daemon::frames::encode::encode_solid(
        ds_daemon::frames::FrameTarget::Key(0),
        [0x3c, 0xc8, 0x6e],
    );
    fs::write(&img, &source).unwrap();
    let img_path = img.display().to_string();
    stack.set(
        &mut conn,
        1,
        "/policy/fs/roots",
        json!([allowed.display().to_string()]),
    );
    stack.set(
        &mut conn,
        2,
        "/controls/keys/0/image",
        json!({"kind": "file", "path": img_path}),
    );
    // the state round-trips the visual
    let r = conn.round(json!({"id": 3, "op": "get", "path": "/controls/keys/0/image"}));
    assert_eq!(
        r["result"]["value"],
        json!({"kind": "file", "path": img_path})
    );
    // and the device task pushed the re-encoded JPEG: the vdev frame for key:0 has its sha
    let encoded =
        ds_daemon::frames::encode::encode_frame(ds_daemon::frames::FrameTarget::Key(0), &source)
            .unwrap();
    let line = wait_for_frame(&stack.vdev_sock, "key:0", Duration::from_secs(10))
        .expect("a reassembled frame for key:0 after the file set");
    let want = ds_vdev::sha256_hex(&encoded);
    assert!(
        line.contains(&want),
        "frame line {line:?} must contain sha256(encoded frame) {want}"
    );
    assert!(
        line.contains(&format!("{}B", encoded.len())),
        "frame line {line:?} must report the encoded frame's byte count"
    );
    let code = stack.daemon.sigterm_and_wait(Duration::from_secs(10));
    assert_eq!(code, Some(0), "stderr:\n{}", stack.daemon.stderr_text());
}

#[test]
fn non_image_file_push_is_refused_with_e_format_and_no_frame() {
    let (mut stack, mut conn, _token) = Stack::start();
    let allowed = stack._tmp.path().join("allowed");
    fs::create_dir_all(&allowed).unwrap();
    stack.set(
        &mut conn,
        1,
        "/policy/fs/roots",
        json!([allowed.display().to_string()]),
    );
    // a file inside the roots but not a decodable image: the set is accepted (schema-valid),
    // the device push is refused with E_FORMAT and no frame is stored
    let junk = allowed.join("junk.bin");
    fs::write(&junk, b"this is not a PNG/JPEG/GIF").unwrap();
    stack.set(
        &mut conn,
        2,
        "/controls/keys/1/image",
        json!({"kind": "file", "path": junk.display().to_string()}),
    );
    std::thread::sleep(Duration::from_millis(700));
    assert!(
        ds_vdev::ctl::request(&stack.vdev_sock, "frames key:1").is_err(),
        "no frame may be stored for a non-image file (E_FORMAT drop)"
    );
    let log = stack.daemon.stderr_text();
    assert!(
        log.contains("E_FORMAT"),
        "the E_FORMAT refusal must be logged; stderr:\n{log}"
    );
    let r = conn.round(json!({"id": 3, "op": "ping"}));
    assert_eq!(r["result"], json!({"pong": true}));
    let code = stack.daemon.sigterm_and_wait(Duration::from_secs(10));
    assert_eq!(code, Some(0), "stderr:\n{}", stack.daemon.stderr_text());
}

#[test]
fn colour_image_push_yields_a_frame_for_key_and_dial() {
    let (mut stack, mut conn, _token) = Stack::start();
    stack.set(
        &mut conn,
        1,
        "/controls/keys/2/image",
        json!({"kind": "color", "value": "#3cc86e"}),
    );
    stack.set(
        &mut conn,
        2,
        "/controls/dial/image",
        json!({"kind": "color", "value": "#0a0b0c"}),
    );
    let key_line = wait_for_frame(&stack.vdev_sock, "key:2", Duration::from_secs(10))
        .expect("a frame for key:2 after the colour set");
    // DS-030: the colour is encoded to the target's device JPEG (q90 key, q85 dial)
    let want_key = ds_vdev::sha256_hex(&ds_daemon::frames::encode::encode_solid(
        ds_daemon::frames::FrameTarget::Key(2),
        [0x3c, 0xc8, 0x6e],
    ));
    assert!(
        key_line.contains(&want_key),
        "frame {key_line:?} must be the encoded solid-colour JPEG for key:2 ({want_key})"
    );
    let dial_line = wait_for_frame(&stack.vdev_sock, "dial", Duration::from_secs(10))
        .expect("a frame for the dial after the colour set");
    let want_dial = ds_vdev::sha256_hex(&ds_daemon::frames::encode::encode_solid(
        ds_daemon::frames::FrameTarget::Dial,
        [0x0a, 0x0b, 0x0c],
    ));
    assert!(
        dial_line.contains(&want_dial),
        "frame {dial_line:?} vs {want_dial}"
    );
    let code = stack.daemon.sigterm_and_wait(Duration::from_secs(10));
    assert_eq!(code, Some(0), "stderr:\n{}", stack.daemon.stderr_text());
}

#[test]
fn file_image_outside_fs_roots_is_refused_with_a_logged_error_and_no_frame() {
    let (mut stack, mut conn, _token) = Stack::start();
    // roots point at an empty dir; /etc/hostname is outside every root
    let empty = stack._tmp.path().join("empty");
    fs::create_dir_all(&empty).unwrap();
    stack.set(
        &mut conn,
        1,
        "/policy/fs/roots",
        json!([empty.display().to_string()]),
    );
    // the set itself is accepted (schema-valid path) — the device push is what refuses
    stack.set(
        &mut conn,
        2,
        "/controls/keys/1/image",
        json!({"kind": "file", "path": "/etc/hostname"}),
    );
    // give the device task time to process the push (channel FIFO after the set reply)
    std::thread::sleep(Duration::from_millis(700));
    assert!(
        ds_vdev::ctl::request(&stack.vdev_sock, "frames key:1").is_err(),
        "no frame may be stored for a file outside /policy/fs/roots"
    );
    // the refusal was logged, and the daemon is still healthy (IPC + state unaffected)
    let log = stack.daemon.stderr_text();
    assert!(
        log.contains("outside /policy/fs/roots"),
        "the refusal must be logged; stderr:\n{log}"
    );
    let r = conn.round(json!({"id": 3, "op": "ping"}));
    assert_eq!(r["result"], json!({"pong": true}));
    let r = conn.round(json!({"id": 4, "op": "get", "path": "/controls/keys/1/image"}));
    assert_eq!(
        r["result"]["value"],
        json!({"kind": "file", "path": "/etc/hostname"})
    );
    let code = stack.daemon.sigterm_and_wait(Duration::from_secs(10));
    assert_eq!(code, Some(0), "stderr:\n{}", stack.daemon.stderr_text());
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
    // The device task sends the software-mode handshake on connect (FEATURE_SET, kind=0x02
    // to the keys pid 0x01fe) and pushes the colour image as a 0x36 chunked WRITE, which
    // ds-vdev reassembles into the key:0 frame.
    let mut handshake_writes = 0;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if let Ok(lines) = ds_vdev::ctl::request(&vdev_sock, "writes 50") {
            handshake_writes = lines
                .iter()
                .filter(|l| l.contains("pid=0x01fe") && l.contains("kind=0x02"))
                .count();
            if handshake_writes >= 1 {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        handshake_writes >= 1,
        "expected the software-mode handshake write on pid 0x01fe; writes: {:?}",
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
    // the colour image was pushed as chunks and reassembled into the key:0 frame
    let frame = wait_for_frame(&vdev_sock, "key:0", Duration::from_secs(5))
        .expect("a reassembled frame for key:0 after the colour set");
    let want = ds_vdev::sha256_hex(&ds_daemon::frames::encode::encode_solid(
        ds_daemon::frames::FrameTarget::Key(0),
        [0x3c, 0xc8, 0x6e],
    ));
    assert!(
        frame.contains(&want),
        "frame {frame:?} must carry the sha256 of the encoded colour JPEG {want}"
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

/// Producer-side stream client: `DSctl\n` NDJSON control lines plus DSF1 frames on
/// `<dir>/stream.sock`, the same wire form `ds stream push` uses.
struct StreamConn {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl StreamConn {
    fn connect(path: &Path) -> Self {
        let writer = UnixStream::connect(path).expect("connect to stream.sock");
        writer
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let reader = BufReader::new(writer.try_clone().unwrap());
        Self { reader, writer }
    }

    fn send_line(&mut self, v: &Value) {
        self.writer.write_all(b"DSctl\n").unwrap();
        self.writer
            .write_all(&serde_json::to_vec(v).unwrap())
            .unwrap();
        self.writer.write_all(b"\n").unwrap();
        self.writer.flush().unwrap();
    }

    fn recv_line(&mut self) -> Value {
        let mut magic = [0u8; 6];
        self.reader.read_exact(&mut magic).expect("DSctl magic");
        assert_eq!(&magic, b"DSctl\n", "expected a DSctl reply");
        let line = ds_ipc::framing::read_line(
            &mut self.reader,
            ds_ipc::frames::limits::MAX_DSCTL_LINE_BYTES,
        )
        .expect("one DSctl line");
        serde_json::from_str(line.trim()).expect("one JSON object per line")
    }

    fn send_frame(&mut self, frame: &ds_ipc::frames::codec::Frame) {
        self.writer.write_all(&frame.encode().unwrap()).unwrap();
        self.writer.flush().unwrap();
    }
}

/// The running daemon must serve the frame plane itself: `<dir>/stream.sock` carries a
/// `stream-open` + DSF1 dial frame into the latest-wins queues and out to the device.
/// Wiring only `FrameService` in tests and not in `daemon::run` leaves the socket missing.
#[test]
fn stream_socket_carries_a_dial_frame_to_the_device() {
    let (mut stack, mut conn, _token) = Stack::start();
    let stream_sock = stack.daemon.data_dir.join("stream.sock");

    // The control socket is the readiness gate; the frame plane binds alongside it.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !stream_sock.exists() {
        assert!(
            Instant::now() < deadline,
            "stream.sock was never bound; stderr:\n{}",
            stack.daemon.stderr_text()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(mode_of(&stream_sock), 0o600);

    // Frames are refused until the device task has completed the software-mode handshake;
    // wait for the attach instead of racing it.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !stack
        .daemon
        .stderr_text()
        .contains("device transport attached")
    {
        assert!(
            Instant::now() < deadline,
            "the frame plane never got the device transport; stderr:\n{}",
            stack.daemon.stderr_text()
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    let mut sc = StreamConn::connect(&stream_sock);
    sc.send_line(&json!({"op": "stream-open", "target": "dial", "credits": 4}));
    let credit = sc.recv_line();
    assert_eq!(credit["op"], json!("stream-credit"), "{credit}");
    assert_eq!(credit["credits"], json!(4));

    let target = ds_daemon::frames::FrameTarget::Dial;
    let wire = ds_daemon::frames::encode::encode_solid(target, [0x20, 0x60, 0xa0]);
    let meta = ds_ipc::frames::codec::FrameMeta {
        target: "dial".into(),
        seq: 1,
        ts_ms: 1_732_000_000_000,
        format: ds_ipc::frames::codec::FrameFormat::Jpeg,
        w: Some(240),
        h: Some(240),
        frames: Some(1),
        fps: None,
        loop_: None,
        crc32: 0,
        max_payload_bytes: ds_ipc::frames::limits::MAX_PAYLOAD_BYTES as u32,
    };
    sc.send_frame(&ds_ipc::frames::codec::Frame::new(meta, &wire));
    let ack = sc.recv_line();
    assert_eq!(ack["op"], json!("stream-ack"), "{ack}");
    assert_eq!(ack["seq"], json!(1));

    let line = wait_for_frame(&stack.vdev_sock, "dial", Duration::from_secs(10))
        .expect("the dial frame must reach the device through the frame plane");
    let encoded = ds_daemon::frames::encode::encode_frame(target, &wire).unwrap();
    let want = ds_vdev::sha256_hex(&encoded);
    assert!(
        line.contains(&want),
        "frame line {line:?} must contain {want}"
    );

    // The same counters `ds stream stats dial` reads must show the delivered frame.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let tel = conn.round(json!({"id": 90, "op": "get", "path": "/telemetry/streams/dial"}));
        if tel["result"]["value"]["sent"].as_u64().unwrap_or(0) >= 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no stream telemetry after the ack: {tel}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    let code = stack.daemon.sigterm_and_wait(Duration::from_secs(10));
    assert_eq!(code, Some(0), "stderr:\n{}", stack.daemon.stderr_text());
    assert!(!stream_sock.exists(), "a clean stop removes stream.sock");
}
