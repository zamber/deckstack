//! The documented control-plane limits (`docs/contracts/ipc.md` §5) and the tunable
//! [`ServerConfig`].

use std::time::Duration;

/// Maximum length of one NDJSON request line: **1 MiB** (`docs/contracts/ipc.md` §2/§5).
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Maximum length of one NDJSON response line: **4 MiB** (`docs/contracts/ipc.md` §5).
pub const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// Maximum simultaneous connections: **64** (`docs/contracts/ipc.md` §5).
pub const MAX_CONNECTIONS: usize = 64;

/// Maximum in-flight (read but not yet answered) requests per connection: **64**
/// (`docs/contracts/ipc.md` §5). The reference server answers sequentially, so it never
/// exceeds 1; the cap is enforced by the framing layer for pipelined readers.
pub const MAX_IN_FLIGHT: usize = 64;

/// Idle connection timeout: **300 s**; subscriptions are exempt
/// (`docs/contracts/ipc.md` §5).
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Grace for requests already in flight on an established connection when the token set
/// rotates: **≤ 5 s** (S12/M4, `docs/contracts/ipc.md` §3). Revocation is instant for new
/// connections regardless.
pub const REVOCATION_GRACE: Duration = Duration::from_secs(5);

/// Server configuration. The defaults are the contract values above.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Request line cap in bytes.
    pub max_line_bytes: usize,
    /// Response line cap in bytes.
    pub max_response_bytes: usize,
    /// Maximum simultaneous connections.
    pub max_connections: usize,
    /// Maximum in-flight requests per connection.
    pub max_in_flight: usize,
    /// Idle timeout; `None` disables it. Subscription connections are exempt.
    pub idle_timeout: Option<Duration>,
    /// In-flight grace after a token rotation.
    pub revocation_grace: Duration,
    /// `server` field of the `welcome` result (e.g. `ds-daemon/0.1.0`).
    pub server_name: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_line_bytes: MAX_LINE_BYTES,
            max_response_bytes: MAX_RESPONSE_BYTES,
            max_connections: MAX_CONNECTIONS,
            max_in_flight: MAX_IN_FLIGHT,
            idle_timeout: Some(IDLE_TIMEOUT),
            revocation_grace: REVOCATION_GRACE,
            server_name: format!("ds-daemon/{}", crate::version()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_contract() {
        let c = ServerConfig::default();
        assert_eq!(c.max_line_bytes, 1024 * 1024);
        assert_eq!(c.max_response_bytes, 4 * 1024 * 1024);
        assert_eq!(c.max_connections, 64);
        assert_eq!(c.max_in_flight, 64);
        assert_eq!(c.idle_timeout, Some(Duration::from_secs(300)));
        assert!(c.revocation_grace <= Duration::from_secs(5));
    }
}
