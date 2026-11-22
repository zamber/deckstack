//! `ds-daemon` — the single long-lived daemon process: state task, hardware I/O, IPC
//! server, event router, frame plane, plugin host.
//!
//! Component spec: `docs/components/daemon.md`. This crate is a stub from the DS-004
//! workspace skeleton: it has no dependencies and no behaviour yet. It stays a library for
//! now; the `ds-daemon` binary lands with the phase-1 ticket that fills this crate.

/// Crate name, for logs and `--version` output.
pub const CRATE_NAME: &str = "ds-daemon";

/// Crate version, read from `Cargo.toml` at compile time.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_smoke() {
        assert_eq!(CRATE_NAME, "ds-daemon");
        assert!(!version().is_empty());
    }
}
