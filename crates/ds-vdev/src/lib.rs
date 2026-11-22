//! `ds-vdev` — a software MasterHub at the HID-report level, so the real `ds-device` code
//! path (chunking, framing, safety) runs unchanged in CI and development (R10).
//!
//! Component spec: `docs/components/virtual-device.md`. This crate is a stub from the DS-004
//! workspace skeleton: it has no dependencies and no behaviour yet. It stays a library for
//! now; the `ds-vdev`/`ds-vdevctl` binaries land with the phase-1 ticket that fills this crate.

/// Crate name, for logs and `--version` output.
pub const CRATE_NAME: &str = "ds-vdev";

/// Crate version, read from `Cargo.toml` at compile time.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_smoke() {
        assert_eq!(CRATE_NAME, "ds-vdev");
        assert!(!version().is_empty());
    }
}
