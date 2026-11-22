//! The DSF1 binary frame codec (`docs/contracts/ipc.md` §2, `schemas/stream-frame.json`).
//!
//! Wire layout on the stream socket:
//!
//! ```text
//! "DSF1" | u32 LE meta_len | UTF-8 JSON meta (schemas/stream-frame.json) | u64 LE payload_len | payload
//! ```
//!
//! The meta object is validated against the embedded `schemas/stream-frame.json` (draft
//! 2020-12) on decode, exactly like the control plane validates against the embedded
//! `schemas/ipc-message.json`. The payload is opaque bytes (PNG/JPEG/GIF) whose CRC32
//! rides in the meta; the codec checks it. Stream-socket control lines use the separate
//! `DSctl\n` prefix (see [`super::server`]) so the two framings never mix.
//!
//! S1: a frame is bytes on a wire. This module has no state handle, no revision, no
//! persistence — `ds-ipc` does not depend on `ds-state` (see `Cargo.toml`).

use crate::error::{ErrorCode, IpcError};
use jsonschema::{Draft, Validator};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{Read, Write};
use std::sync::OnceLock;

/// `schemas/stream-frame.json`, embedded at compile time (same file the fixture gate
/// `tests/fixtures/validate.sh` validates).
pub const STREAM_FRAME_SCHEMA_JSON: &str = include_str!("../../../../schemas/stream-frame.json");

/// The 4-byte magic starting every DSF1 binary frame.
pub const DSF1_MAGIC: &[u8; 4] = b"DSF1";

/// The 5-byte magic starting every NDJSON control line on the stream socket
/// (`DSctl\n`), so control lines and binary frames are distinguishable on one socket.
pub const DSCTL_MAGIC: &[u8; 6] = b"DSctl\n";

/// Cap for the meta JSON blob. The schema bounds every field, so a valid meta is a few
/// hundred bytes; the cap exists so a hostile peer cannot make us buffer unbounded input
/// before validation (same role as the 1 MiB control-line cap).
pub const MAX_META_BYTES: u32 = 64 * 1024;

/// The compiled `schemas/stream-frame.json` validator (draft 2020-12). The schema has no
/// external `$ref`s, so no retriever is needed.
pub fn frame_meta_validator() -> &'static Validator {
    static VALIDATOR: OnceLock<Validator> = OnceLock::new();
    VALIDATOR.get_or_init(|| {
        let schema: Value = serde_json::from_str(STREAM_FRAME_SCHEMA_JSON)
            .expect("embedded schemas/stream-frame.json parses");
        jsonschema::options()
            .with_draft(Draft::Draft202012)
            .build(&schema)
            .expect("schemas/stream-frame.json compiles")
    })
}

/// Validate a frame meta object against `schemas/stream-frame.json`. Returns the first
/// violation as an `E_SCHEMA` error.
pub fn validate_meta(meta: &Value) -> Result<(), IpcError> {
    if let Err(err) = frame_meta_validator().validate(meta) {
        return Err(IpcError::new(
            ErrorCode::ESchema,
            format!("frame meta violates schemas/stream-frame.json: {err}"),
        ));
    }
    Ok(())
}

/// The DSF1 frame meta: exactly the properties of `schemas/stream-frame.json`. Field
/// order is the schema's property order, which makes the encoded golden vectors stable.
///
/// `w`/`h`/`frames`/`fps`/`loop` are optional on the wire (the schema does not require
/// them); `max_payload_bytes` is a constant the producer echoes to declare the 16 MB cap.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameMeta {
    /// `keys` | `key:0-14` | `dial` (S6 target vocabulary).
    pub target: String,
    /// Producer-side monotonically increasing sequence per target.
    pub seq: u64,
    /// Producer timestamp, ms since epoch.
    pub ts_ms: u64,
    /// Wire payload format.
    pub format: FrameFormat,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub w: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub h: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frames: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fps: Option<f64>,
    #[serde(rename = "loop", skip_serializing_if = "Option::is_none")]
    pub loop_: Option<bool>,
    /// CRC32 (IEEE) of the payload bytes.
    pub crc32: u32,
    /// Must equal [`crate::frames::limits::MAX_PAYLOAD_BYTES`]; enforced on decode.
    pub max_payload_bytes: u32,
}

/// Wire payload format (the schema's `format` enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FrameFormat {
    Png,
    Jpeg,
    Gif,
}

