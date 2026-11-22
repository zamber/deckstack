//! `ds-protocol` — deckstack's single, safe surface over the MasterHub wire protocol.
//!
//! ADR-010 reuses the hardware-verified upstream [`mh_protocol`] crate as a pinned git
//! dependency; this crate re-exports only the safe parts of it. Component spec:
//! `docs/components/device-driver.md`.
//!
//! What "safe" means here, enforced by types and covered by `tests/protocol/`:
//!
//! - Forbidden HID commands (firmware update, serial rewrite, factory save/reset/recovery)
//!   are **unrepresentable**: builders exist only for safe commands and [`Report`] has no raw
//!   constructor, so no code path through this crate can even spell those bytes. The raw-path
//!   guard [`is_forbidden`] is re-exported for the transport's defense in depth.
//! - Backlight `0` is refused on every module (it wedges the screens — "off" is a black image).
//! - Reads on keys/dial require the [`SoftwareMode`] proof token, which is only minted together
//!   with the software-mode handshake report: handshake-before-read is a type-state rule.
//!
//! Upstream pinned rev: `26364e2fdcf86df6c459ea0e940f1dd5ca84cde7`
//! (`https://github.com/Przemotar/hubdeck.git`, branch `linux-bringup`, also reachable as
//! `refs/pull/14/head` on origin).
//!
//! ```rust
//! use ds_protocol::{ModuleKind, Query};
//!
//! // Handshake first: keys/dial only accept commands and answer reads in software mode.
//! let (handshake, mode) = ds_protocol::enter_software_mode(ModuleKind::Keys).unwrap();
//! assert_eq!(handshake.bytes(), &[0x03, 0x3A, 0x01]);
//! assert_eq!(ds_protocol::query_id(mode, Query::Backlight), Some(0x3C));
//! ```

use mh_protocol::report as upstream;

/// Wire protocol version pinned for the alpha: `protocol: 1` in the IPC hello/welcome
/// handshake (`docs/contracts/ipc.md` §2; breaking changes bump it).
pub const PROTOCOL_VERSION: u32 = 1;

/// Crate name, for logs and `--version` output.
pub const CRATE_NAME: &str = "ds-protocol";

/// Crate version, read from `Cargo.toml` at compile time.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

// Pure data types — safe to construct, they carry no wire bytes.
pub use mh_protocol::input::{Input, Orientation};
pub use mh_protocol::module::{ModuleKind, KEY_COLUMNS, KEY_PX, KEY_ROWS, VENDOR_ID};
pub use mh_protocol::report::{LedMode, LedTarget, Query, Rgb, CHUNK_HEADER_LEN, FEATURE_LEN};

/// Raw-path guard from upstream (`mh_protocol::safety`): the transport checks every hand-built
/// report against this before sending. Builder output never trips it — proven exhaustively in
/// `tests/protocol/safety.rs`.
pub use mh_protocol::safety::is_forbidden;

// Input decoding and answer parsing: pure functions over bytes that were already read.
pub use mh_protocol::input::parse as parse_input;
pub use mh_protocol::report::{
    feature_payload, parse_fader_values, parse_firmware, parse_orientation, parse_percent,
    parse_screen_size, parse_serial_number,
};

/// A wire report produced by one of the safe builders below.
///
/// The inner value is private and there is no constructor from raw bytes, so a forbidden command
/// (firmware update, serial rewrite, factory save/reset/recovery — see [`is_forbidden`]) cannot
/// be represented through this crate at all:
///
/// ```compile_fail
/// // no raw constructor exists:
/// let r = ds_protocol::Report::from_bytes(&[0x02, 0x2A, 0x00, 0x00]);
/// ```
///
/// ```compile_fail
/// // the field is private, so the tuple constructor is not callable outside this crate:
/// let r = ds_protocol::Report(vec![0x02, 0x2A, 0x00, 0x00]);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report(upstream::Report);

/// How a [`Report`] travels on the wire: which HID call carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportKind {
    /// `hid_write` (report id `0x02`).
    Output,
    /// `hid_send_feature_report` (report id `0x03`).
    Feature,
}

impl Report {
    /// Report bytes, report id first.
    pub fn bytes(&self) -> &[u8] {
        self.0.bytes()
    }

    /// Whether the raw-path guard flags these bytes. Always `false` for builder output; public
    /// so the transport can double-check anything that ever arrives as raw bytes.
    pub fn is_forbidden(&self) -> bool {
        self.0.is_forbidden()
    }

    /// Which HID call sends this report.
    pub fn kind(&self) -> ReportKind {
        match &self.0 {
            upstream::Report::Output(_) => ReportKind::Output,
            upstream::Report::Feature(_) => ReportKind::Feature,
        }
    }
}

/// Proof that feature reads are allowed on a module.
///
/// Keys and dial only answer reads in software mode; reading them in hardware mode wedges them
/// until a power reset (upstream `PROTOCOL.md`). For those modules the only way to mint this
/// token is [`enter_software_mode`], which returns the handshake report to send first — so
/// "handshake before read" is enforced by the type state, not by convention. The M0 family
/// (base, faders, knobs, rollers) answers at any time and mints the token without a report via
/// [`SoftwareMode::m0`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoftwareMode {
    kind: ModuleKind,
}

