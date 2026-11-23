//! `ds-vdev` — a software MasterHub at the HID-report level, so the real `ds-device` code
//! path (chunking, framing, safety) runs unchanged in CI and development (R10, ADR-015).
//!
//! Component spec: `docs/components/virtual-device.md`. This crate is the server side of the
//! socket protocol pinned there (§"Socket protocol"):
//!
//! - [`server`] — binds the device socket (`ds-vdev --socket /tmp/vdev.sock`) and a sibling
//!   `<socket>.ctl` control socket, one thread per host connection;
//! - [`hub`] — the report-level state: module reports for the deck layout, chunked
//!   image/color pushes reassembled per target with sha256, per-target latest frame, the
//!   forbidden-report refusal, and the throttle/drop/credit knobs;
//! - [`throttle`] — the throttle/backpressure knobs themselves (DS-035): kbps shaping of
//!   acks, `--drop-after` unplug, and the seeded credit-exhaustion window;
//! - [`inject`] — scripted input events (ndjson) broadcast to hosts as `0x81` INPUT frames;
//! - [`ctl`] — the client side of the control socket, used by `ds-vdevctl`.
//!
//! The frame codec, report view, chunk reassembler and module descriptors are imported from
//! `ds-device`, never reimplemented, so host and virtual device cannot drift.
//!
//! ```no_run
//! use ds_vdev::{HubConfig, VdevServer};
//!
//! let server = VdevServer::start("/tmp/vdev.sock", HubConfig::default()).unwrap();
//! println!("serving on {}", server.socket_path().display());
//! // ds-device connects with DeviceUri::parse("virtual:/tmp/vdev.sock")
//! ```

pub mod ctl;
pub mod hub;
pub mod inject;
pub mod server;
pub mod target;
pub mod throttle;

pub use hub::{sha256_hex, sha256_of, FrameEntry, Hub, HubConfig, Outcome, Throttle, WriteRecord};
pub use inject::{encode_input_report, event_frame, parse_events_ndjson, InjectEvent};
pub use server::{control_path, VdevServer};
pub use target::Target;
pub use throttle::{CreditState, CreditWindow};

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
    fn crate_metadata() {
        assert_eq!(CRATE_NAME, "ds-vdev");
        assert!(!version().is_empty());
    }
}
