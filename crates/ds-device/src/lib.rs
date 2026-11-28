//! `ds-device` — the device-agnostic driver core for MasterHub decks, thin over the reused
//! upstream crates (ADR-010), plus the virtual transport so the same code runs in CI with no
//! hardware (ADR-015).
//!
//! Component spec: `docs/components/device-driver.md`. This crate is the phase-1 core of DS-014
//! plus the real HID transport of DS-021:
//!
//! - [`transport`] — the [`Transport`] trait (open by URI, send reports, feature queries,
//!   reader-thread events); [`open`] dispatches `virtual:<path>` to
//!   [`vtransport::VirtualTransport`] and `hid:`/`hid:<path>` to [`hid::HidTransport`].
//! - [`vtransport`] — the virtual transport over a Unix socket; ds-vdev (DS-017) serves the other
//!   end. The socket protocol is pinned in `docs/components/virtual-device.md`.
//! - [`hid`] — the real HID transport on Linux (DS-021): enumeration of the Cooler Master
//!   modules by VID/PID, open/close, reader threads turning input reports into
//!   [`DeviceEvent`]s, and send/query with the same semantics as the virtual transport.
//! - [`frame`] — the socket framing; [`report`] — the module report codec (classify/pad raw
//!   reports, refuse forbidden bytes); [`module`] — the per-module descriptors (pid, report
//!   lengths, input tag).
//! - [`chunks`] — reassembly of chunked image pushes with sha256 verification.
//! - [`rate`] — host-side write throttling (token bucket; LED cap default 30/s).
//! - [`handshake`] — the software-mode handshake and module init (DS-022): the required
//!   report sequence before any read on keys/dial, a type-state wrapper that refuses reads
//!   before the handshake completed, and the [`handshake::safe`] report builders (images,
//!   LED/backlight — never backlight 0 — brightness in range), every one through the
//!   forbidden-command guard.
//!
//! Out of scope here (later tickets): guard/handback (DS-024), udev rules (DS-025),
//! and ds-vdev itself (DS-017). Hot-plug lives in [`hotplug`] (DS-023).
//!
//! ```no_run
//! use ds_device::{DeviceUri, Transport};
//! use ds_protocol::{ModuleKind, Query};
//!
//! let uri = DeviceUri::parse("virtual:/tmp/vdev.sock").unwrap();
//! let dev = ds_device::open(&uri).unwrap();
//! let keys = ds_device::vtransport::virtual_module_id(ModuleKind::Keys);
//! // DS-022: handshake first — reads are refused until it completed
//! let module = ds_device::handshake::pending(&*dev, keys, Default::default())
//!     .handshake()
//!     .unwrap();
//! let firmware = module.query(Query::Firmware).unwrap();
//! ```

pub mod chunks;
pub mod error;
pub mod frame;
pub mod handshake;
pub mod hid;
pub mod hotplug;
pub mod module;
pub mod rate;
pub mod report;
pub mod slot;
pub mod transport;
pub mod uri;
pub mod vtransport;

pub use chunks::{Reassembled, Reassembler, ReassemblyError};
pub use error::{Error, Result};
pub use handshake::{
    pending, Handshake, HandshakeConfig, InitModule, InitTransport, ModuleState, PendingModule,
    Phase,
};
pub use slot::{orientation_name, DeviceKey, Slot, BASE_MCU_SLOT, SLOT_FEATURE_ID};
pub use transport::{DeviceEvent, Transport};
pub use uri::DeviceUri;
pub use vtransport::VirtualTransport;

/// Crate name, for logs and `--version` output.
pub const CRATE_NAME: &str = "ds-device";

/// Crate version, read from `Cargo.toml` at compile time.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Open the device a URI addresses.
///
/// `virtual:<path>` connects to the ds-vdev Unix socket at `<path>`; `hid:` opens every attached
/// MasterHub module and `hid:<os-path>` exactly the one at `<os-path>` (DS-021, real HID on
/// Linux). Anything else is [`Error::UnsupportedScheme`].
pub fn open(uri: &DeviceUri) -> Result<Box<dyn Transport>> {
    match uri.scheme() {
        "hid" => Ok(Box::new(hid::open_uri(uri)?)),
        _ => transport::open(uri),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_metadata() {
        assert_eq!(CRATE_NAME, "ds-device");
        assert!(!version().is_empty());
    }
}
