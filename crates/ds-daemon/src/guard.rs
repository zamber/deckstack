//! The guard / handback path (DS-024): the daemon owns the hardware while it runs, so on
//! exit — SIGTERM/SIGINT, a normal `daemon stop`, or a crash of a critical task — it must
//! leave the deck in a safe, known state and exit with the documented code.
//!
//! **Why the guard is wired from the crate root (`lib.rs`) and not from `daemon.rs`:** this
//! ticket's `touches` are only `new:crates/ds-daemon/src/guard.rs`,
//! `crates/ds-daemon/src/lib.rs` and `new:tests/hw/`, so `lib.rs` wraps the `daemon` module
//! and routes `ds_daemon::daemon::run` through [`supervise`]. The `--guard` subcommand in
//! `main.rs` stays the documented stub: the supervisor is **in-process** (one supervisor
//! thread watching the run loop + a panic hook), which is the honest choice for a
//! single-process daemon — a separate supervisor process would need its own device handle
//! and could not see thread panics. `ds-daemon --guard` therefore still logs the DS-024
//! pointer and exits 0.
//!
//! # Shutdown sequence (ordered, idempotent, bounded)
//!
//! 1. **commit state** — the state task's atomic persist (`state.json` is written first and
//!    never after; DS-011). The guard never writes state itself (single-writer invariant);
//!    it only verifies the committed document is on disk and valid.
//! 2. **drain the frame service / stop the writers** — the run loop joins the device task,
//!    which finishes the in-flight frame whole (never dropped mid-send, S16) and stops
//!    accepting pushes.
//! 3. **handback** — on a fresh transport connection, send the device's handback reports
//!    built **only** through the `ds-protocol` safe builders: `leave_software_mode` for
//!    keys and dial (`03 3A 00` / `03 49 00`), which returns the modules to firmware
//!    control. No backlight command is ever sent (backlight-0 wedges the screens and is
//!    unrepresentable via the builders anyway; [`handback_report_is_safe`] double-checks
//!    every report: never forbidden, never backlight-0). No module is left mid-transfer:
//!    on the clean path the device task is already joined, and each handback report is a
//!    single atomic report.
//! 4. **close** the transport.
//!
//! The whole handback is bounded by a watchdog ([`HANDBACK_WATCHDOG`]): a hung shutdown is
//! force-exited with a log line, never left hanging.
//!
//! # Crash safety
//!
//! A panic in a worker thread must not leave the process alive-but-deaf: [`install_panic_hook`]
//! records every panic and reports panics on daemon threads (`ds-*`) to the supervisor,
//! which performs the same handback and exits nonzero ([`CRASH_EXIT_CODE`], `E_INTERNAL`
//! per `docs/contracts/ipc.md` §4) with a clear log line. The pid file is deliberately left
//! behind on the crash path — [`detect_stale_run`] turns it into the recovery signal: the
//! next start logs the stale run, re-handshakes on connect (DS-022) and reloads the
//! atomically committed state document (DS-011), i.e. re-pushes on the next change.
//!
//! The tests under `tests/hw/` are compiled into this crate's test binary via `#[path]`
//! from `src/lib.rs` (no `Cargo.toml` is inside this ticket's touches, so no new `[[test]]`
//! target can be registered there).

use crate::lifecycle::{self};
use ds_device::transport::Transport;
use ds_device::uri::DeviceUri;
use ds_protocol::{ModuleKind, Report};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Log target of every guard line (`{"target":"ds-daemon::guard",…}`).
pub const GUARD_TARGET: &str = "ds-daemon::guard";

/// Watchdog bound for the whole device handback: a hung shutdown is force-exited after this
/// with a log line (ticket DS-024: "bounded in time — do not hang shutdown").
pub const HANDBACK_WATCHDOG: Duration = Duration::from_secs(10);

/// Write timeout of the guard's own transport connection (each handback report is a single
/// report, so this only bounds a wedged device).
pub const HANDBACK_SEND_TIMEOUT: Duration = Duration::from_secs(2);

/// Exit code of the crash path: `E_INTERNAL` ("bug") — `docs/contracts/ipc.md` §4 maps it to
/// CLI exit 8, and `main.rs` maps a fatal run error to 8 as well.
pub const CRASH_EXIT_CODE: i32 = 8;

/// The supervisor's poll interval.
const SUPERVISOR_POLL: Duration = Duration::from_millis(50);

