//! `ds-focus` — the window/focus sender (ticket DS-060, `docs/components/focus-sender.md`).
//!
//! R4: the watcher is an ordinary external program; the daemon contains no watcher. This
//! binary polls the session's focused window, coalesces focus churn for `--settle`
//! (default 300 ms), and emits ONE `app/focus/changed` event per settled change on the
//! daemon's control socket via `ds-client`. It never writes `/focus` directly — the
//! daemon mirrors the payload (S7, DS-044).
//!
//! Modes:
//! - default: autodetect a backend and watch forever (SIGTERM/SIGINT exit cleanly);
//! - `--once`: emit the current focus once and exit;
//! - `--inject <json|file.ndjson>`: test hook, bypasses the backend entirely.
//!
//! Exit codes: 0 ok · 1 usage · 3 no usable backend (message lists the available ones) ·
//! 6 E_UNREACHABLE after 60 s of backoff · other client errors keep their IPC exit code.

mod args;
mod backend;
mod focus;
mod log;
mod sender;

use args::{Command, Config};
use backend::Backend;
use focus::Settler;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// The shutdown flag, shared with the signal handlers (SIGTERM/SIGINT → clean exit 0).
fn shutdown_flag() -> Arc<AtomicBool> {
    static FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();
    FLAG.get_or_init(|| Arc::new(AtomicBool::new(false)))
        .clone()
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match args::parse(&argv) {
        Ok(Command::Help) => {
            print!("{}", args::HELP);
            ExitCode::SUCCESS
        }
        Ok(Command::Version) => {
            println!("ds-focus {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Command::Run(cfg)) => {
            log::set_quiet(cfg.quiet);
            install_signal_handlers();
            ExitCode::from(run(cfg) as u8)
        }
        Err(e) => {
            eprintln!("ds-focus: {}  [exit 1]", e.0);
            eprintln!("try `ds-focus --help`");
            ExitCode::from(1)
        }
    }
}

/// How often the shutdown watchdog re-checks the flag.
const WATCHDOG_POLL: Duration = Duration::from_millis(50);

/// Grace the watchdog gives the cooperative watch loop to exit on its own before it
/// exits the process itself (DS-115).
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

/// SIGTERM/SIGINT → clean exit 0 (`unsafe_code` is denied workspace-wide, so the handler
/// registration goes through signal-hook like in ds-daemon). Besides setting the flag,
/// this spawns the shutdown watchdog (DS-115): the flag alone is only observed between
/// `be.poll()` calls, so a watch loop blocked inside a D-Bus call would never see it.
fn install_signal_handlers() {
    let flag = shutdown_flag();
    for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        match signal_hook::flag::register(sig, Arc::clone(&flag)) {
            Ok(_) => {}
            Err(e) => log::warn(
                "ds-focus",
                &format!("cannot register signal {sig} handler: {e} (default action applies)"),
            ),
        }
    }
    spawn_shutdown_watchdog(Arc::clone(&flag));
}

/// DS-115: watchdog thread that exits the process once the shutdown flag is set, even
/// when the main thread is blocked inside a D-Bus call (`zbus` `Proxy::call`) and can
/// never observe the flag itself.
fn spawn_shutdown_watchdog(flag: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("ds-focus-shutdown-watchdog".to_string())
        .spawn(move || watchdog_loop(flag, SHUTDOWN_GRACE, || std::process::exit(0)))
        .expect("cannot spawn the shutdown watchdog thread")
}

/// Watchdog body, factored out so tests can substitute the exit action: poll the flag;
/// once it is set, wait `grace` for the cooperative watch loop to exit on its own, then
/// call `exit` (in production `std::process::exit(0)`; the `return` only matters in
/// tests, where the substituted exit action comes back).
fn watchdog_loop(flag: Arc<AtomicBool>, grace: Duration, exit: impl Fn() + Send + 'static) {
    loop {
        if flag.load(Ordering::Relaxed) {
            std::thread::sleep(grace);
            log::info(
                "ds-focus",
                "shutdown watchdog: flag set and main thread still busy; exiting",
            );
            exit();
            return;
        }
        std::thread::sleep(WATCHDOG_POLL);
    }
}

