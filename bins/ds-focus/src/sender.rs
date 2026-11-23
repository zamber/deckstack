//! Emitting `app/focus/changed` to ds-daemon over the control socket via `ds-client`
//! (one-shot `ds emit`-equivalent: `hello` + token-file auth + `emit`), with reconnect +
//! backoff when the daemon is unreachable (exit 6 after 60 s, per
//! `docs/components/focus-sender.md`).
//!
//! Every emit is logged at `info` with the measured emit latency (the 500 ms budget is
//! measured from the settled focus change; see `focus.rs`).

use crate::focus::{emit_request, TOPIC};
use ds_client::{Client, ClientError};
use serde_json::Value;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How long an unreachable daemon is retried before giving up with exit 6.
pub const UNREACHABLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Initial backoff on connect failure, doubled up to `MAX_BACKOFF`.
const INITIAL_BACKOFF: Duration = Duration::from_millis(100);
const MAX_BACKOFF: Duration = Duration::from_secs(2);

/// A failure to deliver an event.
#[derive(Debug)]
pub enum SendError {
    /// The daemon stayed unreachable for [`UNREACHABLE_TIMEOUT`] → exit 6 (E_UNREACHABLE).
    UnreachableTimeout { path: PathBuf },
    /// Any other client error (auth, schema, …) → its own exit code.
    Client(ClientError),
}

impl SendError {
    /// The process exit code for this failure.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::UnreachableTimeout { .. } => 6,
            Self::Client(e) => e.exit_code(),
        }
    }
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnreachableTimeout { path } => write!(
                f,
                "daemon unreachable at {} for {:?} — start it with: ds daemon start",
                path.display(),
                UNREACHABLE_TIMEOUT
            ),
            Self::Client(e) => write!(f, "{e}"),
        }
    }
}

/// The daemon client: connects lazily, reconnects on failure, retries unreachable with
/// backoff up to [`UNREACHABLE_TIMEOUT`].
pub struct Sender {
    socket: PathBuf,
    token_file: Option<PathBuf>,
    client: Option<Client>,
    backoff: Duration,
}

impl Sender {
    pub fn new(socket: PathBuf, token_file: Option<PathBuf>) -> Self {
        Self {
            socket,
            token_file,
            client: None,
            backoff: INITIAL_BACKOFF,
        }
    }

    /// `hello` + token-file auth on a fresh connection.
    fn connect(&mut self) -> Result<(), ClientError> {
        let mut c = Client::connect(&self.socket)?;
        c.hello(&format!("ds-focus/{}", env!("CARGO_PKG_VERSION")))?;
        c.authenticate_token_file(self.token_file.as_deref())?;
        self.client = Some(c);
        self.backoff = INITIAL_BACKOFF;
        Ok(())
    }

    /// Read `/settings/senders/focus` (optional config; `None` when unset or unreadable).
    /// Only called when the daemon is up, so a failure here is non-fatal.
    pub fn sender_config(&mut self) -> Option<Value> {
        if self.client.is_none() && self.connect().is_err() {
            return None;
        }
        match self
            .client
            .as_mut()
            .unwrap()
            .get("/settings/senders/focus", None)
        {
            Ok(resp) => Some(resp["result"]["value"].clone()),
            Err(_) => None, // E_PATH when unset — fine
        }
    }

    /// Emit one payload on `app/focus/changed`. Returns the measured emit latency.
    /// Logs the emit at `info`. Retries an unreachable daemon with backoff for up to
    /// [`UNREACHABLE_TIMEOUT`] (measured from the first failure), then gives up.
    pub fn emit(&mut self, payload: &Value) -> Result<Duration, SendError> {
        let first_fail: Option<Instant> = None;
        self.emit_inner(payload, first_fail)
    }

