//! `ds-device` — HID transport and driver for MasterHub decks, thin over mh-device (ADR-010),
//! with the virtual transport and the `/policy` write choke point.
//!
//! Component spec: `docs/components/device-driver.md`. This crate is a stub from the DS-004
//! workspace skeleton: it has no dependencies and no behaviour yet. The real implementation
//! lands with the phase-1 ticket that owns this component (DS-014 wires in mh-device).

/// Crate name, for logs and `--version` output.
pub const CRATE_NAME: &str = "ds-device";

/// Crate version, read from `Cargo.toml` at compile time.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_smoke() {
        assert_eq!(CRATE_NAME, "ds-device");
        assert!(!version().is_empty());
    }
}
