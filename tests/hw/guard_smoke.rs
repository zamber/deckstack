//! DS-024 hardware smoke: the SIGTERM handback against the real deck.
//!
//! Run on a machine with the deck attached:
//!
//! ```sh
//! DS_HARDWARE=1 cargo test -p ds-daemon hw_smoke -- --nocapture
//! ```
//!
//! Without `DS_HARDWARE=1` (CI) the test skips immediately and passes, so
//! `cargo test --workspace` stays hardware-free — same convention as `tests/hw/hid.rs`
//! (DS-021). With `DS_HARDWARE=1` but no deck attached (or no permission to open it — the
//! udev rule of DS-025) every step prints a clear SKIP and the test still passes: hardware
//! absence is a state to detect, not a failure.
//!
//! Two parts:
//!
//! - **daemon level** — start the real `ds-daemon` binary against the deck (`--device
//!   hid:`) on a temp data dir, SIGTERM it and assert it exits 0 within a bounded time and
//!   never holds a `/dev/hidraw` fd. (`hid:` URIs are refused by `ds_device::open` until
//!   DS-021 lands the HID transport, so in this tree the daemon logs the attach error and
//!   runs without hardware; the exit-path assertions hold either way and turn into a full
//!   attach + handback check once DS-021 lands.)
//! - **transport level** — open the real deck with `HidTransport`, run the read-only
//!   handshake path on the screen modules (software-mode handshake + confirming feature
//!   read, no image pushes), then run the guard's shutdown sequence over that live
//!   transport and assert no hidraw handle stays open in this process afterwards.
//!
//! Compiled into the `ds-daemon` library test binary via `#[path]` from
//! `crates/ds-daemon/src/lib.rs` (a `[[test]]` registration would need a `Cargo.toml`
//! edit, which is outside this ticket's `touches`).

use ds_daemon::guard::{self, GuardStep};
use ds_device::handshake::{self, HandshakeConfig};
use ds_device::hid::{self, HidTransport};
use ds_protocol::ModuleKind;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn hw_enabled() -> bool {
    std::env::var("DS_HARDWARE").as_deref() == Ok("1")
}

/// Path of the daemon binary built by cargo for this package's tests.
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

/// The open `/dev/hidraw*` fds of a process (empty when the process is gone or holds none).
fn hidraw_fds_of(pid: u32) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(format!("/proc/{pid}/fd")) {
        for e in rd.flatten() {
            if let Ok(target) = fs::read_link(e.path()) {
                let t = target.to_string_lossy();
                if t.contains("/dev/hidraw") {
                    out.push(t.into_owned());
                }
            }
        }
    }
    out
}

#[test]
fn hw_smoke_sigterm_handback() {
    if !hw_enabled() {
        eprintln!("SKIP: DS_HARDWARE is not 1 — hardware smoke disabled (CI stays hardware-free)");
        return;
    }
    let modules = match hid::enumerate() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("SKIP: enumeration failed: {e}");
            return;
        }
    };
    if modules.is_empty() {
        eprintln!("SKIP: no MasterHub module (VID 0x2516) enumerated — no deck attached");
        return;
    }
    println!("enumerated {} MasterHub module(s)", modules.len());

    // --- part A: the daemon binary against the deck, SIGTERM → exit 0, no hidraw leak ---
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let stderr_log = data_dir.join("daemon.stderr.log");
    let stderr = fs::File::create(&stderr_log).unwrap();
    let mut child = Command::new(daemon_bin())
        .args([
            "run",
            "--device",
            "hid:",
            "--data-dir",
            &data_dir.display().to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn ds-daemon");
    let socket = data_dir.join("daemon.sock");
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
            break;
        }
        if let Some(s) = child.try_wait().unwrap() {
            panic!(
                "ds-daemon exited early with {s}; stderr:\n{}",
                fs::read_to_string(&stderr_log).unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(socket.exists(), "daemon did not bind its control socket");
    let pid = child.id();
    let while_running = hidraw_fds_of(pid);
    println!("daemon pid {pid} holds hidraw fds while running: {while_running:?}");
    let status = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("kill -TERM");
    assert!(status.success());
    let start = Instant::now();
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            assert_eq!(
                s.code(),
                Some(0),
                "SIGTERM against the real deck must exit 0; stderr:\n{}",
                fs::read_to_string(&stderr_log).unwrap_or_default()
            );
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "daemon did not exit within 10 s after SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        hidraw_fds_of(pid).is_empty(),
        "no hidraw handle may outlive the daemon"
    );
    let log = fs::read_to_string(&stderr_log).unwrap_or_default();
    assert!(
        log.contains(guard::GUARD_TARGET),
        "the guard logged its handback path:\n{log}"
    );

    // --- part B: the guard's handback sequence on the live deck transport ---
    let t = match HidTransport::open_all() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("SKIP: cannot open the deck: {e} (permissions — the DS-025 udev rule or root); enumeration above still proves the transport sees the modules");
            return;
        }
    };
    // read-only handshake path on the screen modules: software-mode handshake + the
    // confirming feature read, no image pushes
    for kind in [ModuleKind::Keys, ModuleKind::Dial] {
        if let Some(id) = t.modules().into_iter().find(|m| m.kind == kind) {
            match handshake::pending(&t, id, HandshakeConfig::default()).handshake() {
                Ok(m) => println!("{}: handshake ok, firmware {:?}", kind.name(), m.firmware()),
                Err(e) => println!(
                    "{}: handshake failed (unresponsive module): {e}",
                    kind.name()
                ),
            }
        }
    }
    // the guard sequence over the live transport: commit + drain are no-ops here (no state
    // task in this test process), handback sends leave-software-mode, close releases the
    // device
    let outcome = guard::shutdown_sequence(|| Ok(()), || Ok(()), Some(&t));
    assert!(
        outcome.errors.is_empty(),
        "handback on the real deck must not error: {:?}",
        outcome.errors
    );
    assert_eq!(
        outcome.steps,
        vec![
            GuardStep::CommitState,
            GuardStep::DrainWorkers,
            GuardStep::Handback,
            GuardStep::Close
        ]
    );
    drop(t);
    let open = hidraw_fds_of(std::process::id());
    assert!(
        open.is_empty(),
        "no hidraw handle may stay open after handback + close: {open:?}"
    );
    println!("hw smoke: SIGTERM handback path green");
}
