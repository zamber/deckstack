//! Module report codec: classify and decode raw report bytes (report id first) as they travel
//! the transport, and pad reports to the module's report lengths exactly like `hid_write` /
//! `hid_send_feature_report` do.
//!
//! Report layouts (`upstream/PROTOCOL.md`): input `01 <tag> <len u16 LE> <payload…>`,
//! image chunks `02 <cmd> <chunk u16 LE> <len u16 LE> <index> <last> <data…>`, other output
//! reports `02 <cmd> …`, feature reports `03 <cmd> …`.

use ds_protocol::{Report, ReportKind};

use crate::module::ModuleInfo;

/// Image transfer commands: `0x35` panel (keys), `0x36` one key, `0x45` dial screen.
pub const IMAGE_COMMANDS: [u8; 3] = [0x35, 0x36, 0x45];
/// Whole-panel image on the keys module (`02 35 …`).
pub const PANEL_CMD: u8 = 0x35;
/// One key's 72×72 image (`02 36 …`).
pub const KEY_CMD: u8 = 0x36;
/// Dial screen image (`02 45 …`).
pub const DIAL_CMD: u8 = 0x45;
/// Bytes in front of the payload of an image chunk: `02 cmd chunk(LE16) len(LE16) index last`.
pub const CHUNK_HEADER_LEN: usize = ds_protocol::CHUNK_HEADER_LEN;

/// One decoded image chunk header plus its payload slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageChunk<'a> {
    /// Image command (`0x35`/`0x36`/`0x45`).
    pub cmd: u8,
    /// Sequence number of this chunk within the transfer (`u16 LE` on the wire).
    pub chunk: u16,
    /// Declared payload length (`u16 LE` on the wire).
    pub len: u16,
    /// Target index: key number for `0x36`, `0` for panel/dial.
    pub index: u8,
    /// `1` on the last chunk of the transfer.
    pub last: bool,
    /// Payload bytes actually present in the report (may be shorter than `len` if truncated).
    pub data: &'a [u8],
}