/// Daemon worker threads are named `ds-*` (`ds-run`, `ds-state`, `ds-device`, `ds-ipc-*`,
/// `ds-guard-*`, …); a panic on one of them is critical (alive-but-deaf), a panic anywhere
/// else is only recorded.
pub const CRITICAL_THREAD_PREFIX: &str = "ds-";

/// The modules with a screen that must be handed back to firmware control (software mode
/// off). The M0 family has no software mode and needs no handback.
pub const SCREEN_MODULES: [ModuleKind; 2] = [ModuleKind::Keys, ModuleKind::Dial];

/// One step of the shutdown sequence, in execution order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardStep {
    /// Commit the durable state document (first, never after).
    CommitState,
    /// Drain the frame service and stop the writer/device threads.
    DrainWorkers,
    /// Send the device handback sequence (safe builders only).
    Handback,
    /// Close the transport.
    Close,
}

impl GuardStep {
    /// Stable name for logs and tests.
    pub fn name(self) -> &'static str {
        match self {
            GuardStep::CommitState => "commit_state",
            GuardStep::DrainWorkers => "drain_workers",
            GuardStep::Handback => "handback",
            GuardStep::Close => "close",
        }
    }
}

/// Result of running the shutdown sequence: the steps that ran, in order, and the errors
/// collected (a failing step never skips the device-safety steps that follow).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownOutcome {
    /// The steps that ran, in order.
    pub steps: Vec<GuardStep>,
    /// Non-fatal errors collected while running the steps.
    pub errors: Vec<String>,
}

/// The handback reports for one module, built only through the `ds-protocol` safe builders:
/// `leave_software_mode` (keys/dial only — `None`/empty for the M0 family).
pub fn handback_reports(kind: ModuleKind) -> Vec<Report> {
    ds_protocol::leave_software_mode(kind).into_iter().collect()
}

/// Defense in depth over the raw bytes: a handback report must never be a forbidden command
/// and never a backlight-0 (backlight is the feature report `03 34/44 <pct>` on keys/dial;
/// `0` wedges the screens — AGENTS.md invariant, `upstream/PROTOCOL.md:61`).
pub fn handback_bytes_are_safe(bytes: &[u8]) -> bool {
    if ds_protocol::is_forbidden(bytes) {
        return false;
    }
    if bytes.len() >= 3
        && bytes[0] == 0x03
        && (bytes[1] == 0x34 || bytes[1] == 0x44)
        && bytes[2] == 0
    {
        return false;
    }
    true
}

/// Whether one handback report may leave the host.
pub fn handback_report_is_safe(report: &Report) -> bool {
    handback_bytes_are_safe(report.bytes())
}

/// The ordered shutdown sequence, recording every step. Used by the supervisor (steps 1–2
/// are performed by the run loop itself, which the guard verifies) and by the unit tests
/// with a fake transport.
#[derive(Debug, Default)]
pub struct ShutdownSequence {
    steps: Vec<GuardStep>,
}

impl ShutdownSequence {
    /// An empty sequence.
    pub fn new() -> Self {
        Self { steps: Vec::new() }
    }

    /// The steps recorded so far, in order.
    pub fn steps(&self) -> &[GuardStep] {
        &self.steps
    }

    fn record(&mut self, step: GuardStep) {
        lifecycle::log(
            "info",
            GUARD_TARGET,
            &format!("shutdown step {}", step.name()),
        );
        self.steps.push(step);
    }

    /// Step 1 — commit the durable state. The caller's `result` comes from the state task
    /// (atomic persist); the guard only records the order.
    pub fn commit_state(&mut self, result: Result<(), String>) -> Result<(), String> {
        self.record(GuardStep::CommitState);
        result
    }

    /// Step 2 — drain the frame service and stop the writers (the device task joined).
    pub fn drain_workers(&mut self, result: Result<(), String>) -> Result<(), String> {
        self.record(GuardStep::DrainWorkers);
        result
    }

