//! Connection setup shared by all daemon-backed verbs: resolve the socket / data dir,
//! connect, `hello` handshake, then token auth from `--token-file` (default
//! `<socket dir>/token`, 0600). Unreadable token → `E_AUTH` (exit 4), never an
//! unauthenticated fallback (`docs/components/cli.md`).

use crate::cli::Global;
use ds_client::{Client, ClientError};
use std::path::PathBuf;

/// The control socket in effect: `--socket`, else the ds-ipc default.
pub fn socket_path(g: &Global) -> PathBuf {
    g.socket
        .clone()
        .unwrap_or_else(ds_ipc::paths::default_socket_path)
}

/// The data dir in effect: `--data-dir`, else the ds-ipc default
/// (`$XDG_RUNTIME_DIR/deckstack`, fallback `~/.local/state/deckstack`).
pub fn data_dir(g: &Global) -> PathBuf {
    g.data_dir
        .clone()
        .unwrap_or_else(ds_ipc::paths::default_dir)
}

/// The `client` field of the `hello` handshake and of `ds status` output.
pub fn client_name() -> String {
    format!("ds/{}", env!("CARGO_PKG_VERSION"))
}

/// Connect + `hello` + token-file auth. Connect failures map to `E_UNREACHABLE`
/// (exit 6) with the `ds daemon start` hint.
pub fn connect(g: &Global) -> Result<Client, ClientError> {
    let mut c = Client::connect(&socket_path(g))?;
    c.hello(&client_name())?;
    c.authenticate_token_file(g.token_file.as_deref())?;
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_and_data_dir_defaults() {
        let g = Global::default();
        assert_eq!(socket_path(&g), ds_ipc::paths::default_socket_path());
        assert_eq!(data_dir(&g), ds_ipc::paths::default_dir());
        let g = Global {
            socket: Some("/tmp/x.sock".into()),
            data_dir: Some("/tmp/x".into()),
            ..Global::default()
        };
        assert_eq!(socket_path(&g), PathBuf::from("/tmp/x.sock"));
        assert_eq!(data_dir(&g), PathBuf::from("/tmp/x"));
    }

    #[test]
    fn client_name_is_ds_version() {
        assert!(client_name().starts_with("ds/"));
        assert!(client_name().contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn connect_to_missing_socket_is_unreachable_exit_6() {
        let g = Global {
            socket: Some("/tmp/ds-definitely-missing-xyz/daemon.sock".into()),
            ..Global::default()
        };
        let err = match connect(&g) {
            Err(e) => e,
            Ok(_) => panic!("connect to a missing socket must fail"),
        };
        assert_eq!(err.code(), ds_ipc::error::ErrorCode::EUnreachable);
        assert_eq!(err.exit_code(), 6);
    }
}
