//! Token auth for the control plane (`docs/contracts/ipc.md` §3, ADR-003):
//! the per-install token file `<dir>/token` (0600, grants `admin`) plus scoped tokens in
//! `<dir>/tokens.json` (0600), compared **constant-time**. The token set is re-read from
//! disk per connection (and on token-file change), so revocation is instant for new
//! connections (S12/M4) — there is no caching of per-run tokens (D14).

use crate::error::{ErrorCode, IpcError};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The three scopes (`schemas/ipc-message.json` `auth.scope` enum), ordered
/// `read` < `mutate` < `admin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Read-only ops (`get`, `subscribe`, `stream-stats`, `schema`, …).
    Read,
    /// State-changing ops (`set`, `emit`, `bind`, `stream-open`, …).
    Mutate,
    /// Administrative ops (`token issue/revoke`, `daemon stop/reload`); the install token.
    Admin,
}

impl Scope {
    /// The wire spelling (`read` / `mutate` / `admin`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Mutate => "mutate",
            Self::Admin => "admin",
        }
    }

    /// Parse a wire scope; `None` for anything else.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "read" => Self::Read,
            "mutate" => Self::Mutate,
            "admin" => Self::Admin,
            _ => return None,
        })
    }

    /// `self` covers `required` (scope lattice: read < mutate < admin).
    pub fn covers(self, required: Scope) -> bool {
        self >= required
    }

    /// `min(requested, granted)` — what an `auth` request is granted
    /// (`docs/contracts/ipc.md` §3).
    pub fn min_of(self, other: Scope) -> Scope {
        if self < other {
            self
        } else {
            other
        }
    }
}

/// One scoped token in `<dir>/tokens.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopedToken {
    pub id: String,
    pub token: String,
    pub scope: Scope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

/// The on-disk shape of `<dir>/tokens.json` (0600).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokensFile {
    #[serde(default)]
    pub tokens: Vec<ScopedToken>,
}

/// A loaded token set: the install token (`admin`) plus the scoped tokens.
#[derive(Debug, Clone)]
pub struct TokenSet {
    dir: PathBuf,
    install_token: String,
    scoped: Vec<ScopedToken>,
}

/// Why a token set could not be loaded. Maps to `E_AUTH` at the auth boundary (fail
/// closed — the daemon never falls back to unauthenticated, `docs/components/cli.md`).
#[derive(Debug)]
pub enum AuthError {
    /// I/O error reading the token files.
    Io(io::Error),
    /// The token file exists but is group/other-readable/writable (must be 0600).
    InsecurePermissions(PathBuf),
    /// `tokens.json` is malformed.
    Malformed(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "cannot read token files: {e}"),
            Self::InsecurePermissions(p) => {
                write!(f, "token file {}: permissions must be 0600", p.display())
            }
            Self::Malformed(m) => write!(f, "tokens file malformed: {m}"),
        }
    }
}

impl std::error::Error for AuthError {}

impl TokenSet {
    /// Load the token set from `dir` (`<dir>/token` + `<dir>/tokens.json`), failing
    /// closed on I/O errors or insecure permissions (0600 required on both files).
    pub fn load(dir: &Path) -> Result<Self, AuthError> {
        let token_path = dir.join("token");
        check_perms(&token_path)?;
        let install_token = fs::read_to_string(&token_path)
            .map_err(AuthError::Io)?
            .trim()
            .to_string();
        if install_token.len() < 16 {
            return Err(AuthError::Malformed(format!(
                "{}: install token shorter than 16 chars",
                token_path.display()
            )));
        }
        let tokens_path = dir.join("tokens.json");
        let scoped = if tokens_path.exists() {
            check_perms(&tokens_path)?;
            let raw = fs::read_to_string(&tokens_path).map_err(AuthError::Io)?;
            let file: TokensFile = serde_json::from_str(&raw)
                .map_err(|e| AuthError::Malformed(format!("{}: {e}", tokens_path.display())))?;
            file.tokens
        } else {
            Vec::new()
        };
        Ok(Self {
            dir: dir.to_path_buf(),
            install_token,
            scoped,
        })
    }