fn run(cfg: Config) -> i32 {
    let socket = sender::socket_path(&cfg);
    let token_file = sender::token_file(&cfg);
    let mut snd = sender::Sender::new(socket.clone(), token_file);

    // optional config from /settings/senders/focus (CLI flags win; see Config::resolve)
    let store = snd.sender_config();
    let (interval, settle) = cfg.resolve(store.as_ref());

    // --inject: test hook, no backend needed. Payloads are emitted verbatim.
    if let Some(inject) = &cfg.inject {
        return match focus::parse_inject(inject) {
            Ok(payloads) => {
                let mut last: Option<serde_json::Value> = None;
                for p in &payloads {
                    if last.as_ref() == Some(p) {
                        log::info(
                            "ds-focus",
                            &format!(
                                "inject {} deduped (identical to the last emitted payload)",
                                serde_json::to_string(p).unwrap_or_default()
                            ),
                        );
                        continue;
                    }
                    log::info(
                        "ds-focus",
                        &format!(
                            "inject {} -> emit {}",
                            serde_json::to_string(p).unwrap_or_default(),
                            focus::TOPIC
                        ),
                    );
                    match snd.emit(p) {
                        Ok(_) => last = Some(p.clone()),
                        Err(e) => {
                            log::error("ds-focus", &format!("{e}"));
                            eprintln!("ds-focus: {e}  [exit {}]", e.exit_code());
                            return e.exit_code();
                        }
                    }
                }
                log::info(
                    "ds-focus",
                    &format!("inject done: {} payload(s) processed", payloads.len()),
                );
                0
            }
            Err(e) => {
                eprintln!("ds-focus: --inject: {e}  [exit 1]");
                1
            }
        };
    }

    // live backends
    let mut be = match backend::select(cfg.backend) {
        Ok(b) => b,
        Err(e) => {
            log::error("ds-focus", &e.message());
            eprintln!("ds-focus: {}  [exit 3]", e.message());
            return 3;
        }
    };
    log::info(
        "ds-focus",
        &format!(
            "backend {} selected ({}); interval={}ms settle={}ms socket={}",
            be.name(),
            be.detail(),
            interval.as_millis(),
            settle.as_millis(),
            socket.display()
        ),
    );

    let mut settler = Settler::new(settle);

    if cfg.once {
        return match be.poll() {
            Ok(Some(f)) => emit_once(&mut snd, &mut settler, f),
            Ok(None) => {
                log::warn("ds-focus", "no focused window right now; nothing emitted");
                0
            }
            Err(e) => {
                log::error("ds-focus", &format!("backend poll failed: {e}"));
                eprintln!("ds-focus: backend poll failed: {e}  [exit 3]");
                3
            }
        };
    }

    // watch loop: wake at the earlier of the next poll tick and the settle deadline
    loop {
        if shutdown_flag().load(Ordering::Relaxed) {
            log::info("ds-focus", "shutdown signal received; exiting");
            return 0;
        }
        if let Some(f) = settler.settled(Instant::now()) {
            let code = emit_settled(&mut snd, &mut settler, f);
            if code != 0 {
                return code;
            }
        }
        match be.poll() {
            Ok(Some(f)) => settler.observe(f, Instant::now()),
            Ok(None) => {}
            Err(e) => log::warn("ds-focus", &format!("backend poll failed: {e} (retrying)")),
        }
        let wake = settler.next_wake(Instant::now(), interval);
        sleep_interruptible(wake);
    }
}

/// `--once`: emit the current focus immediately (no settle wait), dedupe only.
fn emit_once(snd: &mut sender::Sender, settler: &mut Settler, f: focus::Focus) -> i32 {
    match settler.immediate(f) {
        Some(f) => emit_settled(snd, settler, f),
        None => {
            log::info(
                "ds-focus",
                "focus unchanged since the last emit; nothing sent",
            );
            0
        }
    }
}

