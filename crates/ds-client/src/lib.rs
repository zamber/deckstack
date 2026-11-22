//! `ds-client` — the client side of the control plane, shared by the `ds` CLI,
//! `ds-focus`, `ds-mcp` and agents (ADR-002, `docs/components/cli.md`).
//!
//! Blocking, stateless, one-shot by default (`Client::request`), with a persistent
//! subscription mode (`Client::watch`) for `ds watch`. All framing, validation, error
//! codes and limits come from `ds-ipc`; this crate adds socket-path resolution, the
//! token-file handling, and the client-side error model:
//!
//! - socket missing / connect refused → [`ClientError::Unreachable`] → `E_UNREACHABLE`
//!   (exit 6, message includes the `ds daemon start` hint);
//! - `{"ok":false,…}` response → [`ClientError::Server`] carrying the stable code
//!   (exit code via [`ErrorCode::exit_code`]);
//! - request that would exceed the 1 MiB line cap → [`ClientError::Limit`] → `E_LIMIT`;
//! - anything else (I/O, malformed reply) → [`ClientError::Protocol`] → `E_INTERNAL`.
//!
//! ```rust,no_run
//! use ds_client::Client;
//!
//! let mut c = Client::connect_default()?;          // $XDG_RUNTIME_DIR/deckstack/daemon.sock
//! c.hello("ds/0.1.0")?;                            // protocol 1 handshake
//! c.authenticate_token_file(None)?;                // <dir>/token, 0600
//! let rev = c.ping()?;
//! # Ok::<(), ds_client::ClientError>(())
//! ```

use ds_ipc::auth::Scope;
use ds_ipc::error::{ErrorCode, IpcError};
use ds_ipc::framing::{self, FrameError};
use ds_ipc::limits::MAX_LINE_BYTES;
use ds_ipc::message::validate_request;
use ds_ipc::paths;
use serde_json::{json, Value};
use std::fs;
use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

/// Crate name, for logs and `--version` output.
pub const CRATE_NAME: &str = "ds-client";

/// Crate version, read from `Cargo.toml` at compile time.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Everything that can go wrong on the client side. [`ClientError::code`] /
/// [`ClientError::exit_code`] map to the stable error codes and CLI exit codes of
/// `docs/contracts/ipc.md` §4 / `docs/contracts/cli.md` §3.
#[derive(Debug)]
pub enum ClientError {
    /// The daemon socket is missing or refused the connection → `E_UNREACHABLE` (exit 6).
    Unreachable {
        path: PathBuf,
        hint: String,
        source: std::io::Error,
    },
    /// The daemon answered `{"ok":false,"error":{…}}` → the server's stable code.
    Server(IpcError),
    /// A request would exceed the NDJSON line cap → `E_LIMIT` (exit 5). Rejected
    /// client-side before anything is written.
    Limit(String),
    /// The request does not satisfy `schemas/ipc-message.json` → `E_SCHEMA` (exit 1).
    Schema(String),
    /// Broken framing / malformed reply / I/O error → `E_INTERNAL` (exit 8).
    Protocol(String),
}

impl ClientError {
    /// The stable IPC error code for this failure.
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Unreachable { .. } => ErrorCode::EUnreachable,
            Self::Server(e) => e.code,
            Self::Limit(_) => ErrorCode::ELimit,
            Self::Schema(_) => ErrorCode::ESchema,
            Self::Protocol(_) => ErrorCode::EInternal,
        }
    }

    /// The CLI exit code for this failure (`docs/contracts/cli.md` §3).
    pub fn exit_code(&self) -> i32 {
        self.code().exit_code()
    }

    /// The error as an IPC envelope value (for `--json` error output).
    pub fn to_envelope(&self) -> Value {
        let code = self.code();
        json!({
            "code": code.as_str(),
            "message": self.to_string(),
        })
    }
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable { path, hint, source } => write!(
                f,
                "daemon unreachable at {} ({source}) — {hint}",
                path.display()
            ),
            Self::Server(e) => write!(f, "{}: {}", e.code.as_str(), e.message),
            Self::Limit(m) => write!(f, "E_LIMIT: {m}"),
            Self::Schema(m) => write!(f, "E_SCHEMA: {m}"),
            Self::Protocol(m) => write!(f, "E_INTERNAL: {m}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<IpcError> for ClientError {
    fn from(e: IpcError) -> Self {
        Self::Server(e)
    }
}

/// A connected control-plane client. One connection, reusable; the CLI is one-shot per
/// invocation (R1), `watch` keeps it open.
pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: i64,
    socket_path: PathBuf,
    /// Async server messages (events) that arrived while waiting for a response.
    pending_events: Vec<Value>,
    scope: Option<Scope>,
}

