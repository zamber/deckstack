//! Errors of the driver core.

use std::io;
use std::time::Duration;

use crate::chunks::ReassemblyError;

/// Everything that can go wrong talking to "a device" (virtual now, real HID in DS-021).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error(
        "malformed device URI '{0}' (expected '<scheme>:<rest>', e.g. virtual:/tmp/vdev.sock)"
    )]
    BadUri(String),
    #[error("device scheme '{0}' is not supported by this build (hid: lands with DS-021)")]
    UnsupportedScheme(String),
    #[error("malformed frame on the transport: {0}")]
    BadFrame(String),
    #[error("refusing a forbidden report (firmware, serial number or factory command)")]
    Forbidden,
    #[error("timed out after {0:?} waiting for the device")]
    Timeout(Duration),
    #[error("the device closed the connection")]
    Closed,
    #[error("the device reported an error: {0}")]
    Device(String),
    #[error("unknown module product id {0:#06x}")]
    UnknownPid(u16),
    #[error("chunk reassembly failed: {0}")]
    Reassembly(#[from] ReassemblyError),
}

/// Result type of the driver core.
pub type Result<T> = std::result::Result<T, Error>;