/// Emit one settled focus and log it at info; returns the exit code (0 on success).
fn emit_settled(snd: &mut sender::Sender, _settler: &mut Settler, f: focus::Focus) -> i32 {
    let t0 = Instant::now();
    match snd.emit(&f.payload()) {
        Ok(emit_ms) => {
            log::info(
                "ds-focus",
                &format!(
                    "focus app={} title={} pid={:?} emit_ms={:.1} total_ms={:.1}",
                    f.app,
                    f.title,
                    f.pid,
                    emit_ms.as_secs_f64() * 1000.0,
                    t0.elapsed().as_secs_f64() * 1000.0
                ),
            );
            0
        }
        Err(e) => {
            log::error("ds-focus", &format!("{e}"));
            eprintln!("ds-focus: {e}  [exit {}]", e.exit_code());
            e.exit_code()
        }
    }
}

/// Sleep until `wake`, waking early on the shutdown signal (checked every 50 ms).
fn sleep_interruptible(wake: Instant) {
    loop {
        if shutdown_flag().load(Ordering::Relaxed) {
            return;
        }
        let now = Instant::now();
        if now >= wake {
            return;
        }
        let remaining = wake - now;
        std::thread::sleep(remaining.min(Duration::from_millis(50)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_text_documents_the_contract() {
        assert!(args::HELP.contains("ds-focus"));
        assert!(args::HELP.contains("app/focus/changed"));
        assert!(args::HELP.contains("--inject"));
        assert!(args::HELP.contains("--settle"));
        assert!(args::HELP.contains("3 no usable backend"));
        assert!(args::HELP.contains("6 E_UNREACHABLE"));
    }

    #[test]
    fn sleep_interruptible_returns_at_the_deadline() {
        let t0 = Instant::now();
        sleep_interruptible(t0 + Duration::from_millis(60));
        assert!(t0.elapsed() >= Duration::from_millis(55));
    }

    #[test]
    fn shutdown_flag_stops_the_sleep_early() {
        shutdown_flag().store(true, Ordering::Relaxed);
        let t0 = Instant::now();
        sleep_interruptible(t0 + Duration::from_secs(30));
        assert!(t0.elapsed() < Duration::from_secs(5));
        shutdown_flag().store(false, Ordering::Relaxed);
    }

    /// DS-115: the watchdog must fire while the "main thread" is blocked (here: nobody
    /// clears the flag, mimicking a watch loop stuck inside a D-Bus call). The exit
    /// action is observed over a channel with a timeout instead of calling
    /// `std::process::exit` in the test harness.
    #[test]
    fn watchdog_exits_once_the_flag_is_set_while_blocked() {
        let flag = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let wd = std::thread::spawn({
            let flag = Arc::clone(&flag);
            move || {
                watchdog_loop(flag, Duration::from_millis(20), move || {
                    let _ = tx.send(());
                })
            }
        });
        // let the watchdog reach its poll loop, then simulate the signal arriving while
        // the main thread is blocked: the flag is set and nothing else exits
        std::thread::sleep(Duration::from_millis(100));
        flag.store(true, Ordering::Relaxed);
        rx.recv_timeout(Duration::from_secs(5))
            .expect("watchdog must exit the process once the shutdown flag is set");
        wd.join()
            .expect("watchdog loop returns after the exit action");
    }

    #[test]
    fn watchdog_stays_quiet_while_the_flag_is_clear() {
        let flag = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let wd = std::thread::spawn({
            let flag = Arc::clone(&flag);
            move || {
                watchdog_loop(flag, Duration::from_millis(20), move || {
                    let _ = tx.send(());
                })
            }
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "watchdog must not exit while the shutdown flag is clear"
        );
        flag.store(true, Ordering::Relaxed);
        rx.recv_timeout(Duration::from_secs(5))
            .expect("watchdog fires once the flag is set");
        wd.join()
            .expect("watchdog loop returns after the exit action");
    }
}