impl SoftwareMode {
    /// Token for an M0-family module, which has no software mode and needs no handshake.
    /// Returns `None` for keys/dial — those require [`enter_software_mode`].
    pub fn m0(kind: ModuleKind) -> Option<Self> {
        kind.is_m0().then_some(Self { kind })
    }

    /// The module this token was minted for.
    pub fn kind(self) -> ModuleKind {
        self.kind
    }
}

/// Software-mode handshake for keys/dial: the report to send (`03 3A 01` keys, `03 49 01` dial)
/// plus the [`SoftwareMode`] proof that reads may follow. `None` for the M0 family.
pub fn enter_software_mode(kind: ModuleKind) -> Option<(Report, SoftwareMode)> {
    let report = upstream::software_mode(kind, true)?;
    Some((Report(report), SoftwareMode { kind }))
}

/// Hand a keys/dial module back to its firmware (idle animation). `None` for the M0 family.
pub fn leave_software_mode(kind: ModuleKind) -> Option<Report> {
    upstream::software_mode(kind, false).map(Report)
}

/// Screen backlight, 1..=100 percent (clamped at 100). **`0` is refused on every module**:
/// backlight-0 wedges the screens (upstream `PROTOCOL.md`, AGENTS.md invariant) — "screen off"
/// is a black image, not brightness 0. `None` for modules without a screen.
pub fn backlight(kind: ModuleKind, percent: u8) -> Option<Report> {
    if percent == 0 {
        return None;
    }
    upstream::backlight(kind, percent).map(Report)
}

/// Fill the whole screen of keys/dial with one color.
pub fn fill_screen(kind: ModuleKind, rgb: Rgb) -> Option<Report> {
    upstream::fill_screen(kind, rgb).map(Report)
}

/// Fill one key (0..15) with a color; `None` for `index >= 15`.
pub fn fill_key(index: u8, rgb: Rgb) -> Option<Report> {
    upstream::fill_key(index, rgb).map(Report)
}

/// LED color, never persisted (`save=0` always — `docs/components/device-driver.md`).
pub fn led_color(kind: ModuleKind, target: LedTarget, rgb: Rgb) -> Option<Report> {
    upstream::led_color(kind, target, rgb, false).map(Report)
}

/// LED mode, never persisted (`save=0` always).
pub fn led_mode(kind: ModuleKind, target: LedTarget, mode: LedMode) -> Option<Report> {
    upstream::led_mode(kind, target, mode, false).map(Report)
}

/// LED brightness, 0..=100 (clamped at 100).
pub fn led_brightness(kind: ModuleKind, target: LedTarget, percent: u8) -> Option<Report> {
    upstream::led_brightness(kind, target, percent).map(Report)
}

/// Speed of the animated LED modes, 0..=100 (M0 family only).
pub fn led_speed(kind: ModuleKind, percent: u8) -> Option<Report> {
    upstream::led_speed(kind, percent).map(Report)
}

/// One key's 72x72 JPEG, chunked as `02 36 <chunk> <len> <index> <last> <data>`; empty if
/// `index >= 15`. `output_len` is the module's output report length including the report id
/// (320 on keys, from the HID descriptor).
pub fn key_image(index: u8, jpeg: &[u8], output_len: usize) -> Vec<Report> {
    upstream::key_image(index, jpeg, output_len)
        .into_iter()
        .map(Report)
        .collect()
}

/// One 480x272 JPEG across the whole 15-key panel (`02 35 …`).
pub fn panel_image(jpeg: &[u8], output_len: usize) -> Vec<Report> {
    upstream::panel_image(jpeg, output_len)
        .into_iter()
        .map(Report)
        .collect()
}

/// One 240x240 JPEG on the dial screen (`02 45 …`).
pub fn dial_image(jpeg: &[u8], output_len: usize) -> Vec<Report> {
    upstream::dial_image(jpeg, output_len)
        .into_iter()
        .map(Report)
        .collect()
}

/// Report id of a feature read — only expressible behind the [`SoftwareMode`] proof, so keys
/// and dial cannot be read before the handshake.
pub fn query_id(mode: SoftwareMode, query: Query) -> Option<u8> {
    upstream::query_id(mode.kind(), query)
}

/// The feature reports a module check may read, by name, behind the [`SoftwareMode`] proof.
/// Never includes the serial number (a report the user pastes in public needs none).
pub fn diagnostic_reads(mode: SoftwareMode) -> &'static [(&'static str, u8)] {
    upstream::diagnostic_reads(mode.kind())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_metadata() {
        assert_eq!(CRATE_NAME, "ds-protocol");
        assert!(!version().is_empty());
        assert_eq!(PROTOCOL_VERSION, 1);
    }

    #[test]
    fn raw_path_guard_is_reexported() {
        assert!(is_forbidden(&[0x02, 0x2A, 0x00, 0x00]));
        assert!(!is_forbidden(&[0x02, 0x36, 0x00, 0x00]));
    }
}
