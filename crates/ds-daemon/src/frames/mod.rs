//! The frame plane: re-encode any accepted wire image into the device wire format and
//! split it into the chunked image reports the modules accept (ticket DS-030,
//! `docs/contracts/streaming.md` §1-2, `docs/components/frame-service.md`).
//!
//! - [`encode`] — wire-frame validation (≤ 16 MB payload, decode caps ≤ 4096×4096) and
//!   re-encode to **baseline JPEG, standard Huffman tables, 4:4:4**, quality per target
//!   (`keys` panel q80, `key:N` q90, `dial` q85). Optimized-Huffman or progressive JPEG
//!   input is rejected with a typed [`encode::EncodeError`] that maps to `E_FORMAT`; the
//!   frame is then dropped (counted), never forwarded.
//! - [`chunk`] — the `02 <cmd> <chunk u16 LE> <len u16 LE> <index> <last> <≤312 B payload>`
//!   chunked reports inside 320 B output reports for cmds `0x35` (panel), `0x36` (key N),
//!   `0x45` (dial).
//!
//! - [`service`] — the frame service (DS-031): per-target latest-frame-wins queues
//!   (1 in flight + 1 waiting), the global 256 MiB frame-queue budget, the [`FrameSink`]
//!   that routes accepted stream-socket frames into the queues, stream stop/drain, and
//!   the stats behind `/telemetry/streams/<target>`.
//! - [`writer`] — one dedicated writer thread per module owning the send side of the
//!   transport: whole-frame paced sends, no lock held across device I/O, graceful
//!   stop/drain on shutdown.
//! - [`telemetry`] — the ≤ 1 Hz sampler writing non-durable
//!   `/telemetry/streams/<target>` stats into the state store (never persisted, no rev).
//! - [`animate`] — the animation plane (DS-034): multi-frame decode of animated GIF/APNG
//!   and of a directory of stills at `--fps` under the contractual limits (≤ 300 frames,
//!   ≤ 30 fps, ≥ 0.02 s per frame, ≤ 16 MB wire, ≤ 4096×4096, bounded total decode), paced
//!   pushes through the service queues, and the DSF1 `frames`/`fps`/`loop` meta mapping.
//!
//! Encoding is deterministic for a pinned `jpeg-encoder` version, which is what makes the
//! golden vectors under `tests/golden/` exact-byte fixtures (see `tests/golden/README.md`).

pub mod animate;
pub mod chunk;
pub mod encode;
pub mod service;
pub mod telemetry;
pub mod writer;

use ds_protocol::ModuleKind;

/// A display target in the unified streaming vocabulary (`docs/contracts/streaming.md` §1):
/// the keys panel, one key, or the dial screen — with its module, wire command, index,
/// target size and JPEG quality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameTarget {
    /// The whole keys panel, 480×272 (`02 35`, index 0, q80).
    Panel,
    /// One key, 72×72 (`02 36`, index N, q90).
    Key(u8),
    /// The dial screen, 240×240 (`02 45`, index 0, q85).
    Dial,
}

