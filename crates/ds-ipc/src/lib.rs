//! `ds-ipc` — the daemon's control plane at the transport level: NDJSON framing over a
//! Unix socket, the `hello`/`auth` handshake with scoped tokens, the error envelope, and
//! the documented limits (`docs/contracts/ipc.md`, `schemas/ipc-message.json`, ADR-003).
//!
//! The crate is **transport-only by design**: no state document, no device logic, no daemon
//! loop. A server is a [`Server`] bound to a socket plus a user-supplied [`Handler`] that
//! answers post-auth ops; the daemon (`ds-daemon`, DS-015) embeds both. A client lives in
//! the `ds-client` crate.
//!
//! What is implemented here, per the contract:
//!
//! - **Framing** ([`framing`]): one JSON object per line, one request line → one response
//!   line with the same `id`; request lines are capped at [`limits::MAX_LINE_BYTES`]
//!   (1 MiB) and responses at [`limits::MAX_RESPONSE_BYTES`] (4 MiB).
//! - **Validation** ([`message`]): every line in both directions is validated against the
//!   embedded `schemas/ipc-message.json` (draft 2020-12). Requests never carry `ok`;
//!   responses always do. Bad lines get an `E_SCHEMA` error envelope.
//! - **Handshake and auth** ([`server`], [`auth`]): first message must be
//!   `hello {"protocol":1}` → `welcome` (mismatch → `E_VERSION`); then `auth` with a token
//!   from `<dir>/token` (install token, `admin` scope, 0600) or `<dir>/tokens.json`
//!   (scoped tokens), compared constant-time. The token set is re-read per connection, so
//!   revocation is instant for new connections (S12/M4); requests on an established
//!   connection get at most [`limits::REVOCATION_GRACE`] (≤ 5 s) after a rotation.
//! - **Scopes** ([`auth::Scope`]): `read` < `mutate` < `admin`, enforced per op via
//!   [`server::required_scope`]; `E_SCOPE` when the granted scope is below the op's
//!   requirement, `E_AUTH` for a bad/missing token.
//! - **Errors** ([`error`]): the stable-code envelope of `docs/contracts/ipc.md` §4, with
//!   the CLI exit-code mapping on [`error::ErrorCode::exit_code`].
//! - **Limits** ([`limits`]): 1 MiB line / 4 MiB response / 64 connections / 64 in-flight
//!   requests / 300 s idle timeout (subscriptions exempt).
//!
//! ```rust
//! use ds_ipc::{ErrorCode, Handler, Request, Response};
//!
//! struct Ping;
//! impl Handler for Ping {
//!     fn handle(&self, _conn: &ds_ipc::Connection, req: &Request) -> Response {
//!         match req.op() {
//!             "ping" => Response::ok(req.id(), serde_json::json!({"pong": true})),
//!             _ => Response::error_code(req.id(), ErrorCode::EPath, "unknown op"),
//!         }
//!     }
//! }
//! // Server::bind + serve would run the accept loop; see tests/ for full round-trips.
//! ```

pub mod auth;
pub mod error;
pub mod framing;
pub mod limits;
pub mod message;
pub mod paths;
pub mod server;
pub mod testing;

pub use auth::{constant_time_eq, Scope, TokenSet};
pub use error::{ErrorCode, IpcError};
pub use limits::{
    ServerConfig, MAX_CONNECTIONS, MAX_IN_FLIGHT, MAX_LINE_BYTES, MAX_RESPONSE_BYTES,
};
pub use message::{validate_message, validate_request, Request, Response};
pub use server::{required_scope, Connection, Handler, Server};

/// Crate name, for logs and `--version` output.
pub const CRATE_NAME: &str = "ds-ipc";

/// Crate version, read from `Cargo.toml` at compile time.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Wire protocol version spoken by this crate: `protocol: 1` in the hello/welcome
/// handshake (`docs/contracts/ipc.md` §2; breaking changes bump it).
pub const PROTOCOL_VERSION: u32 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_metadata() {
        assert_eq!(CRATE_NAME, "ds-ipc");
        assert!(!version().is_empty());
        assert_eq!(PROTOCOL_VERSION, 1);
    }
}