    /// The directory this set was loaded from.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Look up a token constant-time; returns the scope it grants (`admin` for the
    /// install token) or `None` for a bad/revoked token.
    pub fn scope_of(&self, token: &str) -> Option<Scope> {
        if constant_time_eq(self.install_token.as_bytes(), token.as_bytes()) {
            return Some(Scope::Admin);
        }
        self.scoped
            .iter()
            .find(|t| constant_time_eq(t.token.as_bytes(), token.as_bytes()))
            .map(|t| t.scope)
    }

    /// Whether the token is still present in this set (revocation check).
    pub fn contains(&self, token: &str) -> bool {
        self.scope_of(token).is_some()
    }

    /// The install token (never logged; `admin` scope).
    pub fn install_token(&self) -> &str {
        &self.install_token
    }

    /// The scoped tokens.
    pub fn scoped(&self) -> &[ScopedToken] {
        &self.scoped
    }

    /// Write the install token file (`<dir>/token`, 0600) plus an empty `tokens.json`.
    /// Used by first daemon start and by tests.
    pub fn create(dir: &Path, install_token: &str) -> io::Result<PathBuf> {
        fs::create_dir_all(dir)?;
        let token_path = dir.join("token");
        write_0600(&token_path, format!("{install_token}\n").as_bytes())?;
        let tokens_path = dir.join("tokens.json");
        if !tokens_path.exists() {
            write_0600(&tokens_path, b"{\"tokens\":[]}")?;
        }
        Ok(token_path)
    }

    /// Replace `tokens.json` (0600) — `ds token issue/revoke` lands here (DS-015/DS-016);
    /// exposed so the daemon's token ops and tests share one writer.
    pub fn write_scoped(dir: &Path, tokens: &[ScopedToken]) -> io::Result<()> {
        let file = TokensFile {
            tokens: tokens.to_vec(),
        };
        let raw = serde_json::to_vec_pretty(&file).map_err(io::Error::other)?;
        write_0600(&dir.join("tokens.json"), &raw)
    }

    /// Append one scoped token to `tokens.json`.
    pub fn issue(dir: &Path, id: &str, token: &str, scope: Scope) -> io::Result<ScopedToken> {
        let mut set = if dir.join("tokens.json").exists() {
            Self::load(dir).map_err(io::Error::other)?
        } else {
            Self {
                dir: dir.to_path_buf(),
                install_token: String::new(),
                scoped: Vec::new(),
            }
        };
        let rec = ScopedToken {
            id: id.to_string(),
            token: token.to_string(),
            scope,
            created_at: Some(
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| format!("{}s", d.as_secs()))
                    .unwrap_or_default(),
            ),
        };
        set.scoped.push(rec.clone());
        Self::write_scoped(dir, &set.scoped)?;
        Ok(rec)
    }

    /// Remove a scoped token by id (revocation); returns whether it existed.
    pub fn revoke(dir: &Path, id: &str) -> io::Result<bool> {
        if !dir.join("tokens.json").exists() {
            return Ok(false);
        }
        let mut set = Self::load(dir).map_err(io::Error::other)?;
        let before = set.scoped.len();
        set.scoped.retain(|t| t.id != id);
        let existed = set.scoped.len() != before;
        if existed {
            Self::write_scoped(dir, &set.scoped)?;
        }
        Ok(existed)
    }

    /// Authenticate an `auth` request: returns the granted scope = min(requested, token
    /// scope), or an `E_AUTH` error for a bad/missing token.
    pub fn authenticate(&self, token: &str, requested: Option<Scope>) -> Result<Scope, IpcError> {
        match self.scope_of(token) {
            Some(granted) => Ok(requested.unwrap_or(granted).min_of(granted)),
            None => Err(IpcError::new(ErrorCode::EAuth, "bad token")),
        }
    }
}

/// Constant-time byte-string equality (the `hmac.compare_digest` precedent of ADR-003).
/// Lengths are compared first — token lengths are not secret.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

