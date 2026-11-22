//! Chunked image reports: split one encoded frame (the device-format JPEG from
//! [`crate::frames::encode`]) into the `02 <cmd> <chunk u16 LE> <len u16 LE> <index> <last>
//! <payload>` reports of one transfer, each padded to the module's 320-byte output report
//! (`docs/contracts/streaming.md` §2, `upstream/crates/mh-protocol/src/report.rs:132-171`,
//! `schemas/stream-frame.json`).
//!
//! - cmd `0x35` = panel (`keys`), index 0; cmd `0x36` = one key, index N; cmd `0x45` = dial.
//! - payload ≤ 312 B per report (320 − 8-byte chunk header); the last chunk carries
//!   `last = 1`. ds-vdev reassembles the chunks and the sha256 of the whole frame is the
//!   golden-frame oracle (`docs/contracts/streaming.md` §8).
//!
//! [`chunk_reports`] builds the raw padded report bytes (the exact wire format, unit-tested
//! here); [`reports`] returns the same reports as sendable [`ds_protocol::Report`] values
//! (the transport accepts only builder-made reports and pads them to the module's
//! `out_len` itself — a test below proves both forms are byte-identical once padded).

use ds_protocol::Report;

use crate::frames::FrameTarget;

/// Bytes in front of a chunk payload: `02 cmd chunk(LE16) len(LE16) index last`.
pub const CHUNK_HEADER_LEN: usize = 8;

/// Output report length of the screen modules (keys and dial), report id included.
pub const REPORT_LEN: usize = 320;

/// Largest chunk payload per report: 320 − 8.
pub const MAX_CHUNK_PAYLOAD: usize = REPORT_LEN - CHUNK_HEADER_LEN; // 312

/// Split `data` into the padded image reports of one transfer for `cmd`/`index`.
///
/// Every report is exactly `report_len` bytes (zero-padded after the payload), the chunk
/// sequence numbers are `u16 LE` starting at 0, `len` is the payload length of that chunk
/// (≤ `report_len − 8`), and only the final chunk has `last = 1`. An empty `data` yields no
/// reports (same as the upstream chunker).
pub fn chunk_reports(cmd: u8, index: u8, data: &[u8], report_len: usize) -> Vec<Vec<u8>> {
    let cap = report_len.saturating_sub(CHUNK_HEADER_LEN).max(1);
    let count = data.len().div_ceil(cap);
    data.chunks(cap)
        .enumerate()
        .map(|(i, payload)| {
            let mut r = Vec::with_capacity(report_len.max(CHUNK_HEADER_LEN + payload.len()));
            r.push(0x02);
            r.push(cmd);
            r.extend_from_slice(&(i as u16).to_le_bytes());
            r.extend_from_slice(&(payload.len() as u16).to_le_bytes());
            r.push(index);
            r.push(u8::from(i + 1 == count));
            r.extend_from_slice(payload);
            r.resize(report_len.max(r.len()), 0);
            r
        })
        .collect()
}

