//! `ds-protocol` — MasterHub wire-protocol builders and safety rules; re-exports the mh-protocol crate (ADR-010).
//!
//! Component spec: `docs/components/device-driver.md`. This crate is a stub from the DS-004
//! workspace skeleton: it has no dependencies and no behaviour yet. The real implementation
//! lands with the phase-1 ticket that owns this component (DS-010 wires in mh-protocol).

/// Crate name, for logs and `--version` output.
pub const CRATE_NAME: &str = "ds-protocol";

/// Crate version, read from `Cargo.toml` at compile time.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_smoke() {
        assert_eq!(CRATE_NAME, "ds-protocol");
        assert!(!version().is_empty());
    }
}