    /// Step 3 — handback on `t`: send every safe handback report of every screen module.
    /// Every report goes through the ds-protocol builders and [`handback_report_is_safe`].
    /// The transport owns module identity: the handback goes to the id of the connected
    /// module of that kind (hid: -> the enumerated serial, virtual: -> the virtual id).
    pub fn handback(&mut self, t: &dyn Transport) -> Result<(), String> {
        self.record(GuardStep::Handback);
        for kind in SCREEN_MODULES {
            for report in handback_reports(kind) {
                if !handback_report_is_safe(&report) {
                    return Err(format!(
                        "refusing an unsafe handback report for {kind:?}: {:02x?}",
                        report.bytes()
                    ));
                }
                let Some(module) = t.module_id(kind) else {
                    lifecycle::log(
                        "info",
                        GUARD_TARGET,
                        &format!("handback: {kind:?} is not connected on this transport; skipping"),
                    );
                    continue;
                };
                let hex: String = report.bytes().iter().map(|b| format!("{b:02x}")).collect();
                t.send(&module, &report)
                    .map_err(|e| format!("handback report {hex} for {kind:?} failed: {e}"))?;
                lifecycle::log(
                    "info",
                    GUARD_TARGET,
                    &format!("handback: sent {hex} to {kind:?} ({module}) — leave software mode"),
                );
            }
        }
        Ok(())
    }

    /// Step 4 — close the transport.
    pub fn close(&mut self, t: &dyn Transport) -> Result<(), String> {
        self.record(GuardStep::Close);
        t.close()
            .map_err(|e| format!("transport close failed: {e}"))
    }
}

/// Run the full shutdown sequence in the canonical order
/// (`commit_state` → `drain_workers` → `handback` → `close`) and return what ran. Every
/// step runs even when an earlier one failed (device safety first); errors are collected,
/// not propagated. `transport` is `None` for `--device none` (nothing to hand back).
pub fn shutdown_sequence(
    commit: impl FnOnce() -> Result<(), String>,
    drain: impl FnOnce() -> Result<(), String>,
    transport: Option<&dyn Transport>,
) -> ShutdownOutcome {
    let mut seq = ShutdownSequence::new();
    let mut errors = Vec::new();
    if let Err(e) = seq.commit_state(commit()) {
        errors.push(e);
    }
    if let Err(e) = seq.drain_workers(drain()) {
        errors.push(e);
    }
    if let Some(t) = transport {
        if let Err(e) = seq.handback(t) {
            errors.push(e);
        }
        if let Err(e) = seq.close(t) {
            errors.push(e);
        }
    }
    ShutdownOutcome {
        steps: seq.steps().to_vec(),
        errors,
    }
}

/// What the supervisor needs to know about the run: the device to hand back and the data
/// dir (for the committed state document and the stale-run pid file).
#[derive(Debug)]
pub struct GuardContext {
    /// The device URI (`None` for `--device none`).
    pub device: Option<DeviceUri>,
    /// The daemon's data dir.
    pub data_dir: PathBuf,
}

/// A panic recorded by the panic hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashRecord {
    /// The thread that panicked.
    pub thread: String,
    /// The panic payload (and location).
    pub message: String,
}

static CRASH_SINKS: Mutex<Vec<Sender<CrashRecord>>> = Mutex::new(Vec::new());
static HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Register `tx` as a crash sink and install the panic hook (once per process; it chains to
/// the previous hook so the default panic message still prints). Every panic is logged;
/// panics on `ds-*` threads are additionally reported to every registered sink, which is
/// what the supervisor ([`supervise`]) acts on.
pub fn install_panic_hook(tx: Sender<CrashRecord>) {
    if let Ok(mut sinks) = CRASH_SINKS.lock() {
        sinks.push(tx);
    }
    if HOOK_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let name = std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_string();
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string payload>".to_string()
        };
        let loc = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let critical = name.starts_with(CRITICAL_THREAD_PREFIX);
        let level = if critical { "error" } else { "warn" };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            lifecycle::log(
                level,
                GUARD_TARGET,
                &format!("panic in thread '{name}' at {loc}: {payload}"),
            );
        }));
        if critical {
            let rec = CrashRecord {
                thread: name,
                message: format!("{payload} ({loc})"),
            };
            if let Ok(sinks) = CRASH_SINKS.lock() {
                for tx in sinks.iter() {
                    let _ = tx.send(rec.clone());
                }
            }
        }
        prev(info);
    }));
}

