//! Integration test for the `ds` binary: an in-process daemon (the ds-daemon
//! library, dev-dependency) serving a ds-vdev virtual device socket, driven through
//! the compiled `ds` binary (`env!("CARGO_BIN_EXE_ds")`) exactly as a user would —
//! PLAN.md §First walking skeleton steps 3–6 plus the exit-code/output contract.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn ds_bin() -> &'static str {
    env!("CARGO_BIN_EXE_ds")
}

/// Run `ds --socket <socket> <args>`.
fn run(socket: &Path, args: &[&str]) -> Output {
    Command::new(ds_bin())
        .arg("--socket")
        .arg(socket)
        .args(args)
        .output()
        .expect("spawn ds")
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// stdout must be exactly one JSON value (the `--json` contract).
fn stdout_json(out: &Output) -> Value {
    let s = stdout(out);
    serde_json::from_str(s.trim())
        .unwrap_or_else(|e| panic!("stdout is not one JSON object: {e}\n{s}"))
}

/// Wait with a timeout so a wedged child fails the test instead of hanging it.
fn wait_with_timeout(mut child: std::process::Child, timeout: Duration) -> Output {
    use std::io::Read;
    let deadline = Instant::now() + timeout;
    let mut pipe = child.stdout.take().expect("piped stdout");
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            let _ = child.wait();
            return Output {
                status,
                stdout: buf,
                stderr: Vec::new(),
            };
        }
        assert!(
            Instant::now() < deadline,
            "child did not exit within {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Poll until the control socket answers a full hello+auth+status round trip.
fn wait_for_daemon(socket: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if socket.exists() {
            if let Ok(mut c) = ds_client::Client::connect(socket) {
                if c.hello("ds-test/0.1").is_ok()
                    && c.authenticate_token_file(None).is_ok()
                    && c.request(json!({"op": "daemon", "action": "status"}))
                        .is_ok()
                {
                    return;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "daemon did not come up on {}",
            socket.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn ds_cli_end_to_end_against_in_process_daemon_on_vdev() {
    let tmp = tempfile::tempdir().unwrap();
    let vdev_sock = tmp.path().join("vdev.sock");
    let vdev = ds_vdev::VdevServer::start(&vdev_sock, ds_vdev::HubConfig::default()).unwrap();

    // Start the daemon IN-PROCESS from the ds-daemon library (dev-dependency),
    // bound to the ds-vdev socket, on a temp data dir.
    let data_dir = tmp.path().join("data");
    let device = format!("virtual:{}", vdev_sock.display());
    let dd = data_dir.clone();
    let dev = device.clone();
    let daemon_thread = std::thread::spawn(move || {
        ds_daemon::daemon::run(ds_daemon::daemon::RunOptions {
            device: Some(dev),
            data_dir: Some(dd),
            preview: false,
        })
    });
    let socket = data_dir.join("daemon.sock");
    wait_for_daemon(&socket, Duration::from_secs(10));

    // 1. set a color visual on /controls/keys/0/image (inline value + --json flag)
    let out = run(
        &socket,
        &[
            "set",
            "/controls/keys/0/image",
            "--json",
            r##"{"kind":"color","value":"#3cc86e"}"##,
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let v = stdout_json(&out);
    assert_eq!(v["ok"], json!(true));
    assert_eq!(
        v["rev"],
        json!(1),
        "first set on empty state bumps rev to 1: {v}"
    );
    // human output of the same write is one fact line
    let out = run(&socket, &["set", "/controls/keys/0/brightness", "55"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(stdout(&out).starts_with("ok rev="), "{}", stdout(&out));

    // 2. get it back with --json: exactly one object, value round-trips
    let out = run(&socket, &["get", "/controls/keys/0", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let v = stdout_json(&out);
    assert_eq!(v["path"], json!("/controls/keys/0"));
    assert_eq!(v["value"]["image"]["value"], json!("#3cc86e"));
    assert!(v["rev"].is_u64(), "durable reads carry rev: {v}");

    // 2b. human get prints the human value
    let out = run(&socket, &["get", "/controls/keys/0/image"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(stdout(&out).contains("#3cc86e"), "{}", stdout(&out));

    // 3. set a scalar (inline JSON string) and read it back with --fields
    let out = run(&socket, &["set", "/controls/keys/0/label", "\"Salon\""]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let out = run(
        &socket,
        &["get", "/controls/keys/0", "--fields", "label", "--json"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(stdout_json(&out)["value"], json!({"label": "Salon"}));

    // 4. --if-rev conflict → exit 3, current_rev in the --json error envelope
    let out = run(&socket, &["set", "/settings/x", "1", "--if-rev", "0"]);
    assert_eq!(code(&out), 3, "{}", stderr(&out));
    let out = run(
        &socket,
        &["set", "/settings/x", "1", "--if-rev", "0", "--json"],
    );
    assert_eq!(code(&out), 3, "{}", stderr(&out));
    let e: Value = serde_json::from_str(stderr(&out).trim()).unwrap();
    assert_eq!(e["ok"], json!(false));
    assert_eq!(e["error"]["code"], json!("E_CONFLICT"));
    assert!(e["error"]["current_rev"].is_u64());

    // 5. get --rev: matching rev ok, mismatch exit 3
    let out = run(&socket, &["get", "/controls/keys/0", "--json"]);
    let cur = stdout_json(&out)["rev"].as_u64().unwrap();
    let out = run(
        &socket,
        &["get", "/controls/keys/0", "--rev", &cur.to_string()],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let out = run(&socket, &["get", "/controls/keys/0", "--rev", "99999"]);
    assert_eq!(code(&out), 3, "{}", stderr(&out));

    // 6. watch --ndjson streams events (--max-events 1) while a set lands
    let child = Command::new(ds_bin())
        .arg("--socket")
        .arg(&socket)
        .args(["watch", "/controls", "--ndjson", "--max-events", "1"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn ds watch");
    std::thread::sleep(Duration::from_millis(500)); // let the subscribe land
    let out = run(&socket, &["set", "/controls/keys/0/brightness", "80"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let out = wait_with_timeout(child, Duration::from_secs(10));
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let line = stdout(&out);
    let ev: Value = serde_json::from_str(line.trim())
        .unwrap_or_else(|e| panic!("watch --ndjson line is not one JSON object: {e}\n{line}"));
    assert_eq!(ev["op"], json!("event"));
    assert_eq!(ev["path"], json!("/controls/keys/0/brightness"));
    assert_eq!(ev["value"], json!(80));

    // 7. watch --json is the contract usage error → exit 1
    let out = run(
        &socket,
        &["watch", "/controls", "--json", "--max-events", "1"],
    );
    assert_eq!(code(&out), 1, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("--json is not valid for streaming verbs; use --ndjson"),
        "{}",
        stderr(&out)
    );

    // 8. schema <name> prints the embedded JSON Schema (no daemon needed)
    let out = run(&socket, &["schema", "state"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let v: Value = serde_json::from_str(stdout(&out).trim()).unwrap();
    assert_eq!(
        v["$id"],
        json!("https://deckstack.dev/schemas/state.json/v1")
    );
    let out = run(&socket, &["schema", "state", "--json", "--compact"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(
        stdout(&out).trim().lines().count(),
        1,
        "--compact is one line"
    );
    let out = run(&socket, &["schema", "nope"]);
    assert_eq!(code(&out), 1, "{}", stderr(&out));

    // 9. describe <path>: schema + current value + allowed verbs
    let out = run(&socket, &["describe", "/controls/keys/0", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let v = stdout_json(&out);
    assert_eq!(v["path"], json!("/controls/keys/0"));
    assert_eq!(v["value"]["label"], json!("Salon"));
    assert_eq!(v["verbs"], json!(["get", "set", "watch", "describe"]));
    assert!(v["schema"].is_object());
    assert!(v["rev"].is_u64());

    // 10. --file: image → {"kind":"file",…} confined to /policy/fs/roots
    let pic = tmp.path().join("pic.png");
    std::fs::write(&pic, b"\x89PNG-fake").unwrap();
    let roots = format!("[\"{}\"]", tmp.path().display());
    let out = run(&socket, &["set", "/policy/fs/roots", "--json", &roots]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let out = run(
        &socket,
        &[
            "set",
            "/controls/keys/1/image",
            "--file",
            pic.to_str().unwrap(),
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let out = run(&socket, &["get", "/controls/keys/1/image", "--json"]);
    let v = stdout_json(&out);
    assert_eq!(v["value"]["kind"], json!("file"));
    assert!(v["value"]["path"].as_str().unwrap().ends_with("pic.png"));
    // a file outside the roots is refused with exit 4 (E_CAP)
    let out = run(
        &socket,
        &["set", "/controls/keys/2/image", "--file", "/etc/hostname"],
    );
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("/policy/fs/roots"),
        "{}",
        stderr(&out)
    );

    // 11. ds migrate exits 1 (no migrate verb exists)
    let out = run(&socket, &["migrate"]);
    assert_eq!(code(&out), 1, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("no 'migrate' verb"),
        "{}",
        stderr(&out)
    );

    // 12. unknown path → exit 2 with an actionable message (contract §5 shape)
    let out = run(&socket, &["get", "/controls/keys/99"]);
    assert_eq!(code(&out), 2, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("path not found: /controls/keys/99"),
        "{}",
        stderr(&out)
    );
    assert!(stderr(&out).contains("[exit 2]"), "{}", stderr(&out));
    let out = run(&socket, &["get", "/nope"]);
    assert_eq!(code(&out), 2, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("unknown top-level path '/nope'"),
        "{}",
        stderr(&out)
    );

    // 13. daemon status (op + socket/pid state) and status (adds the client name)
    let out = Command::new(ds_bin())
        .arg("--socket")
        .arg(&socket)
        .arg("--data-dir")
        .arg(&data_dir)
        .args(["daemon", "status", "--json"])
        .output()
        .unwrap();
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let v = stdout_json(&out);
    assert_eq!(v["running"], json!(true));
    assert_eq!(v["device"], json!(device));
    assert!(v["pid"].is_u64());
    assert!(v["socket"].as_str().unwrap().ends_with("daemon.sock"));
    let out = run(&socket, &["status", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(stdout_json(&out)["client"]
        .as_str()
        .unwrap()
        .starts_with("ds/"));

    // 14. --help at every level through the real binary, plus --version
    for args in [
        vec!["--help"],
        vec!["get", "--help"],
        vec!["set", "--help"],
        vec!["watch", "--help"],
        vec!["schema", "--help"],
        vec!["describe", "--help"],
        vec!["status", "--help"],
        vec!["daemon", "--help"],
        vec!["daemon", "start", "--help"],
        vec!["daemon", "stop", "--help"],
        vec!["daemon", "status", "--help"],
    ] {
        let out = Command::new(ds_bin()).args(&args).output().unwrap();
        assert_eq!(code(&out), 0, "{args:?}");
        assert!(
            stdout(&out).contains("Usage:"),
            "{args:?} help has no Usage"
        );
    }
    let out = Command::new(ds_bin()).arg("--version").output().unwrap();
    assert_eq!(code(&out), 0);
    assert!(stdout(&out).starts_with("ds "), "{}", stdout(&out));

    // 15. daemon down → exit 6 with the `ds daemon start` hint
    let missing = tmp.path().join("missing");
    let out = Command::new(ds_bin())
        .args([
            "--socket",
            missing.join("daemon.sock").to_str().unwrap(),
            "get",
            "/x",
        ])
        .output()
        .unwrap();
    assert_eq!(code(&out), 6, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("daemon unreachable at"),
        "{}",
        stderr(&out)
    );
    assert!(stderr(&out).contains("ds daemon start"), "{}", stderr(&out));
    let out = Command::new(ds_bin())
        .args([
            "--socket",
            missing.join("daemon.sock").to_str().unwrap(),
            "--data-dir",
            missing.to_str().unwrap(),
            "daemon",
            "status",
        ])
        .output()
        .unwrap();
    assert_eq!(code(&out), 6, "{}", stderr(&out));

    // 16. daemon stop (the daemon/stop op) shuts the in-process daemon down cleanly
    let out = run(&socket, &["daemon", "stop"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(stdout(&out).contains("ok stopping"), "{}", stdout(&out));
    let rc = daemon_thread.join().expect("daemon thread panicked");
    assert_eq!(rc.expect("daemon run ok"), 0, "daemon exited 0 on stop");
    vdev.shutdown();
}

/// `ds daemon start` spawns a real `ds-daemon` child (DS_DAEMON_BIN / sibling /
/// PATH resolution) — exercised against a second vdev + data dir.
#[test]
fn daemon_start_and_stop_via_the_ds_binary() {
    let Some(bin) = find_daemon_bin() else {
        eprintln!(
            "skip: no ds-daemon binary next to the test build (run cargo build --workspace first)"
        );
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let vdev_sock = tmp.path().join("vdev2.sock");
    let vdev = ds_vdev::VdevServer::start(&vdev_sock, ds_vdev::HubConfig::default()).unwrap();
    let data_dir = tmp.path().join("data2");
    let device = format!("virtual:{}", vdev_sock.display());

    let out = Command::new(ds_bin())
        .env("DS_DAEMON_BIN", &bin)
        .args([
            "daemon",
            "start",
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--device",
            &device,
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let v = stdout_json(&out);
    assert_eq!(v["running"], json!(true), "{v}");
    assert_eq!(v["device"], json!(device));
    assert!(
        data_dir.join("daemon.log").exists(),
        "stderr of the child goes to <data-dir>/daemon.log"
    );

    let sock = data_dir.join("daemon.sock");
    let out = run(&sock, &["daemon", "status", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(stdout_json(&out)["running"], json!(true));

    let out = run(&sock, &["daemon", "stop"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let deadline = Instant::now() + Duration::from_secs(5);
    while sock.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!sock.exists(), "control socket removed after daemon stop");
    vdev.shutdown();
}

/// The ds-daemon binary for `ds daemon start`: $DS_DAEMON_BIN, else the sibling of
/// the test executable's build dir (`target/debug/ds-daemon`).
fn find_daemon_bin() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("DS_DAEMON_BIN") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.parent()?.join("ds-daemon");
    dir.exists().then_some(dir)
}