impl Client {
    /// Connect to a specific socket path. Connect errors map to `E_UNREACHABLE` with
    /// the `ds daemon start` hint.
    pub fn connect(path: &Path) -> Result<Self, ClientError> {
        let writer = UnixStream::connect(path).map_err(|source| ClientError::Unreachable {
            path: path.to_path_buf(),
            hint: "start it with: ds daemon start".to_string(),
            source,
        })?;
        let reader = BufReader::new(
            writer
                .try_clone()
                .map_err(|e| ClientError::Protocol(e.to_string()))?,
        );
        Ok(Self {
            reader,
            writer,
            next_id: 0,
            socket_path: path.to_path_buf(),
            pending_events: Vec::new(),
            scope: None,
        })
    }

    /// Connect to the default socket (`$XDG_RUNTIME_DIR/deckstack/daemon.sock`,
    /// fallback `~/.local/state/deckstack/daemon.sock`).
    pub fn connect_default() -> Result<Self, ClientError> {
        Self::connect(&paths::default_socket_path())
    }

    /// The socket this client is connected to.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// The scope granted by the last successful `auth` (`None` before auth).
    pub fn scope(&self) -> Option<Scope> {
        self.scope
    }

    /// Fresh correlation id.
    fn next_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Send one request (schema-validated client-side, line-capped) and wait for its
    /// response. The client owns the correlation id. Async events that arrive first are
    /// queued in `pending_events`.
    pub fn request(&mut self, mut req: Value) -> Result<Value, ClientError> {
        let id = self.next_id();
        req["id"] = Value::from(id);
        validate_request(&req).map_err(|e| ClientError::Schema(e.message))?;
        let line = serde_json::to_vec(&req).map_err(|e| ClientError::Protocol(e.to_string()))?;
        if line.len() + 1 > MAX_LINE_BYTES {
            return Err(ClientError::Limit(format!(
                "request line would be {} bytes (cap {MAX_LINE_BYTES})",
                line.len() + 1
            )));
        }
        let mut buf = line;
        buf.push(b'\n');
        self.writer
            .write_all(&buf)
            .map_err(|e| ClientError::Protocol(format!("write: {e}")))?;
        self.writer
            .flush()
            .map_err(|e| ClientError::Protocol(format!("flush: {e}")))?;
        loop {
            let line =
                framing::read_line(&mut self.reader, MAX_LINE_BYTES).map_err(|e| match e {
                    FrameError::Eof => ClientError::Protocol("connection closed by daemon".into()),
                    other => ClientError::Protocol(other.to_ipc_error().message),
                })?;
            let msg: Value = serde_json::from_str(line.trim())
                .map_err(|e| ClientError::Protocol(format!("bad response line: {e}")))?;
            ds_ipc::validate_message(&msg).map_err(|e| ClientError::Protocol(e.message))?;
            if msg.get("ok").is_some() {
                // a response: check correlation
                if msg.get("id").and_then(Value::as_i64) != Some(id) {
                    return Err(ClientError::Protocol(format!(
                        "response id {:?} does not match request id {id}",
                        msg.get("id")
                    )));
                }
                if msg["ok"] == Value::Bool(false) {
                    let err = msg
                        .get("error")
                        .and_then(IpcError::from_value)
                        .unwrap_or_else(|| {
                            IpcError::new(ErrorCode::EInternal, "error without envelope")
                        });
                    return Err(ClientError::Server(err));
                }
                return Ok(msg);
            }
            // async server message (event) — queue it
            self.pending_events.push(msg);
        }
    }