impl FrameMeta {
    /// The meta as a JSON value, validated against the schema.
    pub fn to_value(&self) -> Result<Value, IpcError> {
        let v = serde_json::to_value(self).expect("FrameMeta serializes");
        validate_meta(&v)?;
        Ok(v)
    }

    /// Parse + validate a meta JSON value.
    pub fn from_value(v: &Value) -> Result<Self, IpcError> {
        validate_meta(v)?;
        serde_json::from_value(v.clone()).map_err(|e| {
            IpcError::new(
                ErrorCode::ESchema,
                format!("frame meta does not match the schema types: {e}"),
            )
        })
    }
}

/// One decoded DSF1 frame: validated meta + raw payload bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub meta: FrameMeta,
    pub payload: Vec<u8>,
}

impl Frame {
    /// Build a frame, computing the payload CRC32 into the meta.
    pub fn new(mut meta: FrameMeta, payload: &[u8]) -> Self {
        meta.crc32 = crc32(payload);
        Self {
            meta,
            payload: payload.to_vec(),
        }
    }

    /// Encode to the DSF1 wire form: `"DSF1" | u32 LE meta_len | meta | u64 LE payload_len | payload`.
    pub fn encode(&self) -> Result<Vec<u8>, IpcError> {
        let meta_json = serde_json::to_vec(&self.meta).expect("FrameMeta serializes");
        if meta_json.len() > MAX_META_BYTES as usize {
            return Err(IpcError::new(
                ErrorCode::ELimit,
                "frame meta exceeds the 64 KiB meta cap",
            ));
        }
        let mut out = Vec::with_capacity(4 + 4 + meta_json.len() + 8 + self.payload.len());
        out.extend_from_slice(DSF1_MAGIC);
        out.extend_from_slice(&(meta_json.len() as u32).to_le_bytes());
        out.extend_from_slice(&meta_json);
        out.extend_from_slice(&(self.payload.len() as u64).to_le_bytes());
        out.extend_from_slice(&self.payload);
        Ok(out)
    }
}

/// Why decoding a frame failed. Each variant maps to a stable [`ErrorCode`].
#[derive(Debug)]
pub enum DecodeError {
    /// Not enough bytes / clean EOF mid-frame.
    Eof,
    /// The 4-byte magic was not `DSF1` (and not `DSctl` — control lines are handled by
    /// the stream server before the codec is invoked).
    BadMagic,
    /// `meta_len` exceeded [`MAX_META_BYTES`] → `E_LIMIT` before reading the meta.
    MetaTooLong,
    /// The meta is not valid UTF-8/JSON or violates `schemas/stream-frame.json` → `E_SCHEMA`.
    BadMeta(IpcError),
    /// `payload_len` exceeded the 16 MB wire cap → `E_LIMIT` **before reading the payload**
    /// (the cap exists so a hostile peer cannot make us buffer it). Carries the frame's
    /// `seq` so the nack can reference it.
    PayloadTooLong(u64),
    /// The declared `max_payload_bytes` is not the 16 MB constant → `E_SCHEMA`.
    BadMaxPayload,
    /// The payload CRC32 did not match the meta's `crc32` → `E_SCHEMA`.
    CrcMismatch,
    /// Underlying I/O error.
    Io(std::io::Error),
}

impl DecodeError {
    /// The stable error envelope for this failure.
    pub fn to_ipc_error(&self) -> IpcError {
        match self {
            Self::Eof => IpcError::new(ErrorCode::EUnreachable, "connection closed mid-frame"),
            Self::BadMagic => IpcError::new(
                ErrorCode::ESchema,
                "stream socket message is neither DSF1 nor DSctl",
            ),
            Self::MetaTooLong => {
                IpcError::new(ErrorCode::ELimit, "frame meta exceeds the 64 KiB meta cap")
            }
            Self::BadMeta(e) => e.clone(),
            Self::PayloadTooLong(_) => IpcError::new(
                ErrorCode::ELimit,
                "frame payload exceeds the 16 MB wire cap",
            ),
            Self::BadMaxPayload => IpcError::new(
                ErrorCode::ESchema,
                "meta max_payload_bytes must be 16777216",
            ),
            Self::CrcMismatch => IpcError::new(
                ErrorCode::ESchema,
                "payload CRC32 does not match the frame meta",
            ),
            Self::Io(e) => IpcError::new(ErrorCode::EInternal, format!("i/o error: {e}")),
        }
    }
}

