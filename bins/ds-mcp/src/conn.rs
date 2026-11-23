//! Connection config and the error mapping shared by every tool: one `ds-client`
//! connection per tool call (connect → `hello` → token-file auth, exactly like the
//! `ds` CLI), and daemon/client failures mapped to the stable `E_*` code plus the CLI
//! exit code (`docs/contracts/ipc.md` §4, `docs/contracts/cli.md` §3).

use ds_client::{Client, ClientError};
use ds_ipc::error::ErrorCode;
use std::path::{Path, PathBuf};

/// The `client` field of the `hello` handshake.
pub const CLIENT_NAME: &str = concat!("ds-mcp/", env!("CARGO_PKG_VERSION"));

/// Where the bridge connects: the control socket plus the token file used for auth.
/// `--socket` wins over `--data-dir/daemon.sock`; the token defaults to `<socket
/// dir>/token` (the same rule `ds-client` applies).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub socket: PathBuf,
    pub token_file: Option<PathBuf>,
}

impl Config {
    pub fn new(socket: PathBuf, token_file: Option<PathBuf>) -> Self {
        Self { socket, token_file }
    }

    /// Resolve `--socket` / `--data-dir` / defaults into the control socket path.
    pub fn resolve_socket(socket: Option<PathBuf>, data_dir: Option<PathBuf>) -> PathBuf {
        match (socket, data_dir) {
            (Some(s), _) => s,
            (None, Some(d)) => d.join("daemon.sock"),
            (None, None) => ds_ipc::paths::default_socket_path(),
        }
    }

    /// The token file in effect: `--token-file`, else `<socket dir>/token` (0600).
    pub fn token_path(&self) -> PathBuf {
        self.token_file.clone().unwrap_or_else(|| {
            self.socket
                .parent()
                .map(|d| d.join("token"))
                .unwrap_or_else(ds_ipc::paths::default_token_path)
        })
    }

    /// The frame-stream socket: the sibling `stream.sock` of the control socket.
    pub fn stream_sock(&self) -> PathBuf {
        self.socket
            .parent()
            .map(|d| d.join("stream.sock"))
            .unwrap_or_else(|| PathBuf::from("stream.sock"))
    }

    /// The data dir in effect (the socket's parent directory).
    pub fn data_dir(&self) -> PathBuf {
        self.socket
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(ds_ipc::paths::default_dir)
    }
}

/// Connect + `hello` + token-file auth — the same handshake the `ds` CLI performs.
/// Connect failures map to `E_UNREACHABLE` (exit 6) with the `ds daemon start` hint.
pub fn connect(cfg: &Config) -> Result<Client, ClientError> {
    let mut c = Client::connect(&cfg.socket)?;
    c.hello(CLIENT_NAME)?;
    c.authenticate_token_file(cfg.token_file.as_deref())?;
    Ok(c)
}

/// Read the token file for the hand-rolled watch connection (same rules as
/// `Client::authenticate_token_file`: unreadable or too short → `E_AUTH`, exit 4,
/// never an unauthenticated fallback).
pub fn read_token(cfg: &Config) -> Result<String, ClientError> {
    let path = cfg.token_path();
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        ClientError::Server(ds_ipc::error::IpcError::new(
            ErrorCode::EAuth,
            format!("cannot read token file {}: {e}", path.display()),
        ))
    })?;
    let token = raw.trim().to_string();
    if token.len() < 16 {
        return Err(ClientError::Server(ds_ipc::error::IpcError::new(
            ErrorCode::EAuth,
            format!("token file {} is too short", path.display()),
        )));
    }
    Ok(token)
}

/// A tool failure: the stable `E_*` code and the CLI exit code, preserved in the text
/// of the MCP tool error (`E_PATH: path not found: … [exit 2]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolFailure {
    pub code: String,
    pub message: String,
    pub exit: i32,
}

impl ToolFailure {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: code.as_str().to_string(),
            message: message.into(),
            exit: code.exit_code(),
        }
    }

    /// The `content[0].text` of the MCP tool error result.
    pub fn text(&self) -> String {
        format!("{}: {} [exit {}]", self.code, self.message, self.exit)
    }
}

impl From<ClientError> for ToolFailure {
    fn from(e: ClientError) -> Self {
        let code = e.code();
        // Server errors already carry the code in their Display; use the bare message so
        // the tool error text reads `E_PATH: <message> [exit 2]`, not `E_PATH: E_PATH: …`.
        let message = match &e {
            ClientError::Server(inner) => inner.message.clone(),
            other => other.to_string(),
        };
        Self {
            code: code.as_str().to_string(),
            message,
            exit: e.exit_code(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_ipc::error::IpcError;

    #[test]
    fn config_paths_derive_from_the_socket() {
        let cfg = Config::new(PathBuf::from("/tmp/x/daemon.sock"), None);
        assert_eq!(cfg.token_path(), PathBuf::from("/tmp/x/token"));
        assert_eq!(cfg.stream_sock(), PathBuf::from("/tmp/x/stream.sock"));
        assert_eq!(cfg.data_dir(), PathBuf::from("/tmp/x"));
        let cfg = Config::new(
            PathBuf::from("/tmp/x/daemon.sock"),
            Some(PathBuf::from("/elsewhere/tok")),
        );
        assert_eq!(cfg.token_path(), PathBuf::from("/elsewhere/tok"));
    }

    #[test]
    fn resolve_socket_precedence() {
        assert_eq!(
            Config::resolve_socket(Some("/a.sock".into()), Some("/d".into())),
            PathBuf::from("/a.sock")
        );
        assert_eq!(
            Config::resolve_socket(None, Some("/d".into())),
            PathBuf::from("/d/daemon.sock")
        );
        assert_eq!(
            Config::resolve_socket(None, None),
            ds_ipc::paths::default_socket_path()
        );
    }

    #[test]
    fn tool_failure_preserves_code_and_exit() {
        let f = ToolFailure::from(ClientError::Server(IpcError::new(ErrorCode::EPath, "nope")));
        assert_eq!(f.code, "E_PATH");
        assert_eq!(f.exit, 2);
        assert_eq!(f.text(), "E_PATH: nope [exit 2]");
        let f = ToolFailure::new(
            ErrorCode::EUnreachable,
            "daemon unreachable at /x — start it with: ds daemon start",
        );
        assert_eq!(f.exit, 6);
        assert!(f.text().contains("ds daemon start"));
        let f = ToolFailure::from(ClientError::Limit("too big".into()));
        assert_eq!((f.code.as_str(), f.exit), ("E_LIMIT", 5));
    }

    #[test]
    fn connect_to_missing_socket_is_unreachable_exit_6() {
        let cfg = Config::new(
            PathBuf::from("/tmp/ds-mcp-definitely-missing-xyz/daemon.sock"),
            None,
        );
        let err = match connect(&cfg) {
            Err(e) => e,
            Ok(_) => panic!("connect to a missing socket must fail"),
        };
        assert_eq!(err.code(), ErrorCode::EUnreachable);
        assert_eq!(err.exit_code(), 6);
        let f = ToolFailure::from(err);
        assert!(f.text().contains("ds daemon start"), "{}", f.text());
    }
}
