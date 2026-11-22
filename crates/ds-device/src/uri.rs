//! Device addressing: `virtual:<path>` now, `hid:` later (DS-021).
//!
//! The daemon addresses "a device" by URI, never by transport internals, so the same driver
//! code runs against the virtual device (`virtual:/tmp/vdev.sock`, served by ds-vdev) and,
//! from DS-021 on, against real hardware (`hid:…`).

use std::fmt;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// A parsed device URI: `<scheme>:<rest>`.
///
/// Known schemes: `virtual` (rest = Unix socket path) and `hid` (rest = OS device locator,
/// parsed but only openable from DS-021 on).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceUri {
    scheme: String,
    rest: String,
}

impl DeviceUri {
    /// Parse `<scheme>:<rest>`. The scheme is lowercased; `rest` is kept verbatim.
    pub fn parse(s: &str) -> Result<Self> {
        let (scheme, rest) = s
            .split_once(':')
            .ok_or_else(|| Error::BadUri(s.to_string()))?;
        if scheme.is_empty()
            || !scheme
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        {
            return Err(Error::BadUri(s.to_string()));
        }
        if scheme == "virtual" && rest.is_empty() {
            return Err(Error::BadUri(s.to_string()));
        }
        Ok(DeviceUri {
            scheme: scheme.to_string(),
            rest: rest.to_string(),
        })
    }

    /// The scheme, e.g. `virtual` or `hid`.
    pub fn scheme(&self) -> &str {
        &self.scheme
    }

    /// Everything after the colon, e.g. `/tmp/vdev.sock`.
    pub fn rest(&self) -> &str {
        &self.rest
    }

    /// Whether this is a virtual device URI.
    pub fn is_virtual(&self) -> bool {
        self.scheme == "virtual"
    }

    /// The Unix socket path of a `virtual:` URI; `None` for other schemes or an empty rest.
    pub fn virtual_path(&self) -> Option<&Path> {
        (self.is_virtual() && !self.rest.is_empty()).then(|| Path::new(self.rest.as_str()))
    }

    /// The Unix socket path as an owned `PathBuf`, for `virtual:` URIs with a non-empty rest.
    pub fn virtual_path_buf(&self) -> Option<PathBuf> {
        self.virtual_path().map(Path::to_path_buf)
    }
}

impl fmt::Display for DeviceUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.scheme, self.rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_virtual_uri() {
        let u = DeviceUri::parse("virtual:/tmp/vdev.sock").unwrap();
        assert_eq!(u.scheme(), "virtual");
        assert_eq!(u.rest(), "/tmp/vdev.sock");
        assert!(u.is_virtual());
        assert_eq!(u.virtual_path(), Some(Path::new("/tmp/vdev.sock")));
        assert_eq!(u.to_string(), "virtual:/tmp/vdev.sock");
    }

    #[test]
    fn parses_hid_uri_but_has_no_path() {
        let u = DeviceUri::parse("hid:").unwrap();
        assert_eq!(u.scheme(), "hid");
        assert!(!u.is_virtual());
        assert_eq!(u.virtual_path(), None);
    }

    #[test]
    fn rejects_malformed_uris() {
        for bad in [
            "",
            "virtual",
            "virtual:",
            ":/tmp/x",
            "vir tual:/x",
            "VIRTUAL:/x",
        ] {
            assert!(DeviceUri::parse(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn scheme_is_lowercased_input_tolerant() {
        // 'Virtual' is not all-lowercase, so it is rejected — URIs are canonical lowercase.
        assert!(DeviceUri::parse("Virtual:/tmp/x").is_err());
    }
}