/// The header of one DSF1 frame: validated meta + declared payload length (already
/// checked against the wire cap).
#[derive(Debug, Clone)]
pub struct FrameHeader {
    pub meta: FrameMeta,
    pub payload_len: u64,
}

/// Read exactly one DSF1 frame from `reader` (magic included). `max_payload` is the wire
/// cap ([`crate::frames::limits::MAX_PAYLOAD_BYTES`]); a larger declared length is
/// rejected before the payload is read.
pub fn read_frame(reader: &mut impl Read, max_payload: u64) -> Result<Frame, DecodeError> {
    let mut magic = [0u8; 4];
    reader.read_exact(&mut magic).map_err(eof_or_io)?;
    if &magic != DSF1_MAGIC {
        return Err(DecodeError::BadMagic);
    }
    let header = read_frame_header(reader, max_payload)?;
    let payload = read_payload(reader, header.payload_len, header.meta.crc32)?;
    Ok(Frame {
        meta: header.meta,
        payload,
    })
}

/// Read the meta + payload length of one DSF1 frame, **assuming the 4-byte magic was
/// already consumed** (the stream server peeks the magic to tell frames from `DSctl\n`
/// lines). The declared payload length is checked against `max_payload` here, so an
/// oversize frame is rejected before any payload byte is buffered.
pub fn read_frame_header(
    reader: &mut impl Read,
    max_payload: u64,
) -> Result<FrameHeader, DecodeError> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).map_err(eof_or_io)?;
    let meta_len = u32::from_le_bytes(len_buf);
    if meta_len > MAX_META_BYTES {
        return Err(DecodeError::MetaTooLong);
    }
    let mut meta_bytes = vec![0u8; meta_len as usize];
    reader.read_exact(&mut meta_bytes).map_err(eof_or_io)?;
    let meta_value: Value = serde_json::from_slice(&meta_bytes).map_err(|e| {
        DecodeError::BadMeta(IpcError::new(
            ErrorCode::ESchema,
            format!("frame meta is not one JSON object: {e}"),
        ))
    })?;
    if let Err(e) = validate_meta(&meta_value) {
        return Err(DecodeError::BadMeta(e));
    }
    let meta = FrameMeta::from_value(&meta_value).map_err(DecodeError::BadMeta)?;
    if u64::from(meta.max_payload_bytes) != max_payload {
        return Err(DecodeError::BadMaxPayload);
    }
    let mut plen_buf = [0u8; 8];
    reader.read_exact(&mut plen_buf).map_err(eof_or_io)?;
    let payload_len = u64::from_le_bytes(plen_buf);
    if payload_len > max_payload {
        return Err(DecodeError::PayloadTooLong(meta.seq));
    }
    Ok(FrameHeader { meta, payload_len })
}

/// Read and CRC-check the payload of a frame whose header was read with
/// [`read_frame_header`].
pub fn read_payload(
    reader: &mut impl Read,
    payload_len: u64,
    expected_crc32: u32,
) -> Result<Vec<u8>, DecodeError> {
    let mut payload = vec![0u8; payload_len as usize];
    reader.read_exact(&mut payload).map_err(eof_or_io)?;
    if crc32(&payload) != expected_crc32 {
        return Err(DecodeError::CrcMismatch);
    }
    Ok(payload)
}

/// Peek the first bytes of a stream message to tell `DSF1` binary frames from `DSctl\n`
/// control lines. Returns `true` for a DSF1 frame, `false` for a DSctl line. The peeked
/// bytes must be fed to the subsequent reader (use a `BufReader` and [`std::io::BufRead::fill_buf`]).
pub fn is_frame_prefix(buf: &[u8]) -> bool {
    buf.starts_with(DSF1_MAGIC.as_slice())
}

/// Whether the peeked prefix is a `DSctl\n` control line.
pub fn is_dsctl_prefix(buf: &[u8]) -> bool {
    buf.starts_with(DSCTL_MAGIC.as_slice())
}

fn eof_or_io(e: std::io::Error) -> DecodeError {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        DecodeError::Eof
    } else {
        DecodeError::Io(e)
    }
}