/// A classified view of one raw report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportView<'a> {
    /// Input report: `01 <tag> <len u16 LE> <payload…>`.
    Input { tag: u8, payload: &'a [u8] },
    /// Image chunk: `02 <cmd> <chunk> <len> <index> <last> <data…>`.
    ImageChunk(ImageChunk<'a>),
    /// Other output report: `02 <cmd> …`.
    Output { cmd: u8, rest: &'a [u8] },
    /// Feature report: `03 <cmd> …`.
    Feature { cmd: u8, rest: &'a [u8] },
    /// Anything else (including an empty buffer).
    Other,
}

/// Classify one raw report (report id included).
pub fn view_report(raw: &[u8]) -> ReportView<'_> {
    match raw.first() {
        None => ReportView::Other,
        Some(0x01) if raw.len() >= 4 => {
            let tag = raw[1];
            let len = u16::from_le_bytes([raw[2], raw[3]]) as usize;
            let payload = &raw[4..(4 + len).min(raw.len())];
            ReportView::Input { tag, payload }
        }
        Some(0x02) if raw.len() >= 2 => {
            let cmd = raw[1];
            if IMAGE_COMMANDS.contains(&cmd) && raw.len() >= CHUNK_HEADER_LEN {
                let chunk = u16::from_le_bytes([raw[2], raw[3]]);
                let len = u16::from_le_bytes([raw[4], raw[5]]);
                let index = raw[6];
                let last = raw[7] != 0;
                let data = &raw[CHUNK_HEADER_LEN..(CHUNK_HEADER_LEN + len as usize).min(raw.len())];
                ReportView::ImageChunk(ImageChunk {
                    cmd,
                    chunk,
                    len,
                    index,
                    last,
                    data,
                })
            } else {
                ReportView::Output {
                    cmd,
                    rest: &raw[2..],
                }
            }
        }
        Some(0x03) if raw.len() >= 2 => ReportView::Feature {
            cmd: raw[1],
            rest: &raw[2..],
        },
        Some(_) => ReportView::Other,
    }
}

/// Pad a report to the module's report length, exactly as the HID layer does before
/// `hid_write` (output reports to [`ModuleInfo::out_len`], feature reports to
/// [`ModuleInfo::feature_len`]). Longer reports are passed through untouched.
pub fn pad_report(info: &ModuleInfo, report: &Report) -> Vec<u8> {
    let target = match report.kind() {
        ReportKind::Output => info.out_len,
        ReportKind::Feature => info.feature_len,
    };
    let bytes = report.bytes();
    if bytes.len() >= target {
        return bytes.to_vec();
    }
    let mut padded = bytes.to_vec();
    padded.resize(target, 0);
    padded
}

/// Defense in depth on the raw path: refuse forbidden reports before any byte leaves the host
/// (`upstream/crates/mh-protocol/src/safety.rs`). Builder output never trips this.
pub fn ensure_allowed(bytes: &[u8]) -> crate::error::Result<()> {
    if ds_protocol::is_forbidden(bytes) {
        return Err(crate::error::Error::Forbidden);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::module_info;
    use ds_protocol::ModuleKind;

    #[test]
    fn decodes_an_image_chunk_header() {
        let mut raw = vec![0x02, 0x36, 0x02, 0x00, 0x04, 0x00, 0x03, 0x00];
        raw.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
        let ReportView::ImageChunk(c) = view_report(&raw) else {
            panic!("not a chunk")
        };
        assert_eq!(c.cmd, KEY_CMD);
        assert_eq!(c.chunk, 2);
        assert_eq!(c.len, 4);
        assert_eq!(c.index, 3);
        assert!(!c.last);
        assert_eq!(c.data, &[0xAA, 0xBB, 0xCC, 0xDD]);
    }

    #[test]
    fn decodes_input_output_feature_and_junk() {
        let ReportView::Input { tag, payload } =
            view_report(&[0x01, 0x34, 0x03, 0x00, 0x02, 0x0F, 0x01])
        else {
            panic!("not input")
        };
        assert_eq!((tag, payload), (0x34, &[0x02, 0x0F, 0x01][..]));
        let ReportView::Output { cmd, rest } =
            view_report(&[0x02, 0x2D, 0x00, 0x00, 0xFF, 1, 2, 3, 0])
        else {
            panic!("not output")
        };
        assert_eq!((cmd, rest), (0x2D, &[0x00, 0x00, 0xFF, 1, 2, 3, 0][..]));
        let ReportView::Feature { cmd, rest } = view_report(&[0x03, 0x3A, 0x01]) else {
            panic!("not feature")
        };
        assert_eq!((cmd, rest), (0x3A, &[0x01][..]));
        assert_eq!(view_report(&[]), ReportView::Other);
        assert_eq!(view_report(&[0x09, 0x00]), ReportView::Other);
    }

    #[test]
    fn chunk_payload_is_clamped_to_what_is_present() {
        let raw = [0x02, 0x35, 0x00, 0x00, 0x64, 0x00, 0x00, 0x00, 0x11];
        let ReportView::ImageChunk(c) = view_report(&raw) else {
            panic!("not a chunk")
        };
        assert_eq!(c.len, 100);
        assert_eq!(c.data, &[0x11]);
    }

    #[test]
    fn pads_like_the_hid_layer() {
        let keys = module_info(ModuleKind::Keys);
        let dial = module_info(ModuleKind::Dial);
        let faders = module_info(ModuleKind::Faders);
        let (handshake, _mode) = ds_protocol::enter_software_mode(ModuleKind::Keys).unwrap();
        assert_eq!(pad_report(&keys, &handshake).len(), 32); // feature report -> FEATURE_LEN
        let led =
            ds_protocol::led_color(ModuleKind::Faders, ds_protocol::LedTarget::All, [1, 2, 3])
                .unwrap();
        assert_eq!(pad_report(&faders, &led).len(), 64);
        let img = ds_protocol::panel_image(&[0xFF; 10], keys.out_len);
        assert_eq!(pad_report(&keys, &img[0]).len(), 320);
        assert_eq!(pad_report(&dial, &img[0]).len(), 320);
        // 400 bytes split into 312-byte chunks, each report still padded to exactly 320
        let long = ds_protocol::panel_image(&[0xFF; 400], 320);
        assert_eq!(long.len(), 2);
        for r in &long {
            assert_eq!(pad_report(&keys, r).len(), 320);
        }
    }

    #[test]
    fn forbidden_bytes_are_refused_on_the_raw_path() {
        assert!(ensure_allowed(&[0x02, 0x2A, 0x00, 0x00]).is_err());
        assert!(ensure_allowed(&[0x03, 0x35, b'M', b'H']).is_err());
        assert!(ensure_allowed(&[0x02, 0x36, 0x00, 0x00]).is_ok());
        assert!(ensure_allowed(&[0x03, 0x3A, 0x01]).is_ok());
    }
}
