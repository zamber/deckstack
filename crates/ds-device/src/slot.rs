//! Module placement: the USB **slot** (port path relative to the base's top hub) and the
//! **connector position** derived from it — the DS-112 identity layer, grounded in the
//! SALON-001 hardware evidence (`docs/discovery/salon-001/README.md`).
//!
//! Facts pinned by the evidence:
//!
//! - slot = the USB port path **relative to the base's top hub**: the absolute sysfs path
//!   minus its leading `<bus>-<rootport>` prefix, e.g. abs `3-2-1-4-3` → slot `1-4-3`.
//!   The base MCU itself sits at slot `1-4-4` ([`BASE_MCU_SLOT`]).
//! - every physical connector exposes a *different* slot per module rotation, so
//!   `(slot, orientation)` — not the serial alone — is the stable identity key of a
//!   seated module ([`DeviceKey`]).
//! - the observed connector port paths are 2-segment (`2-1` … `4-4`) and 3-segment
//!   (`1-1-1` … `1-4-3`) paths with port numbers 1..=4; 26 physical connectors plus the
//!   base MCU (`upstream/PROTOCOL.md:109-127`, SALON-001 sweeps).
//!
//! [`Slot::position`] decodes the port path into the connector index: the path read as a
//! base-4 number over its (1-based) port digits, tagged by the segment count so paths of
//! different lengths can never collide:
//!
//! ```text
//! position = 4^n + Σ_i (d_i − 1) · 4^(n−i)      (n = segment count, d_i = port digit)
//! ```
//!
//! e.g. `1-4-3` → 64 + 0·16 + 3·4 + 2 = 78, `4-4` → 16 + 12 + 3 = 31, base `1-4-4` → 79.

use std::ffi::CStr;
use std::fmt;

use ds_protocol::Orientation;

/// Slot of the base MCU itself (abs `3-2-1-4-4` → `1-4-4`).
pub const BASE_MCU_SLOT: &str = "1-4-4";

/// Feature id the **virtual** device answers with a module's current slot (ASCII, like the
/// serial read) — `ds-vdevctl move` changes it. A vdev-only read: on real HID the slot
/// comes from the sysfs port path, never from a feature query.
pub const SLOT_FEATURE_ID: u8 = 0x39;

/// Maximum port-path depth observed on the MasterHub top hub (3 segments).
pub const MAX_SLOT_DEPTH: usize = 3;

/// One USB slot: the port path relative to the base's top hub, e.g. `1-4-3`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Slot(String);

impl Slot {
    /// Parse a relative port path (`1-4-3` style): 1..=3 segments of 1..=4.
    pub fn parse(s: &str) -> Option<Slot> {
        let mut digits = Vec::new();
        for seg in s.split('-') {
            if seg.is_empty() || seg.len() > 1 {
                return None;
            }
            let d: u8 = seg.parse().ok()?;
            if !(1..=4).contains(&d) {
                return None;
            }
            digits.push(d);
        }
        if digits.is_empty() || digits.len() > MAX_SLOT_DEPTH {
            return None;
        }
        Some(Slot(s.to_string()))
    }

    /// The slot of an absolute sysfs port path: strip the leading `<bus>-<rootport>`
    /// prefix (abs `3-2-1-4-3` → `1-4-3`). `None` when the path has no relative part.
    pub fn from_abs_path(abs: &str) -> Option<Slot> {
        let parts: Vec<&str> = abs.split('-').collect();
        if parts.len() <= 2 {
            return None;
        }
        Slot::parse(&parts[2..].join("-"))
    }

    /// The slot of a hidapi (libusb backend) device path: `<bus>-<ports…>:<config>.<iface>`
    /// (e.g. `3-2.3.4:1.0`) — take the part before `:`, turn the `.` separators into `-`,
    /// then strip the `<bus>-<rootport>` prefix.
    pub fn from_hidapi_path(path: &CStr) -> Option<Slot> {
        let s = path.to_string_lossy();
        let dev = s.split(':').next()?;
        Slot::from_abs_path(&dev.replace('.', "-"))
    }

    /// The connector index of this slot within the MasterHub connector grid (see the
    /// module doc for the formula). Collision-free across path lengths; the base MCU
    /// slot decodes to its own index (79).
    pub fn position(&self) -> u8 {
        let digits: Vec<u8> = self
            .0
            .split('-')
            .map(|s| s.parse::<u8>().unwrap_or(1))
            .collect();
        let n = digits.len() as u32;
        let mut pos: u32 = 4u32.pow(n);
        for (i, d) in digits.iter().enumerate() {
            pos += u32::from(d - 1) * 4u32.pow(n - 1 - i as u32);
        }
        pos as u8
    }

