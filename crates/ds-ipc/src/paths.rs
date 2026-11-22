//! Default filesystem locations of the control plane (ADR-003, `docs/contracts/ipc.md` §1.1):
//! the socket lives at `$XDG_RUNTIME_DIR/deckstack/daemon.sock` with fallback
//! `~/.local/state/deckstack/daemon.sock`; the token files live in the same directory.

use std::path::PathBuf;

/// The per-install state/run directory: `$XDG_RUNTIME_DIR/deckstack` when set, else
/// `~/.local/state/deckstack`.
pub fn default_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("deckstack");
        }
    }
    home_dir().join(".local").join("state").join("deckstack")
}

/// The control socket path: `<default_dir>/daemon.sock`.
pub fn default_socket_path() -> PathBuf {
    default_dir().join("daemon.sock")
}

/// The per-install token file path: `<default_dir>/token` (0600, `admin` scope).
pub fn default_token_path() -> PathBuf {
    default_dir().join("token")
}

/// The scoped-token file path: `<default_dir>/tokens.json` (0600).
pub fn default_tokens_path() -> PathBuf {
    default_dir().join("tokens.json")
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_live_under_the_dir() {
        let dir = default_dir();
        assert!(default_socket_path().starts_with(&dir));
        assert_eq!(default_socket_path().file_name().unwrap(), "daemon.sock");
        assert_eq!(default_token_path().file_name().unwrap(), "token");
        assert_eq!(default_tokens_path().file_name().unwrap(), "tokens.json");
    }
}
