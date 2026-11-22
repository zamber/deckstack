//! Chunk reassembly for image pushes: the `02 <cmd> <chunk u16 LE> <len u16 LE> <index> <last>
//! <data…>` reports of one transfer are collected into the whole JPEG again, and the sha256 of
//! the reassembled bytes is verified against the expected digest when one was given
//! (`ds-vdevctl frames <target>` prints that sha256 — the golden-frame oracle, streaming.md §8).
//!
//! The reassembler is deliberately strict where the hardware is silent: a duplicate or
//! out-of-order chunk, an oversized or truncated chunk, or a sha mismatch is an error, not a
//! silent corruption. Transfers are keyed by `(cmd, index)`, so a panel push (`0x35`, index 0)
//! and a key push (`0x36`, index N) can be in flight at the same time.

use std::collections::HashMap;

use sha2::{Digest, Sha256};

use crate::error::Result;
use crate::report::{view_report, ImageChunk, ReportView};

/// Largest chunk payload: a 320-byte output report minus the 8-byte chunk header
/// (`upstream/PROTOCOL.md:87-96`).
pub const MAX_CHUNK_PAYLOAD: usize = 312;

/// Why a chunk was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReassemblyError {
    #[error("chunk length {len} exceeds the {max}-byte chunk capacity")]
    Oversized { len: u16, max: usize },
    #[error("chunk {0} was already received for this transfer")]
    Duplicate(u16),
    #[error("expected chunk {expected}, got {got}")]
    OutOfOrder { expected: u16, got: u16 },
    #[error("sha256 mismatch: expected {expected}, reassembled {got}")]
    ShaMismatch { expected: String, got: String },
    #[error("report id {0:#04x} is not an image chunk")]
    NotAChunk(u8),
    #[error("no transfer in progress for command {0:#04x} index {1}")]
    NotInProgress(u8, u8),
}

/// A finished transfer: the whole image plus its sha256.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reassembled {
    /// Image command (`0x35` panel, `0x36` key, `0x45` dial).
    pub cmd: u8,
    /// Target index (key number for `0x36`, `0` for panel/dial).
    pub index: u8,
    /// Reassembled payload (the JPEG bytes).
    pub data: Vec<u8>,
    /// sha256 of `data`.
    pub sha256: [u8; 32],
}

impl Reassembled {
    /// sha256 of the reassembled bytes as lowercase hex — the golden-frame oracle value.
    pub fn sha256_hex(&self) -> String {
        self.sha256.iter().map(|b| format!("{b:02x}")).collect()
    }
}

#[derive(Debug)]
struct Transfer {
    buf: Vec<u8>,
    next: u16,
    expected: Option<[u8; 32]>,
}

/// Collects the chunked reports of image transfers. Feed it raw output reports in arrival
/// order with [`Reassembler::push`].
#[derive(Debug, Default)]
pub struct Reassembler {
    transfers: HashMap<(u8, u8), Transfer>,
}

impl Reassembler {
    /// A fresh reassembler with no transfers in progress.
    pub fn new() -> Self {
        Self::default()
    }

    /// Start (or restart) the transfer for `cmd`/`index`, optionally expecting `sha256`.
    /// Restarting drops any partially collected bytes of the previous attempt.
    pub fn begin(&mut self, cmd: u8, index: u8, expected_sha256: Option<[u8; 32]>) {
        self.transfers.insert(
            (cmd, index),
            Transfer {
                buf: Vec::new(),
                next: 0,
                expected: expected_sha256,
            },
        );
    }

    /// Number of transfers currently in progress.
    pub fn len(&self) -> usize {
        self.transfers.len()
    }

    /// Whether no transfer is in progress.
    pub fn is_empty(&self) -> bool {
        self.transfers.is_empty()
    }

    /// Drop all in-progress transfers.
    pub fn reset(&mut self) {
        self.transfers.clear();
    }

    /// Feed one raw output report. Returns `Ok(Some(..))` when the report was the last chunk of
    /// a transfer (sha256 already verified if one was expected), `Ok(None)` for intermediate
    /// chunks, and `Err` for malformed, duplicate, out-of-order or oversized chunks and for sha
    /// mismatches.
    pub fn push(&mut self, raw: &[u8]) -> Result<Option<Reassembled>> {
        match view_report(raw) {
            ReportView::ImageChunk(c) => self.push_chunk(&c),
            _ => Err(ReassemblyError::NotAChunk(*raw.first().unwrap_or(&0xFF)).into()),
        }
    }

