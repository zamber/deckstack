//! Lifecycle glue: data-dir resolution and permissions, the install token, the pid file,
//! structured logs to stderr, SIGTERM/SIGINT handling, and socket/pid cleanup on exit
//! (docs/components/daemon.md, docs/contracts/ipc.md §1.1).
//!
//! Layout under the data dir (`--data-dir`, else `$XDG_RUNTIME_DIR/deckstack/`, else
//! `~/.local/state/deckstack/` — the `ds-ipc` default):
//!
//! ```text
//! <dir>/            0700
//! <dir>/daemon.sock 0600  control socket (bound by ds-ipc's Server)
//! <dir>/token       0600  install token (admin scope), random at first start
//! <dir>/tokens.json 0600  scoped tokens
//! <dir>/state.json        durable state document (ds-state)
//! <dir>/daemon.pid  0600  pid of the running daemon, removed on clean exit
//! ```

use serde_json::json;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use signal_hook::consts::{SIGINT, SIGTERM};

/// All filesystem locations of one daemon instance.
#[derive(Debug, Clone)]
pub struct RunPaths {
    /// The data dir itself (0700).
    pub data_dir: PathBuf,
    /// The NDJSON control socket (0600).
    pub socket: PathBuf,
    /// The durable state document.
    pub state: PathBuf,
    /// The pid file (0600), removed on clean exit.
    pub pid: PathBuf,
    /// The install token file (0600, admin scope).
    pub token: PathBuf,
    /// The scoped-token file (0600).
    pub tokens: PathBuf,
}

impl RunPaths {
    /// The paths for a data dir.
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        let data_dir = data_dir.as_ref().to_path_buf();
        Self {
            socket: data_dir.join("daemon.sock"),
            state: data_dir.join("state.json"),
            pid: data_dir.join("daemon.pid"),
            token: data_dir.join("token"),
            tokens: data_dir.join("tokens.json"),
            data_dir,
        }
    }
}

/// Resolve the data dir: explicit `--data-dir` wins, else the `ds-ipc` default
/// (`$XDG_RUNTIME_DIR/deckstack/`, fallback `~/.local/state/deckstack/`).
pub fn resolve_data_dir(explicit: Option<&Path>) -> PathBuf {
    match explicit {
        Some(d) => d.to_path_buf(),
        None => ds_ipc::paths::default_dir(),
    }
}

/// Create the data dir if needed and force it to 0700 (also when it already existed).
pub fn prepare_data_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// The install token: read `<dir>/token` when it exists, else generate a random one and
/// write it (plus an empty `tokens.json`) via `ds-ipc`'s `TokenSet::create` (both 0600).
/// Returns the token (never logged in full by callers).
pub fn ensure_token(dir: &Path) -> io::Result<String> {
    let token_path = dir.join("token");
    if token_path.exists() {
        let existing = fs::read_to_string(&token_path)?.trim().to_string();
        if existing.len() >= 16 {
            return Ok(existing);
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{}: install token shorter than 16 chars",
                token_path.display()
            ),
        ));
    }
    let token = ds_ipc::auth::generate_token()?;
    ds_ipc::auth::TokenSet::create(dir, &token)?;
    Ok(token)
}

/// Write `<dir>/daemon.pid` (0600) with the current pid.
pub fn write_pid(dir: &Path) -> io::Result<PathBuf> {
    let path = dir.join("daemon.pid");
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    f.write_all(format!("{}\n", std::process::id()).as_bytes())?;
    // Belt and braces: the mode only applies at creation.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok(path)
}

/// Remove the control socket and the pid file on clean exit (best effort).
pub fn cleanup(paths: &RunPaths) {
    for p in [&paths.socket, &paths.pid] {
        if p.exists() {
            let _ = fs::remove_file(p);
        }
    }
}

/// One structured log line to stderr: `{"ts_ms":…,"level":…,"target":…,"msg":…}`.
pub fn log(level: &str, target: &str, msg: &str) {
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let line = json!({
        "ts_ms": ts_ms,
        "level": level,
        "target": target,
        "msg": msg,
    });
    match serde_json::to_string(&line) {
        Ok(s) => eprintln!("{s}"),
        Err(_) => eprintln!("{{\"level\":\"{level}\",\"target\":\"{target}\",\"msg\":\"{msg}\"}}"),
    }
}

/// Milliseconds since the Unix epoch (for status/uptime fields).
pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Seconds since the Unix epoch.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Format unix seconds as an RFC 3339 UTC `date-time` (`2026-11-22T03:04:05Z`) — the
/// format `schemas/state.json` requires for `/meta/started_at`. Civil-from-days
/// (Howard Hinnant's algorithm), no chrono dependency.
pub fn rfc3339_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let secs_of_day = secs % 86_400;
    let (hh, mm, ss) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// The shutdown flag: set by the SIGTERM/SIGINT handler (or by the `daemon stop` op), read
