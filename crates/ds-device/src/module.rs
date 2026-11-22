//! Module descriptors: the fixed per-module facts the report codec needs (product id, output
//! and feature report lengths, input report tag). Mirrors the HID descriptors read on hardware
//! (`upstream/PROTOCOL.md` "Reports"): output `0x02` is 320 B on the screen modules and 64 B on
//! the M0 family, feature `0x03` is 32 B everywhere, input `0x01` is 64 B.

use ds_protocol::ModuleKind;

/// USB vendor id of every MasterHub module.
pub const VENDOR_ID: u16 = ds_protocol::VENDOR_ID;

/// Fixed facts of one module kind, as its HID descriptor declares them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModuleInfo {
    /// Module kind.
    pub kind: ModuleKind,
    /// USB product id (vendor id is [`VENDOR_ID`]).
    pub pid: u16,
    /// Output report length in bytes, report id included (what `hid_write` expects).
    pub out_len: usize,
    /// Feature report length in bytes, report id included.
    pub feature_len: usize,
    /// Tag of the module's input reports (`0x01 <tag> …`).
    pub input_tag: u8,
}

/// Descriptor of one module kind.
pub fn module_info(kind: ModuleKind) -> ModuleInfo {
    match kind {
        ModuleKind::Base => ModuleInfo {
            kind,
            pid: 0x01F6,
            out_len: 64,
            feature_len: 32,
            input_tag: 0x2B,
        },
        ModuleKind::Keys => ModuleInfo {
            kind,
            pid: 0x01FE,
            out_len: 320,
            feature_len: 32,
            input_tag: 0x34,
        },
        ModuleKind::Faders => ModuleInfo {
            kind,
            pid: 0x0200,
            out_len: 64,
            feature_len: 32,
            input_tag: 0x2A,
        },
        ModuleKind::Knobs => ModuleInfo {
            kind,
            pid: 0x0202,
            out_len: 64,
            feature_len: 32,
            input_tag: 0x2A,
        },
        ModuleKind::Rollers => ModuleInfo {
            kind,
            pid: 0x0204,
            out_len: 64,
            feature_len: 32,
            input_tag: 0x2A,
        },
        ModuleKind::Dial => ModuleInfo {
            kind,
            pid: 0x0206,
            out_len: 320,
            feature_len: 32,
            input_tag: 0x44,
        },
    }
}

/// Descriptor for a USB product id, `None` if it is not a MasterHub module.
pub fn module_info_for_pid(pid: u16) -> Option<ModuleInfo> {
    ModuleKind::ALL
        .into_iter()
        .map(module_info)
        .find(|i| i.pid == pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_round_trips_for_every_kind() {
        for kind in ModuleKind::ALL {
            let info = module_info(kind);
            assert_eq!(info.kind, kind);
            assert_eq!(info.pid, kind.product_id());
            assert_eq!(module_info_for_pid(info.pid).unwrap().kind, kind);
        }
        assert_eq!(module_info_for_pid(0x1234), None);
    }

    #[test]
    fn report_lengths_match_the_hid_descriptors() {
        assert_eq!(module_info(ModuleKind::Keys).out_len, 320);
        assert_eq!(module_info(ModuleKind::Dial).out_len, 320);
        for kind in [
            ModuleKind::Base,
            ModuleKind::Faders,
            ModuleKind::Knobs,
            ModuleKind::Rollers,
        ] {
            assert_eq!(module_info(kind).out_len, 64);
            assert_eq!(module_info(kind).feature_len, 32);
        }
        assert_eq!(module_info(ModuleKind::Keys).input_tag, 0x34);
        assert_eq!(module_info(ModuleKind::Dial).input_tag, 0x44);
        assert_eq!(module_info(ModuleKind::Faders).input_tag, 0x2A);
        assert_eq!(module_info(ModuleKind::Base).input_tag, 0x2B);
    }
}