    /// Feed one decoded chunk.
    pub fn push_chunk(&mut self, c: &ImageChunk<'_>) -> Result<Option<Reassembled>> {
        let key = (c.cmd, c.index);
        let Some(t) = self.transfers.get_mut(&key) else {
            return Err(ReassemblyError::NotInProgress(c.cmd, c.index).into());
        };
        if c.len as usize > MAX_CHUNK_PAYLOAD || c.data.len() < c.len as usize {
            return Err(ReassemblyError::Oversized {
                len: c.len,
                max: MAX_CHUNK_PAYLOAD.min(c.data.len()),
            }
            .into());
        }
        if c.chunk < t.next {
            return Err(ReassemblyError::Duplicate(c.chunk).into());
        }
        if c.chunk > t.next {
            return Err(ReassemblyError::OutOfOrder {
                expected: t.next,
                got: c.chunk,
            }
            .into());
        }
        t.buf.extend_from_slice(&c.data[..c.len as usize]);
        t.next += 1;
        if !c.last {
            return Ok(None);
        }
        let t = self.transfers.remove(&key).expect("transfer checked above");
        let sha = Sha256::digest(&t.buf);
        let reassembled = Reassembled {
            cmd: c.cmd,
            index: c.index,
            data: t.buf,
            sha256: sha.into(),
        };
        if let Some(expected) = t.expected {
            if expected != reassembled.sha256 {
                return Err(ReassemblyError::ShaMismatch {
                    expected: hex(&expected),
                    got: reassembled.sha256_hex(),
                }
                .into());
            }
        }
        Ok(Some(reassembled))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// sha256 of `data` as a fixed-size digest (the value to pass to [`Reassembler::begin`]).
pub fn sha256_of(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use ds_protocol::{self, ModuleKind};

    use crate::module::module_info;

    fn key_reports(index: u8, jpeg: &[u8]) -> Vec<Vec<u8>> {
        let out_len = module_info(ModuleKind::Keys).out_len;
        ds_protocol::key_image(index, jpeg, out_len)
            .into_iter()
            .map(|r| r.bytes().to_vec())
            .collect()
    }

    #[test]
    fn reassembles_a_split_transfer() {
        let jpeg: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let reports = key_reports(3, &jpeg);
        assert!(
            reports.len() > 1,
            "1000 bytes must split into several 312-byte chunks"
        );
        let mut r = Reassembler::new();
        r.begin(0x36, 3, Some(sha256_of(&jpeg)));
        for (i, raw) in reports.iter().enumerate() {
            let last = i + 1 == reports.len();
            let out = r.push(raw).unwrap();
            if last {
                let done = out.expect("last chunk completes the transfer");
                assert_eq!(done.data, jpeg);
                assert_eq!(done.sha256, sha256_of(&jpeg));
                assert_eq!(done.cmd, 0x36);
                assert_eq!(done.index, 3);
            } else {
                assert!(out.is_none());
            }
        }
        assert!(r.is_empty());
    }

    #[test]
    fn sha_mismatch_is_rejected() {
        let jpeg = vec![0x42u8; 700];
        let reports = key_reports(0, &jpeg);
        let mut r = Reassembler::new();
        r.begin(0x36, 0, Some([0xEE; 32]));
        let mut result = Ok(None);
        for raw in &reports {
            result = r.push(raw);
            if result.is_err() {
                break;
            }
        }
        let Err(Error::Reassembly(ReassemblyError::ShaMismatch { expected, got })) = result else {
            panic!("expected ShaMismatch, got {result:?}");
        };
        assert_eq!(expected, hex(&[0xEE; 32]));
        assert_eq!(got, r_sha(&jpeg));
        assert!(r.is_empty(), "a failed transfer is dropped");
    }

    #[test]
    fn duplicate_chunk_is_rejected() {
        let reports = key_reports(5, &[0x77; 700]);
        let mut r = Reassembler::new();
        r.begin(0x36, 5, None);
        assert!(r.push(&reports[0]).unwrap().is_none());
        let Err(Error::Reassembly(ReassemblyError::Duplicate(0))) = r.push(&reports[0]) else {
            panic!("duplicate chunk accepted");
        };
    }

    #[test]
    fn out_of_order_chunk_is_rejected() {
        let reports = key_reports(5, &[0x77; 700]);
        let mut r = Reassembler::new();
        r.begin(0x36, 5, None);
        let Err(Error::Reassembly(ReassemblyError::OutOfOrder {
            expected: 0,
            got: 1,
        })) = r.push(&reports[1])
        else {
            panic!("out-of-order chunk accepted");
        };
    }

    #[test]
    fn oversized_and_truncated_chunks_are_rejected() {
        let mut r = Reassembler::new();
        r.begin(0x35, 0, None);
        // declared len 400 > 312-byte capacity
        let mut raw = vec![0x02, 0x35, 0x00, 0x00, 0x90, 0x01, 0x00, 0x00];
        raw.extend(std::iter::repeat_n(0x00, MAX_CHUNK_PAYLOAD));
        assert!(matches!(
            r.push(&raw),
            Err(Error::Reassembly(ReassemblyError::Oversized {
                len: 400,
                ..
            }))
        ));
        // declared len 100 but only 3 payload bytes present (truncated report)
        let raw = [0x02, 0x35, 0x00, 0x00, 0x64, 0x00, 0x00, 0x00, 1, 2, 3];
        assert!(matches!(
            r.push(&raw),
            Err(Error::Reassembly(ReassemblyError::Oversized {
                len: 100,
                ..
            }))
        ));
    }

    #[test]
    fn non_chunk_reports_and_unknown_transfers_are_rejected() {
        let mut r = Reassembler::new();
        assert!(matches!(
            r.push(&[0x03, 0x3A, 0x01]),
            Err(Error::Reassembly(ReassemblyError::NotAChunk(0x03)))
        ));
        assert!(matches!(
            r.push(&[0x02, 0x2D, 0x00]),
            Err(Error::Reassembly(ReassemblyError::NotAChunk(0x02)))
        ));
        assert!(matches!(
            r.push(&[0x02, 0x36, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0xFF]),
            Err(Error::Reassembly(ReassemblyError::NotInProgress(0x36, 0)))
        ));
    }

    #[test]
    fn concurrent_transfers_are_independent() {
        let panel: Vec<u8> = (0..500u32).map(|i| (i % 253) as u8).collect();
        let key: Vec<u8> = (0..400u32).map(|i| (i % 249) as u8).collect();
        let out_len = module_info(ModuleKind::Keys).out_len;
        let panel_reports: Vec<Vec<u8>> = ds_protocol::panel_image(&panel, out_len)
            .into_iter()
            .map(|r| r.bytes().to_vec())
            .collect();
        let key_reports: Vec<Vec<u8>> = ds_protocol::key_image(7, &key, out_len)
            .into_iter()
            .map(|r| r.bytes().to_vec())
            .collect();
        let mut r = Reassembler::new();
        r.begin(0x35, 0, Some(sha256_of(&panel)));
        r.begin(0x36, 7, Some(sha256_of(&key)));
        assert_eq!(r.len(), 2);
        // interleave the two transfers
        let mut done = Vec::new();
        for i in 0..panel_reports.len().max(key_reports.len()) {
            if i < panel_reports.len() {
                if let Some(d) = r.push(&panel_reports[i]).unwrap() {
                    done.push(d);
                }
            }
            if i < key_reports.len() {
                if let Some(d) = r.push(&key_reports[i]).unwrap() {
                    done.push(d);
                }
            }
        }
        assert_eq!(done.len(), 2);
        assert!(done.iter().any(|d| d.cmd == 0x35 && d.data == panel));
        assert!(done
            .iter()
            .any(|d| d.cmd == 0x36 && d.index == 7 && d.data == key));
    }

    #[test]
    fn begin_restarts_a_transfer() {
        let reports = key_reports(1, &[0x11; 700]);
        let mut r = Reassembler::new();
        r.begin(0x36, 1, None);
        r.push(&reports[0]).unwrap();
        r.begin(0x36, 1, None); // restart: chunk 0 is acceptable again
        assert!(r.push(&reports[0]).unwrap().is_none());
        r.reset();
        assert!(r.is_empty());
    }

    fn r_sha(data: &[u8]) -> String {
        hex(&sha256_of(data))
    }
}