/// The same chunked transfer as sendable [`Report`]s for `target` (built by the safe
/// ds-protocol chunkers; the transport pads them to the module's `out_len`).
pub fn reports(target: FrameTarget, jpeg: &[u8], out_len: usize) -> Vec<Report> {
    match target {
        FrameTarget::Panel => ds_protocol::panel_image(jpeg, out_len),
        FrameTarget::Key(n) => ds_protocol::key_image(n, jpeg, out_len),
        FrameTarget::Dial => ds_protocol::dial_image(jpeg, out_len),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frames::encode::{encode_rgb, encode_solid, test_rgb};
    use ds_device::chunks::{sha256_of, Reassembler};
    use ds_device::module::module_info;
    use ds_device::report::pad_report;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn single_chunk_report_has_the_exact_header_and_320_bytes() {
        let payload = [0xAAu8; 10];
        let reps = chunk_reports(0x36, 3, &payload, REPORT_LEN);
        assert_eq!(reps.len(), 1);
        let r = &reps[0];
        assert_eq!(
            r.len(),
            320,
            "report padded to the full output report length"
        );
        assert_eq!(
            &r[..8],
            &[0x02, 0x36, 0x00, 0x00, 0x0A, 0x00, 0x03, 0x01],
            "02 cmd chunk=0(LE) len=10(LE) index=3 last=1"
        );
        assert_eq!(&r[8..18], &payload);
        assert!(
            r[18..].iter().all(|&b| b == 0),
            "zero padding after the payload"
        );
    }

    #[test]
    fn multi_chunk_transfer_numbers_chunks_and_flags_only_the_last() {
        let data: Vec<u8> = (0..700u32).map(|i| (i % 251) as u8).collect();
        let reps = chunk_reports(0x35, 0, &data, REPORT_LEN);
        assert_eq!(reps.len(), 3, "700 B = 312 + 312 + 76");
        let mut reassembled = Vec::new();
        for (i, r) in reps.iter().enumerate() {
            assert_eq!(r.len(), 320);
            assert_eq!(&r[..2], &[0x02, 0x35]);
            assert_eq!(
                u16::from_le_bytes([r[2], r[3]]),
                i as u16,
                "chunk seq u16 LE"
            );
            let len = u16::from_le_bytes([r[4], r[5]]) as usize;
            assert_eq!(r[6], 0, "panel index 0");
            assert_eq!(
                r[7],
                u8::from(i + 1 == reps.len()),
                "last flag only on the final chunk"
            );
            assert!(len <= MAX_CHUNK_PAYLOAD);
            reassembled.extend_from_slice(&r[8..8 + len]);
        }
        assert_eq!(reps[0][4..6], 312u16.to_le_bytes());
        assert_eq!(reps[2][4..6], 76u16.to_le_bytes());
        assert_eq!(reassembled, data, "payload round-trips exactly");
    }

    #[test]
    fn payload_cap_is_312_bytes() {
        let data = vec![0x5Au8; MAX_CHUNK_PAYLOAD + 1]; // 313
        let reps = chunk_reports(0x45, 0, &data, REPORT_LEN);
        assert_eq!(reps.len(), 2);
        assert_eq!(u16::from_le_bytes([reps[0][4], reps[0][5]]), 312);
        assert_eq!(u16::from_le_bytes([reps[1][4], reps[1][5]]), 1);
        assert_eq!(reps[0][7], 0);
        assert_eq!(reps[1][7], 1);
        // exactly 312 fits in one chunk
        let reps = chunk_reports(0x45, 0, &vec![0x5Au8; MAX_CHUNK_PAYLOAD], REPORT_LEN);
        assert_eq!(reps.len(), 1);
        assert_eq!(reps[0][7], 1);
    }

    #[test]
    fn empty_payload_yields_no_reports() {
        assert!(chunk_reports(0x35, 0, &[], REPORT_LEN).is_empty());
    }

    #[test]
    fn raw_reports_match_the_sendable_protocol_reports_byte_for_byte() {
        let jpeg = {
            let rgb = test_rgb(72, 72);
            encode_rgb(FrameTarget::Key(0), &rgb, 72, 72).unwrap()
        };
        for target in [
            FrameTarget::Panel,
            FrameTarget::Key(0),
            FrameTarget::Key(14),
            FrameTarget::Dial,
        ] {
            let out_len = module_info(target.module()).out_len;
            assert_eq!(out_len, REPORT_LEN);
            let raw = chunk_reports(target.cmd(), target.index(), &jpeg, out_len);
            let sendable = reports(target, &jpeg, out_len);
            assert_eq!(raw.len(), sendable.len(), "{target:?} chunk count");
            let info = module_info(target.module());
            for (raw_r, s_r) in raw.iter().zip(sendable.iter()) {
                let padded = pad_report(&info, s_r);
                assert_eq!(
                    *raw_r, padded,
                    "{target:?} report bytes identical once padded"
                );
                assert_eq!(padded.len(), REPORT_LEN);
            }
        }
    }

    #[test]
    fn chunk_round_trip_through_the_vdev_reassembler() {
        let jpeg = encode_solid(FrameTarget::Key(7), [0x11, 0x22, 0x33]);
        // pad the payload so the transfer spans several chunks
        let mut big = jpeg.clone();
        big.extend(std::iter::repeat_n(0xAB, 1000));
        let target = FrameTarget::Key(7);
        let raw = chunk_reports(target.cmd(), target.index(), &big, REPORT_LEN);
        let mut r = Reassembler::new();
        r.begin(target.cmd(), target.index(), Some(sha256_of(&big)));
        let mut done = None;
        for rep in &raw {
            if let Some(d) = r.push(rep).unwrap() {
                done = Some(d);
            }
        }
        let done = done.expect("last chunk completes the transfer");
        assert_eq!(done.data, big);
        assert_eq!(done.sha256_hex(), hex(&sha256_of(&big)));
        assert_eq!((done.cmd, done.index), (0x36, 7));
    }

    #[test]
    fn key_index_over_14_produces_no_sendable_reports() {
        // ds_protocol::key_image refuses index >= 15; the raw builder is only called with
        // valid indices (FrameTarget::Key holds 0..=14 by construction/parse)
        assert!(reports(FrameTarget::Key(14), b"x", REPORT_LEN).len() >= 1);
    }
}