/// Detect a previous run that crashed without cleanup: a `daemon.pid` whose pid is dead.
/// A clean exit removes the pid file ([`crate::lifecycle::cleanup`]), so a leftover pid
/// file with a dead pid is exactly the crash evidence the next start recovers from.
pub fn detect_stale_run(data_dir: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(data_dir.join("daemon.pid")).ok()?;
    let pid: u32 = text.trim().parse().ok()?;
    if pid == std::process::id() {
        return None;
    }
    // Linux-only alpha: liveness via /proc.
    if Path::new(&format!("/proc/{pid}")).exists() {
        return None;
    }
    Some(pid)
}

/// Open a fresh transport for the handback. `virtual:` connections get the short
/// [`HANDBACK_SEND_TIMEOUT`]; other schemes go through [`ds_device::open`] (the `hid:`
/// scheme lands with DS-021).
pub fn open_handback_transport(uri: &DeviceUri) -> Result<Box<dyn Transport>, String> {
    if uri.scheme() == "virtual" {
        let path = uri
            .virtual_path_buf()
            .ok_or_else(|| format!("bad virtual device URI {uri}"))?;
        let t = ds_device::vtransport::VirtualTransport::connect(&path)
            .map_err(|e| format!("cannot connect to {}: {e}", path.display()))?;
        return Ok(Box::new(t.with_write_timeout(HANDBACK_SEND_TIMEOUT)));
    }
    ds_device::open(uri).map_err(|e| format!("cannot open {uri} for handback: {e}"))
}

/// Perform steps 3–4 (handback + close) on a fresh connection, bounded by a watchdog that
/// force-exits with `exit_code` if the handback hangs. Steps 1–2 were performed by the run
/// loop (state persisted before the tasks stopped, device task joined = writers stopped);
/// the commit step here verifies the committed document is on disk — the guard never
/// writes state itself.
pub fn handback_on_exit(ctx: &GuardContext, exit_code: i32, watchdog: Duration) -> ShutdownOutcome {
    let disarm = Arc::new(AtomicBool::new(false));
    let disarm2 = Arc::clone(&disarm);
    let wd = std::thread::Builder::new()
        .name("ds-guard-watchdog".into())
        .spawn(move || {
            // sleep in slices so a disarmed watchdog can be joined immediately
            let slice = Duration::from_millis(50);
            let mut waited = Duration::ZERO;
            while waited < watchdog && !disarm2.load(Ordering::SeqCst) {
                std::thread::sleep(slice);
                waited += slice;
            }
            if !disarm2.load(Ordering::SeqCst) {
                lifecycle::log(
                    "error",
                    GUARD_TARGET,
                    &format!("watchdog: handback exceeded {watchdog:?} — force-exiting with code {exit_code}"),
                );
                std::process::exit(exit_code);
            }
        })
        .ok();
    let outcome = handback_device(ctx);
    disarm.store(true, Ordering::SeqCst);
    if let Some(w) = wd {
        let _ = w.join();
    }
    outcome
}

fn handback_device(ctx: &GuardContext) -> ShutdownOutcome {
    let Some(uri) = &ctx.device else {
        lifecycle::log(
            "info",
            GUARD_TARGET,
            "no device configured — nothing to hand back",
        );
        return ShutdownOutcome {
            steps: Vec::new(),
            errors: Vec::new(),
        };
    };
    let t = match open_handback_transport(uri) {
        Ok(t) => t,
        Err(e) => {
            lifecycle::log(
                "warn",
                GUARD_TARGET,
                &format!("{e}; the device is left as-is (state is already committed)"),
            );
            return ShutdownOutcome {
                steps: Vec::new(),
                errors: vec![e],
            };
        }
    };
    lifecycle::log(
        "info",
        GUARD_TARGET,
        &format!("handing {uri} back on a fresh connection (state committed, writers stopped)"),
    );
    let data_dir = ctx.data_dir.clone();
    let commit = move || -> Result<(), String> {
        let path = data_dir.join("state.json");
        let text = std::fs::read_to_string(&path).map_err(|e| {
            format!(
                "committed state document {} unreadable: {e}",
                path.display()
            )
        })?;
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            format!(
                "committed state document {} is not valid JSON: {e}",
                path.display()
            )
        })?;
        if v.pointer("/meta/rev").is_none() {
            return Err(format!(
                "committed state document {} has no /meta/rev",
                path.display()
            ));
        }
        Ok(())
    };
    let drain = || -> Result<(), String> { Ok(()) };
    let outcome = shutdown_sequence(commit, drain, Some(t.as_ref()));
    for e in &outcome.errors {
        lifecycle::log("error", GUARD_TARGET, &format!("handback error: {e}"));
    }
    lifecycle::log("info", GUARD_TARGET, "device handed back, transport closed");
    outcome
}

