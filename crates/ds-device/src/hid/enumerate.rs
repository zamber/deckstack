//! Enumeration of the Cooler Master MasterHub modules by USB vendor/product id (DS-021).
//!
//! The vendor id is fixed (`0x2516`, [`crate::module::VENDOR_ID`]) and every module kind has
//! its own product id ([`crate::module::module_info`]); anything else is not a deck module and
//! is ignored. Enumeration itself needs no device permissions on Linux: the libusb backend
//! reads the descriptors from sysfs, so a plain user can list the attached modules (opening
//! them may still need the udev rule of DS-025).

use std::ffi::{CStr, CString};

use hidapi::HidApi;
use mh_device::ModuleId;

use crate::error::{Error, Result};
use crate::module::{module_info_for_pid, ModuleInfo, VENDOR_ID};
use crate::slot::Slot;

/// One attached MasterHub module, as enumeration sees it.
#[derive(Debug, Clone)]
pub struct HidModuleInfo {
    /// The module descriptor (kind, pid, report lengths, input tag).
    pub info: ModuleInfo,
    /// OS device path to open (with the libusb backend: the sysfs-style
    /// `<bus>-<ports…>:<config>.<interface>`, e.g. `3-2.3.4:1.0`).
    pub path: CString,
    /// USB serial string, when the device could be opened during enumeration (empty otherwise).
    pub serial: Option<String>,
    /// USB product string, when available.
    pub product_string: Option<String>,
    /// The DS-112 placement: the USB slot (port path relative to the base's top hub, e.g.
    /// `1-4-3`) derived from the OS path. `None` when the path carries no port part.
    pub slot: Option<Slot>,
    /// The [`ModuleId`] the transport addresses this module by: the USB serial when there is
    /// one, the OS path otherwise (a wedged keys/dial module reports a garbage serial, so the
    /// path is the fallback that always identifies the physical module).
    pub module_id: ModuleId,
}

/// Map a vendor/product id pair to a module descriptor; `None` for anything that is not a
/// MasterHub module.
pub fn module_info_for_vid_pid(vid: u16, pid: u16) -> Option<ModuleInfo> {
    (vid == VENDOR_ID)
        .then(|| module_info_for_pid(pid))
        .flatten()
}

/// The [`ModuleId`] of an enumerated module: USB serial if present, else the OS path.
pub fn module_id_for(info: &ModuleInfo, serial: Option<&str>, path: &CStr) -> ModuleId {
    let serial = serial
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    ModuleId {
        kind: info.kind,
        serial,
    }
}

/// The DS-112 slot of an enumerated module: the port path relative to the base's top hub,
/// derived from the hidapi path (`3-2.1.4.3:1.0` → `1-4-3`). Linux has no locationID
/// (`upstream/crates/mh-device/src/location.rs:144-149` TODO), so the sysfs-style path is
/// the placement signal.
pub fn slot_for_path(path: &CStr) -> Option<Slot> {
    Slot::from_hidapi_path(path)
}

/// Map a hidapi error onto the driver error type.
pub fn hid_error(e: hidapi::HidError) -> Error {
    Error::Device(format!("hidapi: {e}"))
}

/// Enumerate the attached MasterHub modules (`VID 0x2516`, any known pid).
///
/// Returns an empty list when no deck is attached — that is a state, not an error.
pub fn enumerate() -> Result<Vec<HidModuleInfo>> {
    let mut api = HidApi::new().map_err(hid_error)?;
    enumerate_with(&mut api)
}

/// Same, on an existing api context (so `open_all` enumerates and opens with one scan).
pub(crate) fn enumerate_with(api: &mut HidApi) -> Result<Vec<HidModuleInfo>> {
    api.reset_devices().map_err(hid_error)?;
    api.add_devices(VENDOR_ID, 0).map_err(hid_error)?;
    let mut out = Vec::new();
    for d in api.device_list() {
        let Some(info) = module_info_for_vid_pid(d.vendor_id(), d.product_id()) else {
            continue;
        };
        out.push(HidModuleInfo {
            info,
            path: d.path().to_owned(),
            serial: d.serial_number().map(str::to_string),
            product_string: d.product_string().map(str::to_string),
            slot: slot_for_path(d.path()),
            module_id: module_id_for(&info, d.serial_number(), d.path()),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_protocol::ModuleKind;

    #[test]
    fn every_module_pid_maps_back_to_its_kind() {
        // The five modules attached to the DS-021 smoke machine plus faders.
        for (pid, kind) in [
            (0x01F6, ModuleKind::Base),
            (0x01FE, ModuleKind::Keys),
            (0x0200, ModuleKind::Faders),
            (0x0202, ModuleKind::Knobs),
            (0x0204, ModuleKind::Rollers),
            (0x0206, ModuleKind::Dial),
        ] {
            let info = module_info_for_vid_pid(VENDOR_ID, pid).unwrap();
            assert_eq!(info.kind, kind);
            assert_eq!(info.pid, pid);
        }
    }

    #[test]
    fn non_masterhub_devices_are_ignored() {
        assert_eq!(VENDOR_ID, 0x2516);
        assert!(
            module_info_for_vid_pid(0x1462, 0x0202).is_none(),
            "wrong vendor"
        );
        assert!(
            module_info_for_vid_pid(VENDOR_ID, 0x9999).is_none(),
            "unknown pid"
        );
        assert!(module_info_for_vid_pid(0x0000, 0x0000).is_none());
    }

    #[test]
    fn module_id_falls_back_to_the_os_path_without_a_serial() {
        let info = module_info_for_vid_pid(VENDOR_ID, 0x0206).unwrap();
        let path = CString::new("/dev/bus/usb/003/016").unwrap();
        let with_serial = module_id_for(&info, Some("MHKB03AA221252301767"), &path);
        assert_eq!(with_serial.serial, "MHKB03AA221252301767");
        assert_eq!(with_serial.kind, ModuleKind::Dial);
        let without = module_id_for(&info, None, &path);
        assert_eq!(without.serial, "/dev/bus/usb/003/016");
        let empty = module_id_for(&info, Some(""), &path);
        assert_eq!(empty.serial, "/dev/bus/usb/003/016");
    }

    #[test]
    fn slot_is_derived_from_the_sysfs_style_path() {
        // SALON-001: abs 3-2-1-4-3 -> slot 1-4-3 (knobs at top-right rot90)
        let knobs = CString::new("3-2.1.4.3:1.0").unwrap();
        assert_eq!(slot_for_path(&knobs).unwrap().as_str(), "1-4-3");
        // base MCU: abs 3-2-1-4-4 -> 1-4-4
        let base = CString::new("3-2.1.4.4:1.0").unwrap();
        let slot = slot_for_path(&base).unwrap();
        assert_eq!(slot.as_str(), crate::slot::BASE_MCU_SLOT);
        assert_eq!(slot.position(), 79);
        // a non-port path has no slot
        let dev = CString::new("/dev/bus/usb/003/016").unwrap();
        assert!(slot_for_path(&dev).is_none());
    }
}
