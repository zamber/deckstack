//! The device-agnostic transport trait and URI-based opening.
//!
//! Everything the daemon needs to talk to "a device" before real HID exists: open by URI
//! (`virtual:<path>` now, `hid:` in DS-021), send reports, run feature queries, and receive
//! what the reader thread decoded. Implementations: [`crate::vtransport::VirtualTransport`] now;
//! the mh-device/HID one lands as a new impl in DS-021, not as a refactor of this trait.

use std::time::Duration;

use ds_protocol::{Input, Report};
use mh_device::ModuleId;

use crate::error::{Error, Result};
use crate::uri::DeviceUri;
use crate::vtransport::VirtualTransport;

/// What the reader thread of a transport decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceEvent {
    /// An input report arrived (a key press, fader move, dial turn, … — or one injected by
    /// ds-vdev's `--inject`).
    Input {
        /// The module that sent it.
        module: ModuleId,
        /// The decoded input.
        input: Input,
    },
    /// The device went away (socket closed / unplugged).
    Disconnected,
}

/// A connected device: reports out, events in.
///
/// All methods take `&self` (interior mutability), so one transport can be shared between the
/// writer path and the reader path of the daemon.
pub trait Transport: Send + Sync + 'static {
    /// Send one report to a module; returns the byte count the device accepted (the
    /// `hid_write` return). Refuses forbidden reports with [`Error::Forbidden`] before any
    /// byte leaves the host.
    fn send(&self, module: &ModuleId, report: &Report) -> Result<usize>;

    /// Feature query (`hid_get_feature_report`): send `feature_id`, return the answer bytes
    /// (report id first). Keys/dial must be in software mode first — that rule is enforced by
    /// the [`ds_protocol::SoftwareMode`] proof token on the caller side (DS-022).
    fn query(&self, module: &ModuleId, feature_id: u8, timeout: Duration) -> Result<Vec<u8>>;

    /// Wait for the next event from the reader thread (input reports, disconnect).
    fn recv(&self, timeout: Duration) -> Result<DeviceEvent>;

    /// Close the connection (handback on real hardware is DS-024).
    fn close(&self) -> Result<()>;
}

/// Open the device a URI addresses.
///
/// `virtual:<path>` connects to the ds-vdev Unix socket at `<path>`; `hid:` parses but returns
/// [`Error::UnsupportedScheme`] until DS-021 lands the real HID transport.
pub fn open(uri: &DeviceUri) -> Result<Box<dyn Transport>> {
    match uri.scheme() {
        "virtual" => {
            let path = uri
                .virtual_path_buf()
                .ok_or_else(|| Error::BadUri(uri.to_string()))?;
            Ok(Box::new(VirtualTransport::connect(&path)?))
        }
        other => Err(Error::UnsupportedScheme(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_protocol::ModuleKind;

    #[test]
    fn virtual_uris_open_a_virtual_transport() {
        let dir = std::env::temp_dir().join(format!("ds-device-uri-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("vdev.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let uri = DeviceUri::parse(&format!("virtual:{}", sock.display())).unwrap();
        let t = open(&uri).unwrap();
        t.close().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hid_and_unknown_schemes_are_refused() {
        for s in ["hid:", "hid:1-2", "usb:/dev/x"] {
            let uri = DeviceUri::parse(s).unwrap();
            assert!(
                matches!(open(&uri), Err(Error::UnsupportedScheme(_))),
                "{s}"
            );
        }
    }

    #[test]
    fn module_id_carries_the_kind() {
        let id = mh_device::ModuleId {
            kind: ModuleKind::Dial,
            serial: "S".into(),
        };
        assert_eq!(id.kind, ModuleKind::Dial);
    }
}
