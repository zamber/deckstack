//! `ds-client` — client library shared by the `ds` CLI and the other senders (`ds-focus`,
//! `ds-mcp`) and agents (ADR-002).
//!
//! Component spec: `docs/components/cli.md`. This crate is a stub from the DS-004 workspace
//! skeleton: it has no dependencies and no behaviour yet. The real implementation lands with
//! the phase-1 ticket that owns this component.

/// Crate name, for logs and `--version` output.
pub const CRATE_NAME: &str = "ds-client";

/// Crate version, read from `Cargo.toml` at compile time.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_smoke() {
        assert_eq!(CRATE_NAME, "ds-client");
        assert!(!version().is_empty());
    }
}
