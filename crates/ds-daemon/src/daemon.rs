//! The daemon glue: command-line parsing for the `ds-daemon` binary and the `run` loop
//! that assembles the state task, the IPC server and the device task, then waits for
//! SIGTERM / `daemon stop` and cleans up (docs/components/daemon.md).
//!
//! ```text
//! run --device virtual:<path>|none [--data-dir <dir>]
//!   → data dir (0700) + token (0600) + pid file + state.json
//!   → state task (owns the ds-state Store, single writer)
//!   → IPC server on <dir>/daemon.sock (0600) with the DaemonHandler
//!   → device task (ds-device transport from the URI)
//!   → SIGTERM/SIGINT → persist, stop tasks, remove socket + pid, exit 0
//! ```

use crate::device;
use crate::ipc::{DaemonHandler, StatusInfo};
use crate::lifecycle::{self, RunPaths, Shutdown};
use crate::state::StateTask;
use ds_device::uri::DeviceUri;
use ds_ipc::limits::ServerConfig;
use ds_ipc::server::Server;
use ds_state::Store;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// How often the run loop wakes to check the shutdown flag.
const SHUTDOWN_POLL: Duration = Duration::from_millis(100);

/// Parsed command line of the `run` subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOptions {
    /// Device URI string (`virtual:<path>`), or `none`/absent for no hardware.
    pub device: Option<String>,
    /// Explicit data dir (`--data-dir`).
    pub data_dir: Option<PathBuf>,
}

/// The parsed top-level command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cli {
    /// `run` (the default when no subcommand is given).
    Run(RunOptions),
    /// `--guard` (stub; guard/handback is DS-024).
    Guard,
    /// `--version`.
    Version,
    /// `--help`.
    Help,
}

/// Parse argv (without argv[0]). Hand-rolled like the other deckstack binaries.
pub fn parse_args(args: &[String]) -> Result<Cli, String> {
    let mut args = args.iter().peekable();
    // Top-level flags first.
    if args
        .peek()
        .is_some_and(|a| matches!(a.as_str(), "--version" | "-V" | "version"))
    {
        return Ok(Cli::Version);
    }
    if args
        .peek()
        .is_some_and(|a| matches!(a.as_str(), "--help" | "-h" | "help"))
    {
        return Ok(Cli::Help);
    }
    if args.peek().is_some_and(|a| a.as_str() == "--guard") {
        return Ok(Cli::Guard);
    }
    // Default subcommand: run.
    if args.peek().is_some_and(|a| a.as_str() == "run") {
        args.next();
    }
    let mut opts = RunOptions {
        device: None,
        data_dir: None,
    };
    while let Some(arg) = args.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) => (f.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };
        let mut value = |flag: &str| -> Result<String, String> {
            match inline {
                Some(ref v) => Ok(v.clone()),
                None => args
                    .next()
                    .cloned()
                    .ok_or_else(|| format!("{flag} needs a value")),
            }
        };
        match flag.as_str() {
            "--device" => opts.device = Some(value("--device")?),
            "--data-dir" => opts.data_dir = Some(PathBuf::from(value("--data-dir")?)),
            other => return Err(format!("unknown argument '{other}' (try --help)")),
        }
    }
    Ok(Cli::Run(opts))
}

/// The `--help` text.
pub fn usage() -> String {
    format!(
        "{name} {ver} — the deckstack daemon (docs/components/daemon.md)\n\
         \n\
         Usage:\n\
         \x20 ds-daemon [run] [--device <uri|none>] [--data-dir <dir>]   run the daemon (default)\n\
         \x20 ds-daemon --guard        guard/handback supervisor (stub; lands with DS-024)\n\
         \x20 ds-daemon --version      print the version and exit\n\
         \x20 ds-daemon --help         print this text and exit\n\
         \n\
         The data dir is --data-dir, else $XDG_RUNTIME_DIR/deckstack/, else\n\
         ~/.local/state/deckstack/ (dir 0700, daemon.sock + token 0600).\n\
         Device URIs: virtual:<unix-socket> (ds-vdev) or none.",
        name = crate::CRATE_NAME,
        ver = crate::version()
    )
}

/// Parse the device option into a URI: `none` (or absent) → `None`.
pub fn device_uri(device: &Option<String>) -> Result<Option<DeviceUri>, String> {
    match device.as_deref() {
        None => Ok(None),
        Some("none") => Ok(None),
        Some(s) => DeviceUri::parse(s)
            .map(Some)
            .map_err(|e| format!("bad --device URI: {e}")),
    }
}

