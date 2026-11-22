//! `ds-state` — state document with revisions, transactions and subscriptions; owned by the
//! daemon's state task (`docs/contracts/state.md`).
//!
//! Component spec: `docs/components/daemon.md`. This crate is a stub from the DS-004
//! workspace skeleton: it has no dependencies and no behaviour yet. The real implementation
//! lands with the phase-1 ticket that owns this component.

/// Crate name, for logs and `--version` output.
pub const CRATE_NAME: &str = "ds-state";

/// Crate version, read from `Cargo.toml` at compile time.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_smoke() {
        assert_eq!(CRATE_NAME, "ds-state");
        assert!(!version().is_empty());
    }
}