/// Generate a fresh random token (32 bytes from `/dev/urandom`, hex-encoded, 64 chars).
pub fn generate_token() -> io::Result<String> {
    use std::io::Read as _;
    let mut buf = [0u8; 32];
    fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

fn check_perms(path: &Path) -> Result<(), AuthError> {
    use std::os::unix::fs::PermissionsExt;
    if !path.exists() {
        return Ok(());
    }
    let mode = fs::metadata(path)
        .map_err(AuthError::Io)?
        .permissions()
        .mode();
    if mode & 0o177 != 0 {
        return Err(AuthError::InsecurePermissions(path.to_path_buf()));
    }
    Ok(())
}

fn write_0600(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    std::io::Write::write_all(&mut f, bytes)?;
    // Belt and braces: the mode only applies at creation.
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Read;

    #[test]
    fn scope_lattice() {
        assert!(Scope::Admin.covers(Scope::Mutate));
        assert!(Scope::Admin.covers(Scope::Read));
        assert!(Scope::Mutate.covers(Scope::Read));
        assert!(!Scope::Read.covers(Scope::Mutate));
        assert!(!Scope::Mutate.covers(Scope::Admin));
        assert_eq!(Scope::Read.min_of(Scope::Admin), Scope::Read);
        assert_eq!(Scope::Admin.min_of(Scope::Read), Scope::Read);
        assert_eq!(Scope::parse("mutate"), Some(Scope::Mutate));
        assert_eq!(Scope::parse("bogus"), None);
        assert_eq!(Scope::Admin.as_str(), "admin");
    }

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn token_set_round_trip_and_scopes() {
        let dir = tempfile::tempdir().unwrap();
        let install = generate_token().unwrap();
        TokenSet::create(dir.path(), &install).unwrap();
        // files are 0600
        use std::os::unix::fs::PermissionsExt;
        for name in ["token", "tokens.json"] {
            let mode = fs::metadata(dir.path().join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{name} must be 0600");
        }
        let scoped_tok = generate_token().unwrap();
        TokenSet::issue(dir.path(), "t0001", &scoped_tok, Scope::Read).unwrap();

        let set = TokenSet::load(dir.path()).unwrap();
        assert_eq!(set.authenticate(&install, None).unwrap(), Scope::Admin);
        assert_eq!(
            set.authenticate(&install, Some(Scope::Read)).unwrap(),
            Scope::Read
        );
        assert_eq!(set.authenticate(&scoped_tok, None).unwrap(), Scope::Read);
        assert_eq!(
            set.authenticate(&scoped_tok, Some(Scope::Admin)).unwrap(),
            Scope::Read
        );
        let bad = "0".repeat(64);
        let err = set.authenticate(&bad, None).unwrap_err();
        assert_eq!(err.code, ErrorCode::EAuth);

        // instant revocation: a fresh load no longer knows the token
        assert!(TokenSet::revoke(dir.path(), "t0001").unwrap());
        let set = TokenSet::load(dir.path()).unwrap();
        assert!(!set.contains(&scoped_tok));
        assert_eq!(
            set.authenticate(&scoped_tok, None).unwrap_err().code,
            ErrorCode::EAuth
        );
        assert!(!TokenSet::revoke(dir.path(), "t0001").unwrap());
    }

    #[test]
    fn insecure_permissions_fail_closed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let install = generate_token().unwrap();
        TokenSet::create(dir.path(), &install).unwrap();
        fs::set_permissions(dir.path().join("token"), fs::Permissions::from_mode(0o644)).unwrap();
        match TokenSet::load(dir.path()) {
            Err(AuthError::InsecurePermissions(p)) => {
                assert!(p.ends_with("token"));
            }
            other => panic!("expected InsecurePermissions, got {other:?}"),
        }
        // missing dir -> Io error
        let missing = dir.path().join("nope");
        assert!(matches!(TokenSet::load(&missing), Err(AuthError::Io(_))));
    }

    #[test]
    fn generate_token_shape() {
        let t = generate_token().unwrap();
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        let mut f = fs::File::open("/dev/urandom").unwrap();
        let mut b = [0u8; 1];
        f.read_exact(&mut b).unwrap();
    }

    #[test]
    fn tokens_file_serializes() {
        let f = TokensFile {
            tokens: vec![ScopedToken {
                id: "t0001".into(),
                token: "x".repeat(64),
                scope: Scope::Mutate,
                created_at: None,
            }],
        };
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(
            v,
            json!({"tokens": [{"id": "t0001", "token": "x".repeat(64), "scope": "mutate"}]})
        );
    }
}