/// Run the daemon until SIGTERM/SIGINT/`daemon stop`. Returns the process exit code.
pub fn run(opts: RunOptions) -> io::Result<i32> {
    let data_dir = lifecycle::resolve_data_dir(opts.data_dir.as_deref());
    let paths = RunPaths::new(&data_dir);
    lifecycle::prepare_data_dir(&data_dir)?;
    let _token = lifecycle::ensure_token(&data_dir)?;
    let _pid_path = lifecycle::write_pid(&data_dir)?;

    let device =
        device_uri(&opts.device).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let device_label = opts.device.clone().unwrap_or_else(|| "none".to_string());

    lifecycle::log(
        "info",
        "ds-daemon",
        &format!(
            "starting: data_dir={} socket={} device={}",
            paths.data_dir.display(),
            paths.socket.display(),
            device_label
        ),
    );

    // State: empty at start on a fresh data dir (ADR-013); a corrupt state.json is
    // quarantined by ds-state and we start empty with a loud log line.
    let store = Store::open(&paths.state)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    if let Some(err) = store.load_error() {
        lifecycle::log(
            "error",
            "ds-daemon",
            &format!(
                "state file was corrupt ({err}); quarantined to {:?}, starting empty",
                store.corrupt_backup()
            ),
        );
    }
    store
        .record_daemon_start(
            std::process::id(),
            &lifecycle::rfc3339_utc(lifecycle::now_secs()),
        )
        .map_err(io::Error::other)?;

    let (shutdown, _signals) = Shutdown::install();

    // Device task first so the state task can forward pushes to it from the start.
    let (device_handle, device_thread, device_status) = device::spawn(device, shutdown.clone());
    let (state, state_thread) = StateTask::spawn(store, Some(device_handle.sender()));

    // IPC control server on <dir>/daemon.sock (0600; dir already 0700).
    let status = StatusInfo {
        pid: std::process::id(),
        device: device_label.clone(),
        started_ms: lifecycle::now_ms(),
        device_status: device_status.clone(),
    };
    let handler = Arc::new(DaemonHandler::new(
        state.clone(),
        shutdown.clone(),
        status,
        crate::server_name(),
    ));
    let config = ServerConfig {
        server_name: crate::server_name(),
        ..ServerConfig::default()
    };
    let server = Server::bind(&paths.socket, &data_dir, config)?;
    let _accept_thread = server.start(handler)?;
    let rev = state.rev().unwrap_or(0);
    lifecycle::log(
        "info",
        "ds-daemon",
        &format!(
            "listening on {} (rev={rev}, server={})",
            paths.socket.display(),
            crate::server_name()
        ),
    );

    // Wait for SIGTERM/SIGINT or the `daemon stop` op.
    while !shutdown.get() {
        lifecycle::poll_sleep(&shutdown, SHUTDOWN_POLL);
    }

    lifecycle::log("info", "ds-daemon", "shutdown requested; draining");
    if let Err(e) = state.persist() {
        lifecycle::log("error", "ds-daemon", &format!("final persist failed: {e}"));
    }
    state.stop();
    let _ = state_thread.join();
    let _ = device_thread.join();
    // The IPC accept loop cannot be unblocked from outside yet; process exit ends it.
    lifecycle::cleanup(&paths);
    lifecycle::log("info", "ds-daemon", "stopped cleanly (exit 0)");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_default_is_run() {
        assert_eq!(
            parse_args(&args(&[])).unwrap(),
            Cli::Run(RunOptions {
                device: None,
                data_dir: None
            })
        );
        assert_eq!(
            parse_args(&args(&["run"])).unwrap(),
            Cli::Run(RunOptions {
                device: None,
                data_dir: None
            })
        );
    }

    #[test]
    fn parse_run_options() {
        let c = parse_args(&args(&[
            "run",
            "--device",
            "virtual:/tmp/vdev.sock",
            "--data-dir",
            "/tmp/ds-data",
        ]))
        .unwrap();
        assert_eq!(
            c,
            Cli::Run(RunOptions {
                device: Some("virtual:/tmp/vdev.sock".into()),
                data_dir: Some(PathBuf::from("/tmp/ds-data")),
            })
        );
        // --flag=value form
        let c = parse_args(&args(&["--device=none", "--data-dir=/tmp/x"])).unwrap();
        assert_eq!(
            c,
            Cli::Run(RunOptions {
                device: Some("none".into()),
                data_dir: Some(PathBuf::from("/tmp/x")),
            })
        );
        // flags
        assert_eq!(parse_args(&args(&["--version"])).unwrap(), Cli::Version);
        assert_eq!(parse_args(&args(&["-V"])).unwrap(), Cli::Version);
        assert_eq!(parse_args(&args(&["--guard"])).unwrap(), Cli::Guard);
        assert_eq!(parse_args(&args(&["--help"])).unwrap(), Cli::Help);
        // errors
        assert!(parse_args(&args(&["--bogus"])).is_err());
        assert!(parse_args(&args(&["run", "--device"])).is_err());
    }

    #[test]
    fn device_uri_parsing() {
        assert_eq!(device_uri(&None).unwrap(), None);
        assert_eq!(device_uri(&Some("none".into())).unwrap(), None);
        let u = device_uri(&Some("virtual:/tmp/v.sock".into()))
            .unwrap()
            .unwrap();
        assert_eq!(u.to_string(), "virtual:/tmp/v.sock");
        assert!(device_uri(&Some("bogus".into())).is_err());
    }

    #[test]
    fn usage_mentions_the_subcommands() {
        let u = usage();
        assert!(u.contains("run"));
        assert!(u.contains("--guard"));
        assert!(u.contains("--version"));
        assert!(u.contains("--device"));
        assert!(u.contains("--data-dir"));
    }
}
