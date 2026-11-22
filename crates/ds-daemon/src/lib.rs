//! `ds-daemon` — the single long-lived daemon process: state task, IPC control server,
//! device task, lifecycle (docs/components/daemon.md, ticket DS-015).
//!
//! Phase-1 skeleton, all on std threads (no async runtime — the ds-ipc server, the state
//! task and the device task each own one thread, wired by bounded channels):
//!
//! - [`state`] — the state task: the **single writer** of the `ds-state` document. The IPC
//!   handler never touches the store directly; it sends commands over a bounded channel
//!   and waits for the reply, so no state lock is ever held across device I/O or socket
//!   I/O.
//! - [`ipc`] — the control-plane handler embedded in the `ds-ipc` [`Server`]: scoped token
//!   auth is done by the server itself; this handler serves the phase-1 state ops
//!   (`ping`, `get`, `set`, `subscribe`/`unsubscribe`, `daemon status`, `schema`).
//! - [`device`] — the device task: owns the `ds-device` [`Transport`] opened from the
//!   device URI and applies state image/color pushes to the hardware.
//! - [`router`] — the event bus + router (DS-040): device input reports become
//!   `deck/control/*` events, committed state changes become state events, the `emit`
//!   op publishes `app/*`/`deck/*` topics; IPC `subscribe`/`unsubscribe` register on it.
//! - [`daemon`] — the glue: arg parsing + assembling the tasks + the run loop.
//! - [`lifecycle`] — data dir resolution + permissions, token, pid file, structured logs,
//!   SIGTERM handling, socket/pid cleanup.
//!
//! [`Server`]: ds_ipc::Server
//! [`Transport`]: ds_device::Transport

pub mod daemon;
pub mod device;
pub mod ipc;
pub mod lifecycle;
pub mod router;
pub mod state;

/// `schemas/stream-frame.json`, embedded so the `schema` op can serve it like the others.
pub const STREAM_FRAME_SCHEMA_JSON: &str = include_str!("../../../schemas/stream-frame.json");

/// Crate name, for logs and `--version` output.
pub const CRATE_NAME: &str = "ds-daemon";

/// Crate version, read from `Cargo.toml` at compile time.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The `server` field of the `welcome` result (`ds-daemon/0.1.0`).
pub fn server_name() -> String {
    format!("{CRATE_NAME}/{}", version())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_metadata() {
        assert_eq!(CRATE_NAME, "ds-daemon");
        assert!(!version().is_empty());
        assert!(server_name().starts_with("ds-daemon/"));
        assert!(serde_json::from_str::<serde_json::Value>(STREAM_FRAME_SCHEMA_JSON).is_ok());
    }
}