/// Write one DSF1 frame to `writer` (see [`Frame::encode`]).
pub fn write_frame(writer: &mut impl Write, frame: &Frame) -> Result<(), IpcError> {
    let bytes = frame.encode()?;
    writer
        .write_all(&bytes)
        .map_err(|e| IpcError::new(ErrorCode::EInternal, format!("i/o error: {e}")))
}

/// CRC32 (IEEE 802.3, polynomial 0xEDB88320, reflected) — same value as Python's
/// `zlib.crc32` and `cksum`-family tools, so golden vectors are portable. Table-driven,
/// no external crate.
pub fn crc32(data: &[u8]) -> u32 {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, entry) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *entry = c;
        }
        t
    });
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = table[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_meta() -> FrameMeta {
        FrameMeta {
            target: "keys".into(),
            seq: 7,
            ts_ms: 1_732_000_000_000,
            format: FrameFormat::Png,
            w: Some(480),
            h: Some(272),
            frames: Some(1),
            fps: None,
            loop_: None,
            crc32: 0, // filled by Frame::new
            max_payload_bytes: crate::frames::limits::MAX_PAYLOAD_BYTES as u32,
        }
    }

    /// Golden vector: fixed meta + payload "hello" → exact wire bytes (independently
    /// computed with Python's json/zlib.crc32/struct). Guards the binary layout against
    /// accidental reordering (the ticket's "golden vectors from day one" risk note).
    #[test]
    fn golden_vector_encode() {
        let frame = Frame::new(sample_meta(), b"hello");
        let expected_hex = "44534631880000007b22746172676574223a226b657973222c22736571223a372c2274735f6d73223a313733323030303030303030302c22666f726d6174223a22706e67222c2277223a3438302c2268223a3237322c226672616d6573223a312c226372633332223a3930373036303837302c226d61785f7061796c6f61645f6279746573223a31363737373231367d050000000000000068656c6c6f";
        let expected = hex_decode(expected_hex);
        assert_eq!(frame.encode().unwrap(), expected);
        assert_eq!(frame.meta.crc32, 907_060_870);
    }

    #[test]
    fn golden_vector_round_trip() {
        let frame = Frame::new(sample_meta(), b"hello");
        let bytes = frame.encode().unwrap();
        let mut cursor = std::io::Cursor::new(bytes);
        let back =
            read_frame(&mut cursor, crate::frames::limits::MAX_PAYLOAD_BYTES as u64).unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn round_trip_all_targets_and_formats() {
        for target in ["keys", "dial", "key:0", "key:14"] {
            for format in [FrameFormat::Png, FrameFormat::Jpeg, FrameFormat::Gif] {
                let mut m = FrameMeta {
                    target: target.into(),
                    seq: 42,
                    ts_ms: 1,
                    format,
                    w: Some(72),
                    h: Some(72),
                    frames: Some(300),
                    fps: Some(30.0),
                    loop_: Some(true),
                    crc32: 0,
                    max_payload_bytes: crate::frames::limits::MAX_PAYLOAD_BYTES as u32,
                };
                m.crc32 = crc32(&[0xAB; 1000]);
                let frame = Frame {
                    meta: m,
                    payload: vec![0xAB; 1000],
                };
                let bytes = frame.encode().unwrap();
                let mut cursor = std::io::Cursor::new(bytes);
                let back = read_frame(&mut cursor, crate::frames::limits::MAX_PAYLOAD_BYTES as u64)
                    .unwrap();
                assert_eq!(back, frame);
            }
        }
    }

    #[test]
    fn minimal_meta_round_trip() {
        let m = json!({"target":"dial","seq":1,"ts_ms":5,"format":"jpeg","crc32":0,"max_payload_bytes":16777216});
        let mut meta = FrameMeta::from_value(&m).unwrap();
        meta.crc32 = crc32(b"x");
        let frame = Frame::new(meta, b"x");
        let bytes = frame.encode().unwrap();
        let mut cursor = std::io::Cursor::new(bytes);
        let back = read_frame(&mut cursor, 16777216).unwrap();
        assert_eq!(back.meta.target, "dial");
        assert!(back.meta.w.is_none());
    }

    #[test]
    fn meta_validated_against_schema() {
        // 'panel' is not a target (S6)
        let bad = json!({"target":"panel","seq":1,"ts_ms":1,"format":"png","crc32":1,"max_payload_bytes":16777216});
        assert!(FrameMeta::from_value(&bad).is_err());
        // 301 frames rejected
        let bad = json!({"target":"keys","seq":1,"ts_ms":1,"format":"gif","frames":301,"crc32":1,"max_payload_bytes":16777216});
        assert!(FrameMeta::from_value(&bad).is_err());
        // unknown property rejected (additionalProperties: false)
        let bad = json!({"target":"keys","seq":1,"ts_ms":1,"format":"png","crc32":1,"max_payload_bytes":16777216,"evil":true});
        assert!(FrameMeta::from_value(&bad).is_err());
        // fps above 30 rejected
        let bad = json!({"target":"keys","seq":1,"ts_ms":1,"format":"gif","fps":31,"crc32":1,"max_payload_bytes":16777216});
        assert!(FrameMeta::from_value(&bad).is_err());
        // w above 4096 rejected (decode cap)
        let bad = json!({"target":"keys","seq":1,"ts_ms":1,"format":"png","w":4097,"h":1,"crc32":1,"max_payload_bytes":16777216});
        assert!(FrameMeta::from_value(&bad).is_err());
        // the fixture-gate example validates
        let good = json!({"target":"keys","seq":7,"ts_ms":1732000000000u64,"format":"png","w":480,"h":272,"frames":1,"crc32":123456,"max_payload_bytes":16777216});
        assert!(FrameMeta::from_value(&good).is_ok());
    }

    #[test]
    fn decode_rejects_bad_magic_meta_and_caps() {
        let frame = Frame::new(sample_meta(), b"hello");
        let bytes = frame.encode().unwrap();

        // bad magic
        let mut bad = bytes.clone();
        bad[0] = b'X';
        assert!(matches!(
            read_frame(&mut std::io::Cursor::new(bad), 16777216),
            Err(DecodeError::BadMagic)
        ));

        // meta_len over the cap -> E_LIMIT without reading the meta
        let mut bad = Vec::new();
        bad.extend_from_slice(DSF1_MAGIC);
        bad.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            read_frame(&mut std::io::Cursor::new(bad), 16777216),
            Err(DecodeError::MetaTooLong)
        ));

        // payload_len over the wire cap -> E_LIMIT before reading the payload
        let mut bad = bytes.clone();
        let plen_at = 4 + 4 + u32::from_le_bytes([136, 0, 0, 0]) as usize; // meta_len = 136
        bad[plen_at..plen_at + 8].copy_from_slice(&(16777216u64 + 1).to_le_bytes());
        let err = read_frame(&mut std::io::Cursor::new(bad), 16777216).unwrap_err();
        assert!(matches!(err, DecodeError::PayloadTooLong(7)));
        assert_eq!(err.to_ipc_error().code, ErrorCode::ELimit);

        // crc mismatch
        let mut bad = bytes.clone();
        let last = bad.len() - 1;
        bad[last] ^= 0xFF;
        let err = read_frame(&mut std::io::Cursor::new(bad), 16777216).unwrap_err();
        assert!(matches!(err, DecodeError::CrcMismatch));
        assert_eq!(err.to_ipc_error().code, ErrorCode::ESchema);

        // wrong max_payload_bytes constant
        let mut bad = bytes.clone();
        let marker = b"\"max_payload_bytes\":16777216".to_vec();
        let at = find_subslice(&bad, &marker).unwrap();
        bad[at + 19..at + 27].copy_from_slice(b"16777215");
        // fix meta_len unchanged (same length); crc of payload still fine
        let err = read_frame(&mut std::io::Cursor::new(bad), 16777216).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::BadMaxPayload | DecodeError::BadMeta(_)
        ));

        // truncated stream -> Eof
        let err = read_frame(&mut std::io::Cursor::new(&bytes[..10]), 16777216).unwrap_err();
        assert!(matches!(err, DecodeError::Eof));
    }

    #[test]
    fn crc32_matches_reference_values() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"hello"), 907_060_870); // zlib.crc32(b"hello")
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926); // canonical CRC32 check value
    }

    #[test]
    fn dsctl_and_dsf1_prefixes_are_distinguishable() {
        assert!(is_frame_prefix(b"DSF1"));
        assert!(!is_dsctl_prefix(b"DSF1"));
        assert!(is_dsctl_prefix(b"DSctl\n{\"op\":\"stream-open\""));
        assert!(!is_frame_prefix(b"DSctl\n"));
    }

    fn hex_decode(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }
}