    fn emit_inner(
        &mut self,
        payload: &Value,
        mut first_fail: Option<Instant>,
    ) -> Result<Duration, SendError> {
        loop {
            if self.client.is_none() {
                match self.connect() {
                    Ok(()) => {}
                    Err(e) => {
                        if !matches!(e, ClientError::Unreachable { .. }) {
                            return Err(SendError::Client(e));
                        }
                        first_fail = Some(*first_fail.get_or_insert_with(Instant::now));
                        if first_fail.unwrap().elapsed() >= UNREACHABLE_TIMEOUT {
                            return Err(SendError::UnreachableTimeout {
                                path: self.socket.clone(),
                            });
                        }
                        crate::log::warn(
                            "ds-focus::sender",
                            &format!("daemon unreachable ({e}); retrying in {:?}", self.backoff),
                        );
                        std::thread::sleep(self.backoff);
                        self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
                        continue;
                    }
                }
            }
            let client = self.client.as_mut().expect("connected above");
            let t0 = Instant::now();
            match client.request(emit_request(payload)) {
                Ok(resp) => {
                    let emit_ms = t0.elapsed();
                    let subs = resp["result"]["subscribers"].as_u64().unwrap_or(0);
                    crate::log::info(
                        "ds-focus::sender",
                        &format!(
                            "emit {TOPIC} payload={} subscribers={subs} emit_ms={:.1}",
                            serde_json::to_string(payload).unwrap_or_default(),
                            emit_ms.as_secs_f64() * 1000.0,
                        ),
                    );
                    if emit_ms > Duration::from_millis(500) {
                        crate::log::warn(
                            "ds-focus::sender",
                            &format!(
                                "emit took {:.1} ms (> 500 ms budget) for {TOPIC}",
                                emit_ms.as_secs_f64() * 1000.0
                            ),
                        );
                    }
                    return Ok(emit_ms);
                }
                Err(e) => {
                    // drop the connection; reconnect (with backoff if unreachable) and retry
                    self.client = None;
                    if !matches!(e, ClientError::Unreachable { .. }) {
                        return Err(SendError::Client(e));
                    }
                    first_fail = Some(*first_fail.get_or_insert_with(Instant::now));
                    if first_fail.unwrap().elapsed() >= UNREACHABLE_TIMEOUT {
                        return Err(SendError::UnreachableTimeout {
                            path: self.socket.clone(),
                        });
                    }
                    crate::log::warn(
                        "ds-focus::sender",
                        &format!("emit failed ({e}); reconnecting in {:?}", self.backoff),
                    );
                    std::thread::sleep(self.backoff);
                    self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    }
}

/// Resolve the control socket: `--socket`, else the ds-ipc default.
pub fn socket_path(cfg: &crate::args::Config) -> PathBuf {
    cfg.socket
        .clone()
        .unwrap_or_else(ds_ipc::paths::default_socket_path)
}

/// The token file in effect: `--token-file`, else `<socket dir>/token` (ds-client default).
pub fn token_file(cfg: &crate::args::Config) -> Option<PathBuf> {
    cfg.token_file.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn emit_request_shape_is_stable() {
        let req = emit_request(&json!({"app": "code", "title": "x"}));
        assert_eq!(req["op"], "emit");
        assert_eq!(req["topic"], "app/focus/changed");
        assert_eq!(req["payload"], json!({"app": "code", "title": "x"}));
        ds_ipc::message::validate_request(&req).unwrap();
    }

    #[test]
    fn unreachable_maps_to_exit_6() {
        let e = SendError::UnreachableTimeout {
            path: PathBuf::from("/tmp/x.sock"),
        };
        assert_eq!(e.exit_code(), 6);
        assert!(e.to_string().contains("ds daemon start"));
        assert!(e.to_string().contains("/tmp/x.sock"));
        let c = SendError::Client(ClientError::Limit("x".into()));
        assert_eq!(c.exit_code(), 5);
    }

    #[test]
    fn socket_path_defaults() {
        let cfg = crate::args::Config::default();
        assert_eq!(socket_path(&cfg), ds_ipc::paths::default_socket_path());
        let cfg = crate::args::Config {
            socket: Some("/tmp/x.sock".into()),
            ..Default::default()
        };
        assert_eq!(socket_path(&cfg), PathBuf::from("/tmp/x.sock"));
        assert!(token_file(&cfg).is_none());
    }

    #[test]
    fn backoff_grows_and_caps() {
        let mut b = INITIAL_BACKOFF;
        b = (b * 2).min(MAX_BACKOFF);
        assert_eq!(b, Duration::from_millis(200));
        for _ in 0..20 {
            b = (b * 2).min(MAX_BACKOFF);
        }
        assert_eq!(b, MAX_BACKOFF);
    }
}