    /// Events that arrived while waiting for responses (drained by [`Client::watch`]).
    pub fn take_pending_events(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.pending_events)
    }

    /// `hello` handshake (protocol 1). Returns the `welcome` result object.
    pub fn hello(&mut self, client: &str) -> Result<Value, ClientError> {
        let resp = self.request(
            json!({"op": "hello", "protocol": ds_ipc::PROTOCOL_VERSION, "client": client}),
        )?;
        Ok(resp["result"].clone())
    }

    /// `auth` with an explicit token; `scope` is the requested scope (granted =
    /// min(requested, token scope)). Returns the granted scope.
    pub fn authenticate(
        &mut self,
        token: &str,
        scope: Option<Scope>,
    ) -> Result<Scope, ClientError> {
        let mut req = json!({"op": "auth", "token": token});
        if let Some(s) = scope {
            req["scope"] = Value::from(s.as_str());
        }
        let resp = self.request(req)?;
        let granted = resp["result"]["scope"]
            .as_str()
            .and_then(Scope::parse)
            .ok_or_else(|| ClientError::Protocol("auth reply has no scope".into()))?;
        self.scope = Some(granted);
        Ok(granted)
    }

    /// `auth` with the token read from a token file (default: `<socket dir>/token`,
    /// 0600). Unreadable token file → `E_AUTH` (exit 4) — never unauthenticated fallback.
    pub fn authenticate_token_file(
        &mut self,
        token_file: Option<&Path>,
    ) -> Result<Scope, ClientError> {
        let path = token_file.map(Path::to_path_buf).unwrap_or_else(|| {
            self.socket_path
                .parent()
                .map(|d| d.join("token"))
                .unwrap_or_else(paths::default_token_path)
        });
        let token = fs::read_to_string(&path).map_err(|e| {
            ClientError::Server(IpcError::new(
                ErrorCode::EAuth,
                format!("cannot read token file {}: {e}", path.display()),
            ))
        })?;
        let token = token.trim().to_string();
        if token.len() < 16 {
            return Err(ClientError::Server(IpcError::new(
                ErrorCode::EAuth,
                format!("token file {} is too short", path.display()),
            )));
        }
        self.authenticate(&token, None)
    }

    /// `ping` → the `{"pong": true}` result.
    pub fn ping(&mut self) -> Result<Value, ClientError> {
        Ok(self.request(json!({"op": "ping"}))?["result"].clone())
    }

    /// `get <path>` (optionally with `fields`).
    pub fn get(&mut self, path: &str, fields: Option<&[String]>) -> Result<Value, ClientError> {
        let mut req = json!({"op": "get", "path": path});
        if let Some(f) = fields {
            req["fields"] = serde_json::to_value(f).unwrap();
        }
        self.request(req)
    }

    /// `set` with a list of ops and an optional `if_rev`. Returns the full response
    /// (carries `rev` on success, `error.current_rev` on `E_CONFLICT`).
    pub fn set(&mut self, ops: Value, if_rev: Option<u64>) -> Result<Value, ClientError> {
        let mut req = json!({"op": "set", "ops": ops});
        if let Some(r) = if_rev {
            req["if_rev"] = Value::from(r);
        }
        self.request(req)
    }

    /// `subscribe <path>` (optional `from_rev`). Events arrive on the connection and
    /// are delivered by [`Client::watch`] / [`Client::take_pending_events`].
    pub fn subscribe(&mut self, path: &str, from_rev: Option<u64>) -> Result<Value, ClientError> {
        let mut req = json!({"op": "subscribe", "path": path});
        if let Some(r) = from_rev {
            req["from_rev"] = Value::from(r);
        }
        self.request(req)
    }

    /// `unsubscribe <path>`.
    pub fn unsubscribe(&mut self, path: &str) -> Result<Value, ClientError> {
        self.request(json!({"op": "unsubscribe", "path": path}))
    }

    /// Persistent subscription (`ds watch`): sends `subscribe`, then reads NDJSON lines
    /// and hands every event to `on_event` until the daemon closes the connection,
    /// `max_events` is reached, or an error response arrives. Returns the number of
    /// events delivered.
    pub fn watch(
        &mut self,
        path: &str,
        from_rev: Option<u64>,
        max_events: Option<usize>,
        on_event: &mut dyn FnMut(&Value),
    ) -> Result<usize, ClientError> {
        self.subscribe(path, from_rev)?;
        let mut delivered = 0;
        for ev in self.take_pending_events() {
            on_event(&ev);
            delivered += 1;
        }
        loop {
            if max_events.is_some_and(|m| delivered >= m) {
                return Ok(delivered);
            }
            let line =
                framing::read_line(&mut self.reader, MAX_LINE_BYTES).map_err(|e| match e {
                    FrameError::Eof => ClientError::Protocol("connection closed by daemon".into()),
                    other => ClientError::Protocol(other.to_ipc_error().message),
                })?;
            if line.trim().is_empty() {
                continue;
            }
            let msg: Value = serde_json::from_str(line.trim())
                .map_err(|e| ClientError::Protocol(format!("bad line: {e}")))?;
            ds_ipc::validate_message(&msg).map_err(|e| ClientError::Protocol(e.message))?;
            if msg.get("ok").is_some() {
                // an error response on the subscription connection (e.g. E_LIMIT close)
                if msg["ok"] == Value::Bool(false) {
                    let err = msg
                        .get("error")
                        .and_then(IpcError::from_value)
                        .unwrap_or_else(|| {
                            IpcError::new(ErrorCode::EInternal, "error without envelope")
                        });
                    return Err(ClientError::Server(err));
                }
                continue;
            }
            on_event(&msg);
            delivered += 1;
        }
    }
}

/// Re-export of the message types senders need.
pub use ds_ipc::message::{Request, Response};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_metadata() {
        assert_eq!(CRATE_NAME, "ds-client");
        assert!(!version().is_empty());
    }

    #[test]
    fn error_mapping() {
        let e = ClientError::Limit("too big".into());
        assert_eq!(e.code(), ErrorCode::ELimit);
        assert_eq!(e.exit_code(), 5);
        let e = ClientError::Server(IpcError::new(ErrorCode::EAuth, "bad token"));
        assert_eq!(e.code(), ErrorCode::EAuth);
        assert_eq!(e.exit_code(), 4);
        let e = ClientError::Schema("nope".into());
        assert_eq!(e.exit_code(), 1);
        let e = ClientError::Protocol("io".into());
        assert_eq!(e.exit_code(), 8);
    }
}