    /// The slot as the port-path string (`1-4-3`).
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this slot is the base MCU's own (`1-4-4`).
    pub fn is_base_mcu(&self) -> bool {
        self.0 == BASE_MCU_SLOT
    }
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The DS-112 identity key of a seated module: **(slot, orientation)** in addition to the
/// USB serial. A reseated or rotated module remaps to the same logical position because
/// its key follows the connector, not the enumeration order.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceKey {
    /// USB slot (port path relative to the base's top hub).
    pub slot: Slot,
    /// Base orientation at seating time (`horizontal` flat = 0x01, `vertical` upright =
    /// 0x00 — a one-direction tilt switch, SALON-001 finding 1).
    pub orientation: Orientation,
}

impl DeviceKey {
    /// The key of a module at `slot` under base `orientation`.
    pub fn new(slot: Slot, orientation: Orientation) -> Self {
        DeviceKey { slot, orientation }
    }

    /// The connector index of the key's slot.
    pub fn position(&self) -> u8 {
        self.slot.position()
    }
}

impl fmt::Display for DeviceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let o = match self.orientation {
            Orientation::Horizontal => "horizontal",
            Orientation::Vertical => "vertical",
        };
        write!(f, "{}@{}", self.slot, o)
    }
}

/// The wire spelling of an orientation (the `deck/device/<serial>/orientation` payload and
/// the `/devices/<serial>/orientation` state field).
pub fn orientation_name(o: Orientation) -> &'static str {
    match o {
        Orientation::Horizontal => "horizontal",
        Orientation::Vertical => "vertical",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_salon001_slots() {
        for s in [
            "1-4-3", "1-4-4", "3-1", "1-1-1", "3-3", "2-3", "4-4", "1-2-3", "1-3-4", "2-2",
            "4-1", "2-4", "1-4-2", "1-1-4", "3-2", "4-3", "1-2-1",
        ] {
            assert!(Slot::parse(s).is_some(), "{s}");
        }
        for bad in ["", "0-1", "5-1", "1-2-3-4", "1--2", "a-b", "1-", "-1", "12-3"] {
            assert!(Slot::parse(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn abs_path_strips_bus_and_rootport() {
        // SALON-001: abs 3-2-1-4-3 -> slot 1-4-3; base MCU abs 3-2-1-4-4 -> 1-4-4
        assert_eq!(
            Slot::from_abs_path("3-2-1-4-3").unwrap().as_str(),
            "1-4-3"
        );
        assert_eq!(
            Slot::from_abs_path("3-2-1-4-4").unwrap().as_str(),
            BASE_MCU_SLOT
        );
        assert_eq!(Slot::from_abs_path("3-2-4-4").unwrap().as_str(), "4-4");
        assert!(Slot::from_abs_path("3-2").is_none());
        assert!(Slot::from_abs_path("").is_none());
    }

    #[test]
    fn hidapi_path_maps_to_the_same_slot() {
        let p = std::ffi::CString::new("3-2.1.4.3:1.0").unwrap();
        assert_eq!(Slot::from_hidapi_path(&p).unwrap().as_str(), "1-4-3");
        let base = std::ffi::CString::new("3-2.1.4.4:1.0").unwrap();
        let slot = Slot::from_hidapi_path(&base).unwrap();
        assert_eq!(slot.as_str(), BASE_MCU_SLOT);
        assert!(slot.is_base_mcu());
    }

    #[test]
    fn position_is_collision_free_and_stable() {
        let mut seen = std::collections::HashSet::new();
        // every observed SALON-001 connector slot plus the base MCU
        let all = [
            "1-1-1", "1-1-2", "1-1-3", "1-1-4", "1-2-1", "1-2-2", "1-2-3", "1-2-4", "1-3-1",
            "1-3-2", "1-3-3", "1-3-4", "1-4-1", "1-4-2", "1-4-3", "1-4-4", "2-1", "2-2", "2-3",
            "2-4", "3-1", "3-2", "3-3", "3-4", "4-1", "4-2", "4-3", "4-4",
        ];
        for s in all {
            let slot = Slot::parse(s).unwrap();
            assert!(seen.insert(slot.position()), "position collision at {s}");
        }
        assert_eq!(Slot::parse("1-4-3").unwrap().position(), 78);
        assert_eq!(Slot::parse("4-4").unwrap().position(), 31);
        assert_eq!(Slot::parse("1-4-4").unwrap().position(), 79);
        assert_eq!(Slot::parse("2-3").unwrap().position(), 22);
    }

    #[test]
    fn device_key_pairs_slot_and_orientation() {
        let slot = Slot::parse("1-4-3").unwrap();
        let k = DeviceKey::new(slot.clone(), Orientation::Horizontal);
        assert_eq!(k.position(), 78);
        assert_eq!(k.to_string(), "1-4-3@horizontal");
        let k2 = DeviceKey::new(slot, Orientation::Vertical);
        assert_ne!(k, k2, "rotation changes the identity key");
        assert_eq!(orientation_name(Orientation::Vertical), "vertical");
        assert_eq!(orientation_name(Orientation::Horizontal), "horizontal");
    }
}