/// Supervise one daemon run: run `run` on the `ds-run` thread while this thread watches for
/// a critical-task panic (via [`install_panic_hook`]). On a clean return the device handback
/// runs and the run's exit code is returned (SIGTERM/`daemon stop` → 0). On a critical panic
/// (or a fatal run error) the same handback runs and [`CRASH_EXIT_CODE`] is returned — the
/// process must not stay alive-but-deaf. The pid file is intentionally left behind on the
/// crash path; the next start detects it with [`detect_stale_run`].
pub fn supervise<F>(run: F, ctx: GuardContext) -> i32
where
    F: FnOnce() -> io::Result<i32> + Send + 'static,
{
    if let Some(pid) = detect_stale_run(&ctx.data_dir) {
        lifecycle::log(
            "warn",
            GUARD_TARGET,
            &format!(
                "previous run (pid {pid}) did not exit cleanly (stale daemon.pid) — recovering: \
                 re-handshake on connect, state reloaded from the atomically committed document"
            ),
        );
    }
    let (crash_tx, crash_rx) = std::sync::mpsc::channel::<CrashRecord>();
    install_panic_hook(crash_tx);
    let handle = match std::thread::Builder::new().name("ds-run".into()).spawn(run) {
        Ok(h) => h,
        Err(e) => {
            lifecycle::log(
                "error",
                GUARD_TARGET,
                &format!("cannot spawn the run loop thread: {e}"),
            );
            return CRASH_EXIT_CODE;
        }
    };
    let mut crash: Option<CrashRecord> = None;
    let mut code: Option<i32> = None;
    loop {
        if let Ok(rec) = crash_rx.try_recv() {
            crash = Some(rec);
            break;
        }
        if handle.is_finished() {
            match handle.join() {
                Ok(Ok(c)) => code = Some(c),
                Ok(Err(e)) => {
                    crash = Some(CrashRecord {
                        thread: "ds-run".into(),
                        message: format!("fatal: {e}"),
                    })
                }
                Err(_) => {
                    crash = Some(CrashRecord {
                        thread: "ds-run".into(),
                        message: "the run loop panicked".into(),
                    })
                }
            }
            break;
        }
        std::thread::sleep(SUPERVISOR_POLL);
    }
    // A crash record that raced with the clean exit still wins: a dead critical task is a
    // crash even if the run loop happened to return first.
    if crash.is_none() {
        if let Ok(rec) = crash_rx.try_recv() {
            crash = Some(rec);
        }
    }
    match (code, crash) {
        (Some(c), None) => {
            lifecycle::log(
                "info",
                GUARD_TARGET,
                &format!("run loop exited with code {c} (state committed, workers drained) — handing the device back"),
            );
            handback_on_exit(&ctx, c, HANDBACK_WATCHDOG);
            c
        }
        (_, Some(rec)) => {
            lifecycle::log(
                "error",
                GUARD_TARGET,
                &format!(
                    "supervisor: critical task '{}' is dead ({}) — handing the device back and exiting {CRASH_EXIT_CODE}",
                    rec.thread, rec.message
                ),
            );
            handback_on_exit(&ctx, CRASH_EXIT_CODE, HANDBACK_WATCHDOG);
            CRASH_EXIT_CODE
        }
        (None, None) => unreachable!("the loop always sets code or crash"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_device::transport::DeviceEvent;
    use ds_device::vtransport::virtual_module_id;
    use ds_protocol::ModuleKind;
    use mh_device::ModuleId;
    use std::sync::Mutex as StdMutex;

    /// The panic hook is process-global, so tests that drive `supervise` (and deliberately
    /// panic a `ds-*` thread) must not run concurrently: one lock for all of them.
    static SUPERVISE_LOCK: StdMutex<()> = StdMutex::new(());

    /// A fake transport recording every call, for the shutdown-order tests.
    struct FakeTransport {
        ops: StdMutex<Vec<String>>,
    }

    impl FakeTransport {
        fn new() -> Self {
            Self {
                ops: StdMutex::new(Vec::new()),
            }
        }
    }

    impl Transport for FakeTransport {
        fn send(&self, module: &ModuleId, report: &Report) -> ds_device::Result<usize> {
            self.ops.lock().unwrap().push(format!(
                "send {:?} {:02x?}",
                module.kind,
                report.bytes()
            ));
            Ok(report.bytes().len())
        }
        fn query(
            &self,
            _module: &ModuleId,
            _feature_id: u8,
            _timeout: Duration,
        ) -> ds_device::Result<Vec<u8>> {
            Ok(vec![])
        }
        fn recv(&self, _timeout: Duration) -> ds_device::Result<DeviceEvent> {
            Err(ds_device::Error::Timeout(Duration::from_millis(1)))
        }
        fn close(&self) -> ds_device::Result<()> {
            self.ops.lock().unwrap().push("close".to_string());
            Ok(())
        }
        fn module_id(&self, kind: ModuleKind) -> Option<ModuleId> {
            // the fake stands in for a whole deck: every kind resolves to its virtual id
            Some(virtual_module_id(kind))
        }
    }

    #[test]
    fn handback_reports_are_leave_software_mode_only() {
        let keys = handback_reports(ModuleKind::Keys);
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].bytes(), &[0x03, 0x3A, 0x00]);
        let dial = handback_reports(ModuleKind::Dial);
        assert_eq!(dial.len(), 1);
        assert_eq!(dial[0].bytes(), &[0x03, 0x49, 0x00]);
        // the M0 family has no software mode: nothing to send
        for kind in [
            ModuleKind::Base,
            ModuleKind::Faders,
            ModuleKind::Knobs,
            ModuleKind::Rollers,
        ] {
            assert!(handback_reports(kind).is_empty(), "{kind:?}");
        }
        for kind in SCREEN_MODULES {
            for r in handback_reports(kind) {
                assert!(handback_report_is_safe(&r), "{:02x?}", r.bytes());
                assert!(!r.is_forbidden());
            }
        }
    }

    #[test]
    fn handback_safety_rejects_forbidden_and_backlight_zero() {
        // backlight-0 (feature report 03 34/44 00) is never safe; any percent > 0 is not a
        // handback report either but the byte-level rule only bites at 0
        assert!(!handback_bytes_are_safe(&[0x03, 0x34, 0x00]));
        assert!(!handback_bytes_are_safe(&[0x03, 0x44, 0x00]));
        assert!(handback_bytes_are_safe(&[0x03, 0x34, 0x32]));
        // forbidden commands (firmware update / serial rewrite / factory) are never safe
        assert!(!handback_bytes_are_safe(&[0x02, 0x2A, 0x00, 0x00]));
        assert!(!handback_bytes_are_safe(&[0x03, 0x35, b'M', b'H']));
        // the builders refuse backlight 0 outright
        assert!(ds_protocol::backlight(ModuleKind::Keys, 0).is_none());
        assert!(ds_protocol::backlight(ModuleKind::Dial, 0).is_none());
        // and the leave-software-mode reports are safe
        assert!(handback_bytes_are_safe(&[0x03, 0x3A, 0x00]));
        assert!(handback_bytes_are_safe(&[0x03, 0x49, 0x00]));
    }

    #[test]
    fn shutdown_sequence_runs_commit_drain_handback_close_in_order() {
        let fake = FakeTransport::new();
        let order = StdMutex::new(Vec::new());
        let outcome = shutdown_sequence(
            || {
                order.lock().unwrap().push("commit");
                Ok(())
            },
            || {
                order.lock().unwrap().push("drain");
                Ok(())
            },
            Some(&fake),
        );
        assert_eq!(
            order.lock().unwrap().as_slice(),
            &["commit", "drain"],
            "closures run before the device steps"
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
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        let ops = fake.ops.lock().unwrap();
        assert_eq!(
            ops.as_slice(),
            &[
                "send Keys [03, 3a, 00]".to_string(),
                "send Dial [03, 49, 00]".to_string(),
                "close".to_string(),
            ],
            "handback sends leave-software-mode for keys then dial, then closes: {ops:?}"
        );
    }

    #[test]
    fn shutdown_sequence_continues_after_a_failing_commit() {
        let fake = FakeTransport::new();
        let outcome =
            shutdown_sequence(|| Err("persist failed".to_string()), || Ok(()), Some(&fake));
        assert_eq!(
            outcome.steps,
            vec![
                GuardStep::CommitState,
                GuardStep::DrainWorkers,
                GuardStep::Handback,
                GuardStep::Close
            ],
            "device safety steps still run when the commit failed"
        );
        assert_eq!(outcome.errors, vec!["persist failed".to_string()]);
        assert!(fake.ops.lock().unwrap().last().unwrap() == "close");
    }

    #[test]
    fn shutdown_sequence_without_transport_skips_device_steps() {
        let outcome = shutdown_sequence(|| Ok(()), || Ok(()), None);
        assert_eq!(
            outcome.steps,
            vec![GuardStep::CommitState, GuardStep::DrainWorkers]
        );
        assert!(outcome.errors.is_empty());
    }

    #[test]
    fn supervise_returns_the_run_exit_code_and_hands_back_on_a_vdev() {
        let _lock = SUPERVISE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("vdev.sock");
        let _vdev = ds_vdev::VdevServer::start(&sock, ds_vdev::HubConfig::default()).unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        // a committed state document, as the run loop would have persisted
        std::fs::write(
            data_dir.join("state.json"),
            r#"{"rev": 3, "controls": {}, "bindings": [], "plugins": {}}"#,
        )
        .unwrap();
        let ctx = GuardContext {
            device: Some(DeviceUri::parse(&format!("virtual:{}", sock.display())).unwrap()),
            data_dir: data_dir.clone(),
        };
        let code = supervise(|| Ok(0), ctx);
        assert_eq!(code, 0);
        // the handback went out on a fresh connection: leave-software-mode feature writes
        // (len 3, FEATURE_SET kind 0x02) for the keys and dial pids, none refused
        let writes = ds_vdev::ctl::request(&sock, "writes 50").unwrap();
        let leave = |pid: &str| {
            writes.iter().any(|l| {
                l.contains(pid)
                    && l.contains("kind=0x02")
                    && l.contains("len=3")
                    && !l.contains("refused")
            })
        };
        assert!(leave("pid=0x01fe"), "keys leave report missing: {writes:?}");
        assert!(leave("pid=0x0206"), "dial leave report missing: {writes:?}");
        assert!(
            writes.iter().all(|l| !l.contains("refused")),
            "no forbidden/unsafe report may be sent: {writes:?}"
        );
    }

    #[test]
    fn supervise_detects_a_critical_panic_and_exits_nonzero() {
        let _lock = SUPERVISE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(data_dir.join("state.json"), r#"{"rev": 0}"#).unwrap();
        let ctx = GuardContext {
            device: None,
            data_dir,
        };
        let start = std::time::Instant::now();
        let code = supervise(
            || {
                // a critical worker thread panics while the run loop keeps running
                std::thread::Builder::new()
                    .name("ds-state".into())
                    .spawn(|| panic!("state task died"))
                    .unwrap();
                std::thread::sleep(Duration::from_secs(30));
                Ok(0)
            },
            ctx,
        );
        assert_eq!(
            code, CRASH_EXIT_CODE,
            "a dead critical task must exit nonzero"
        );
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the supervisor must not wait for the alive-but-deaf run loop"
        );
    }

    #[test]
    fn supervise_treats_a_fatal_run_error_as_a_crash() {
        let _lock = SUPERVISE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let ctx = GuardContext {
            device: None,
            data_dir,
        };
        let code = supervise(|| Err(io::Error::new(io::ErrorKind::Other, "boom")), ctx);
        assert_eq!(code, CRASH_EXIT_CODE);
    }

    #[test]
    fn detect_stale_run_finds_a_dead_pid_only() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("data");
        std::fs::create_dir_all(&dir).unwrap();
        // no pid file -> nothing
        assert_eq!(detect_stale_run(&dir), None);
        // our own pid -> not stale
        std::fs::write(dir.join("daemon.pid"), format!("{}\n", std::process::id())).unwrap();
        assert_eq!(detect_stale_run(&dir), None);
        // a dead pid (a child that already exited) -> stale crash evidence
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        std::fs::write(dir.join("daemon.pid"), format!("{dead}\n")).unwrap();
        assert_eq!(detect_stale_run(&dir), Some(dead));
        // a live pid that is not us -> another instance, not a crash
        std::fs::write(dir.join("daemon.pid"), "1\n").unwrap();
        assert_eq!(detect_stale_run(&dir), None);
    }
}