/// by the run loop. `Clone` shares one flag across all tasks.
#[derive(Debug, Clone)]
pub struct Shutdown {
    flag: Arc<AtomicBool>,
}

impl Shutdown {
    /// Install the SIGTERM + SIGINT handlers. The returned [`SignalRegistration`] must be
    /// kept alive for as long as the handlers should stay installed.
    pub fn install() -> (Self, SignalRegistration) {
        let flag = Arc::new(AtomicBool::new(false));
        let mut ids = Vec::new();
        for sig in [SIGTERM, SIGINT] {
            match signal_hook::flag::register(sig, Arc::clone(&flag)) {
                Ok(id) => ids.push(id),
                Err(e) => log(
                    "error",
                    "ds-daemon::lifecycle",
                    &format!("cannot install handler for signal {sig}: {e}"),
                ),
            }
        }
        (Self { flag }, SignalRegistration(ids))
    }

    /// A shutdown flag that no signal handler is attached to (tests).
    pub fn manual() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Whether shutdown was requested.
    pub fn get(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Request shutdown (used by the `daemon stop` op).
    pub fn trigger(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }
}

/// Keeps the signal-hook registrations alive (dropping a `SigId` unregisters it).
#[derive(Debug)]
#[allow(dead_code)] // the field exists only to keep the registrations alive
pub struct SignalRegistration(Vec<signal_hook::SigId>);

/// Sleep in small slices so the run loop notices a shutdown request quickly.
pub fn poll_sleep(shutdown: &Shutdown, slice: Duration) {
    if !shutdown.get() {
        std::thread::sleep(slice);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_data_dir_wins() {
        let explicit = Path::new("/tmp/ds-explicit");
        assert_eq!(
            resolve_data_dir(Some(explicit)),
            PathBuf::from("/tmp/ds-explicit")
        );
        // No explicit dir -> the ds-ipc default ($XDG_RUNTIME_DIR/deckstack or ~/.local/...).
        let d = resolve_data_dir(None);
        assert!(d.ends_with("deckstack"));
    }

    #[test]
    fn run_paths_layout() {
        let p = RunPaths::new("/tmp/ds-x");
        assert_eq!(p.socket, Path::new("/tmp/ds-x/daemon.sock"));
        assert_eq!(p.state, Path::new("/tmp/ds-x/state.json"));
        assert_eq!(p.pid, Path::new("/tmp/ds-x/daemon.pid"));
        assert_eq!(p.token, Path::new("/tmp/ds-x/token"));
        assert_eq!(p.tokens, Path::new("/tmp/ds-x/tokens.json"));
    }

    #[test]
    fn data_dir_is_0700() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("data");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        prepare_data_dir(&dir).unwrap();
        let mode = fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn token_is_created_0600_and_stable() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("data");
        prepare_data_dir(&dir).unwrap();
        let t1 = ensure_token(&dir).unwrap();
        assert_eq!(t1.len(), 64);
        let t2 = ensure_token(&dir).unwrap();
        assert_eq!(t1, t2, "an existing token must not be regenerated");
        for name in ["token", "tokens.json"] {
            let mode = fs::metadata(dir.join(name)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{name} must be 0600");
        }
        // the token set loads and grants admin
        let set = ds_ipc::auth::TokenSet::load(&dir).unwrap();
        assert_eq!(set.scope_of(&t1), Some(ds_ipc::auth::Scope::Admin));
    }

    #[test]
    fn pid_file_round_trip_and_cleanup() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = RunPaths::new(tmp.path().join("data"));
        prepare_data_dir(&paths.data_dir).unwrap();
        let pid = write_pid(&paths.data_dir).unwrap();
        let mode = fs::metadata(&pid).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let content = fs::read_to_string(&pid).unwrap();
        assert_eq!(content.trim(), std::process::id().to_string());
        // fake a socket file, then clean up
        fs::write(&paths.socket, b"").unwrap();
        cleanup(&paths);
        assert!(!paths.socket.exists());
        assert!(!paths.pid.exists());
    }

    #[test]
    fn rfc3339_utc_formats_date_times() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(1_795_369_994), "2026-11-22T17:53:14Z");
        assert_eq!(rfc3339_utc(951_782_400), "2000-02-29T00:00:00Z"); // leap day
        assert_eq!(rfc3339_utc(4_102_444_800), "2100-01-01T00:00:00Z");
    }

    #[test]
    fn shutdown_flag_and_log_line() {
        let s = Shutdown::manual();
        assert!(!s.get());
        s.trigger();
        assert!(s.get());
        let clone = s.clone();
        assert!(clone.get(), "clones share the flag");
        // log() must not panic and must emit one JSON line on stderr
        log("info", "ds-daemon::test", "hello");
    }
}