impl FrameTarget {
    /// Parse the wire/stream vocabulary: `keys`, `key:<0-14>`, `dial`
    /// (`schemas/stream-frame.json` `target` pattern).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "keys" => Some(FrameTarget::Panel),
            "dial" => Some(FrameTarget::Dial),
            _ => s
                .strip_prefix("key:")
                .and_then(|n| n.parse::<u8>().ok())
                .filter(|&n| n < 15)
                .map(FrameTarget::Key),
        }
    }

    /// The image command byte on the wire (`0x35`/`0x36`/`0x45`).
    pub fn cmd(self) -> u8 {
        match self {
            FrameTarget::Panel => 0x35,
            FrameTarget::Key(_) => 0x36,
            FrameTarget::Dial => 0x45,
        }
    }

    /// The chunk index byte: the key number for `0x36`, else 0.
    pub fn index(self) -> u8 {
        match self {
            FrameTarget::Key(n) => n,
            _ => 0,
        }
    }

    /// The JPEG quality for this target (`docs/contracts/streaming.md` §1).
    pub fn quality(self) -> u8 {
        match self {
            FrameTarget::Panel => 80,
            FrameTarget::Key(_) => 90,
            FrameTarget::Dial => 85,
        }
    }

    /// The module the image reports are written to.
    pub fn module(self) -> ModuleKind {
        match self {
            FrameTarget::Panel | FrameTarget::Key(_) => ModuleKind::Keys,
            FrameTarget::Dial => ModuleKind::Dial,
        }
    }

    /// The wire/stream name of the target (`keys` | `key:<0-14>` | `dial`) — the key used
    /// under `/telemetry/streams/<target>` and in `stream-open`/`stream-ack` envelopes.
    pub fn wire_name(self) -> String {
        match self {
            FrameTarget::Panel => "keys".to_string(),
            FrameTarget::Key(n) => format!("key:{n}"),
            FrameTarget::Dial => "dial".to_string(),
        }
    }

    /// Native screen width in pixels (`upstream/PROTOCOL.md:44,64`).
    pub fn width(self) -> u32 {
        match self {
            FrameTarget::Panel => 480,
            FrameTarget::Key(_) => 72,
            FrameTarget::Dial => 240,
        }
    }

    /// Native screen height in pixels.
    pub fn height(self) -> u32 {
        match self {
            FrameTarget::Panel => 272,
            FrameTarget::Key(_) => 72,
            FrameTarget::Dial => 240,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_vocabulary_matches_the_contract() {
        assert_eq!(FrameTarget::parse("keys"), Some(FrameTarget::Panel));
        assert_eq!(FrameTarget::parse("dial"), Some(FrameTarget::Dial));
        assert_eq!(FrameTarget::parse("key:0"), Some(FrameTarget::Key(0)));
        assert_eq!(FrameTarget::parse("key:14"), Some(FrameTarget::Key(14)));
        for bad in [
            "panel", "screen", "key:15", "key:99", "key:x", "keys:1", "", "KEY:0",
        ] {
            assert_eq!(FrameTarget::parse(bad), None, "{bad:?}");
        }
        assert_eq!(FrameTarget::Panel.cmd(), 0x35);
        assert_eq!(FrameTarget::Key(7).cmd(), 0x36);
        assert_eq!(FrameTarget::Dial.cmd(), 0x45);
        assert_eq!(FrameTarget::Panel.index(), 0);
        assert_eq!(FrameTarget::Key(7).index(), 7);
        assert_eq!(FrameTarget::Dial.index(), 0);
        assert_eq!(FrameTarget::Panel.quality(), 80);
        assert_eq!(FrameTarget::Key(3).quality(), 90);
        assert_eq!(FrameTarget::Dial.quality(), 85);
        assert_eq!(
            (FrameTarget::Panel.width(), FrameTarget::Panel.height()),
            (480, 272)
        );
        assert_eq!(
            (FrameTarget::Key(0).width(), FrameTarget::Key(0).height()),
            (72, 72)
        );
        assert_eq!(
            (FrameTarget::Dial.width(), FrameTarget::Dial.height()),
            (240, 240)
        );
        assert_eq!(FrameTarget::Panel.module(), ModuleKind::Keys);
        assert_eq!(FrameTarget::Key(2).module(), ModuleKind::Keys);
        assert_eq!(FrameTarget::Dial.module(), ModuleKind::Dial);
        assert_eq!(FrameTarget::Panel.wire_name(), "keys");
        assert_eq!(FrameTarget::Key(7).wire_name(), "key:7");
        assert_eq!(FrameTarget::Dial.wire_name(), "dial");
    }
}

/// Golden-frame vectors (ticket DS-030): a fixed input image under `tests/golden/` must
/// encode to the exact checked-in JPEG bytes / sha256 / chunk sequence. Regenerate the
/// fixtures deliberately with `DS_GOLDEN_REGEN=1 cargo test -p ds-daemon golden` (never
/// silently — the fixtures record the pinned encoder version in `tests/golden/README.md`).
#[cfg(test)]
mod golden {
    use super::*;
    use crate::frames::{chunk, encode};
    use std::path::PathBuf;

    fn golden_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden")
    }

    fn repo_file(rel: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(rel)
    }

    fn sha256_hex(data: &[u8]) -> String {
        ds_device::chunks::sha256_of(data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    #[test]
    fn golden_key0_vectors_match_the_checked_in_files() {
        let dir = golden_dir();
        let target = FrameTarget::Key(0);
        let input =
            std::fs::read(dir.join("input.png")).expect("tests/golden/input.png checked in");
        let jpeg = encode::encode_frame(target, &input).expect("golden input encodes");

        if std::env::var_os("DS_GOLDEN_REGEN").is_some() {
            std::fs::write(dir.join("key0.jpg"), &jpeg).unwrap();
            std::fs::write(dir.join("key0.sha256"), format!("{}\n", sha256_hex(&jpeg))).unwrap();
            let raw = chunk::chunk_reports(target.cmd(), target.index(), &jpeg, chunk::REPORT_LEN);
            let mut bin = Vec::new();
            for r in &raw {
                bin.extend_from_slice(r);
            }
            std::fs::write(dir.join("key0.chunks.bin"), &bin).unwrap();
            // the e2e suite pushes examples/key.png to key:0 — its encoded sha is the
            // golden the e2e step 3 compares the vdev frame against
            let example = std::fs::read(repo_file("examples/key.png")).unwrap();
            let ex_jpeg = encode::encode_frame(target, &example).unwrap();
            std::fs::write(
                dir.join("example-key0.sha256"),
                format!("{}\n", sha256_hex(&ex_jpeg)),
            )
            .unwrap();
        }

        // exact JPEG bytes
        let want_jpg =
            std::fs::read(dir.join("key0.jpg")).expect("tests/golden/key0.jpg checked in");
        assert_eq!(
            jpeg, want_jpg,
            "encoded bytes drifted from tests/golden/key0.jpg — if the encoder was bumped, \
             regenerate deliberately with DS_GOLDEN_REGEN=1 and record it in tests/golden/README.md"
        );
        // exact sha256
        let want_sha = std::fs::read_to_string(dir.join("key0.sha256")).unwrap();
        assert_eq!(sha256_hex(&jpeg), want_sha.trim());
        // the encoded frame is a valid baseline/standard/4:4:4 JPEG of the input size
        encode::validate_jpeg_baseline_standard(&jpeg).unwrap();
        let mut dec = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(&jpeg));
        let decoded = dec.decode().unwrap();
        let info = dec.info().expect("image info");
        assert_eq!((u32::from(info.width), u32::from(info.height)), (72, 72));
        assert_eq!(decoded.len(), 72 * 72 * 3);
        // exact chunk sequence (concatenated 320-byte reports)
        let want_chunks =
            std::fs::read(dir.join("key0.chunks.bin")).expect("tests/golden/key0.chunks.bin");
        let raw = chunk::chunk_reports(target.cmd(), target.index(), &jpeg, chunk::REPORT_LEN);
        let bin: Vec<u8> = raw.iter().flat_map(|r| r.iter().copied()).collect();
        assert_eq!(bin.len() % chunk::REPORT_LEN, 0);
        assert_eq!(
            bin, want_chunks,
            "chunk sequence must equal tests/golden/key0.chunks.bin"
        );
        // the e2e golden for examples/key.png -> key:0
        let example = std::fs::read(repo_file("examples/key.png")).unwrap();
        let ex_jpeg = encode::encode_frame(target, &example).unwrap();
        let want_ex = std::fs::read_to_string(dir.join("example-key0.sha256")).unwrap();
        assert_eq!(sha256_hex(&ex_jpeg), want_ex.trim());
    }
}
