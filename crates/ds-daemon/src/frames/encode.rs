//! Re-encode any accepted wire image into the device wire format: **baseline JPEG,
//! standard Huffman tables, 4:4:4 chroma** (`docs/contracts/streaming.md` §2,
//! `upstream/PROTOCOL.md:87-96`), quality per target (panel q80, `key:N` q90, dial q85).
//!
//! Accepted wire payloads are PNG, JPEG or GIF (≤ 16 MB, decode caps ≤ 4096×4096 px —
//! `schemas/stream-frame.json`). Everything is re-encoded, so the bytes on the device wire
//! depend only on the decoded pixels, the target and the pinned `jpeg-encoder` version —
//! that is what makes the golden vectors in `tests/golden/` exact-byte fixtures.
//!
//! JPEG input is structurally validated before decoding: only baseline (SOF0) with the
//! standard Annex K Huffman tables is accepted. Progressive (SOF2) or optimized-Huffman
//! (non-standard DHT) input is rejected with [`EncodeError::JpegNotBaseline`] /
//! [`EncodeError::JpegOptimizedHuffman`]; every [`EncodeError`] maps to the `E_FORMAT` error
//! code and the frame is dropped (counted), never forwarded.

use std::fmt;
use std::io::Cursor;

use crate::frames::FrameTarget;

/// Wire payload cap: 16 MB (`docs/contracts/streaming.md` §2/§5, `schemas/stream-frame.json`
/// `max_payload_bytes`).
pub const MAX_WIRE_BYTES: usize = 16 * 1024 * 1024;

/// Decode cap per axis: 4096 px (`docs/contracts/streaming.md` §2).
pub const MAX_DECODE_PX: u32 = 4096;

/// The wire image formats the frame plane accepts (`schemas/stream-frame.json` `format`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireFormat {
    Png,
    Jpeg,
    Gif,
}

impl WireFormat {
    pub fn name(self) -> &'static str {
        match self {
            WireFormat::Png => "png",
            WireFormat::Jpeg => "jpeg",
            WireFormat::Gif => "gif",
        }
    }
}

/// Detect the wire format from the magic bytes.
pub fn detect_format(bytes: &[u8]) -> Option<WireFormat> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some(WireFormat::Png)
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(WireFormat::Jpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(WireFormat::Gif)
    } else {
        None
    }
}

/// Why a wire frame was refused. **Every variant maps to the `E_FORMAT` error code**
/// (`docs/contracts/streaming.md` §2): the frame is dropped and counted, never forwarded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// Empty payload.
    Empty,
    /// Payload over the 16 MB wire cap.
    PayloadTooLarge { len: usize, max: usize },
    /// Magic bytes are none of PNG/JPEG/GIF.
    UnsupportedFormat,
    /// The container decoded badly (truncated/corrupt).
    DecodeFailed { format: WireFormat, reason: String },
    /// JPEG whose SOF marker is not SOF0 (progressive SOF2, arithmetic, lossless, …).
    JpegNotBaseline { sof_marker: u8 },
    /// JPEG whose DHT tables are not exactly the standard Annex K tables (optimized
    /// Huffman) — those break the device decoder (`upstream/PROTOCOL.md:87-88`).
    JpegOptimizedHuffman,
    /// Decoded image over the 4096×4096 cap.
    ImageTooLarge { w: u32, h: u32, max: u32 },
    /// The pinned encoder itself refused the pixels (should not happen after the caps).
    EncodeFailed(String),
}

impl EncodeError {
    /// The stable error code this maps to on the control plane: always `E_FORMAT`.
    pub fn code(&self) -> &'static str {
        "E_FORMAT"
    }
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::Empty => write!(f, "empty frame payload"),
            EncodeError::PayloadTooLarge { len, max } => {
                write!(
                    f,
                    "frame payload {len} bytes exceeds the {max}-byte wire cap"
                )
            }
            EncodeError::UnsupportedFormat => {
                write!(f, "payload is not a PNG, JPEG or GIF image")
            }
            EncodeError::DecodeFailed { format, reason } => {
                write!(f, "{} decode failed: {reason}", format.name())
            }
            EncodeError::JpegNotBaseline { sof_marker } => {
                write!(f, "JPEG is not baseline (SOF marker {sof_marker:#04x})")
            }
            EncodeError::JpegOptimizedHuffman => {
                write!(f, "JPEG uses optimized (non-standard) Huffman tables")
            }
            EncodeError::ImageTooLarge { w, h, max } => {
                write!(
                    f,
                    "decoded image {w}x{h} exceeds the {max}x{max} decode cap"
                )
            }
            EncodeError::EncodeFailed(reason) => write!(f, "JPEG encode failed: {reason}"),
        }
    }
}

impl std::error::Error for EncodeError {}

/// Re-encode one accepted wire frame (PNG/JPEG/GIF bytes) to the device wire format for
/// `target`: baseline JPEG, standard Huffman tables, 4:4:4, target quality.
pub fn encode_frame(target: FrameTarget, wire: &[u8]) -> Result<Vec<u8>, EncodeError> {
    if wire.is_empty() {
        return Err(EncodeError::Empty);
    }
    if wire.len() > MAX_WIRE_BYTES {
        return Err(EncodeError::PayloadTooLarge {
            len: wire.len(),
            max: MAX_WIRE_BYTES,
        });
    }
    let format = detect_format(wire).ok_or(EncodeError::UnsupportedFormat)?;
    let (w, h, rgb) = match format {
        WireFormat::Png => decode_png(wire)?,
        WireFormat::Jpeg => decode_jpeg(wire)?,
        WireFormat::Gif => decode_gif(wire)?,
    };
    if w == 0 || h == 0 {
        return Err(EncodeError::Empty);
    }
    if w > MAX_DECODE_PX || h > MAX_DECODE_PX {
        return Err(EncodeError::ImageTooLarge {
            w,
            h,
            max: MAX_DECODE_PX,
        });
    }
    encode_rgb(target, &rgb, w, h)
}

/// Encode a solid-colour screen of the target's native size (the `kind:"color"` visual)
/// straight to the device JPEG — infallible for the fixed target sizes.
pub fn encode_solid(target: FrameTarget, rgb: [u8; 3]) -> Vec<u8> {
    let (w, h) = (target.width(), target.height());
    let mut pixels = vec![0u8; w as usize * h as usize * 3];
    for px in pixels.chunks_exact_mut(3) {
        px.copy_from_slice(&rgb);
    }
    encode_rgb(target, &pixels, w, h).expect("solid target-size image always encodes")
}

/// Encode 8-bit RGB pixels to the device wire format for `target`.
pub fn encode_rgb(target: FrameTarget, rgb: &[u8], w: u32, h: u32) -> Result<Vec<u8>, EncodeError> {
    if w == 0 || h == 0 || w > u32::from(u16::MAX) || h > u32::from(u16::MAX) {
        return Err(EncodeError::ImageTooLarge {
            w,
            h,
            max: MAX_DECODE_PX,
        });
    }
    if rgb.len() < w as usize * h as usize * 3 {
        return Err(EncodeError::EncodeFailed(format!(
            "rgb buffer {} bytes < {}x{}x3",
            rgb.len(),
            w,
            h
        )));
    }
    let mut out: Vec<u8> = Vec::new();
    let mut encoder = jpeg_encoder::Encoder::new(&mut out, target.quality());
    // 4:4:4: no chroma subsampling (the default for q<90 would be 2x2 / 4:2:0).
    encoder.set_sampling_factor(jpeg_encoder::SamplingFactor::F_1_1);
    // Standard Annex K Huffman tables, never optimized — optimized tables break the
    // device decoder (upstream/PROTOCOL.md:87-88). Baseline (non-progressive) is the
    // encoder default; set explicitly so the wire format cannot drift silently.
    encoder.set_optimized_huffman_tables(false);
    encoder.set_progressive(false);
    encoder
        .encode(rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb)
        .map_err(|e| EncodeError::EncodeFailed(e.to_string()))?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// wire decoders (PNG / JPEG / GIF -> 8-bit RGB)
// ---------------------------------------------------------------------------

fn decode_png(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), EncodeError> {
    let fail = |e: png::DecodingError| EncodeError::DecodeFailed {
        format: WireFormat::Png,
        reason: e.to_string(),
    };
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    // palette -> RGB, tRNS -> alpha, 16-bit -> 8-bit: output is Gray/GrayA/RGB/RGBA u8.
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(fail)?;
    let (color, _depth) = reader.output_color_type();
    let Some(size) = reader.output_buffer_size() else {
        return Err(EncodeError::DecodeFailed {
            format: WireFormat::Png,
            reason: "no output buffer size".into(),
        });
    };
    let mut buf = vec![0u8; size];
    reader.next_frame(&mut buf).map_err(fail)?;
    let info = reader.info();
    let (w, h) = (info.width, info.height);
    let rgb = to_rgb8(color, &buf);
    Ok((w, h, rgb))
}

fn decode_jpeg(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), EncodeError> {
    validate_jpeg_baseline_standard(bytes)?;
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(
        Cursor::new(bytes),
        zune_jpeg::zune_core::options::DecoderOptions::default()
            .jpeg_set_out_colorspace(zune_jpeg::zune_core::colorspace::ColorSpace::RGB),
    );
    let fail = |e: zune_jpeg::errors::DecodeErrors| EncodeError::DecodeFailed {
        format: WireFormat::Jpeg,
        reason: e.to_string(),
    };
    let out = decoder.decode().map_err(fail)?;
    let info = decoder.info().ok_or_else(|| EncodeError::DecodeFailed {
        format: WireFormat::Jpeg,
        reason: "no image info after decode".into(),
    })?;
    let (w, h) = (u32::from(info.width), u32::from(info.height));
    Ok((w, h, out))
}

fn decode_gif(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), EncodeError> {
    let fail = |e: gif::DecodingError| EncodeError::DecodeFailed {
        format: WireFormat::Gif,
        reason: e.to_string(),
    };
    // RGBA output lets the gif crate expand the palette for us; first frame only (the
    // animation pipeline is DS-034, out of scope here).
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    let mut decoder = options.read_info(Cursor::new(bytes)).map_err(fail)?;
    if decoder.next_frame_info().map_err(fail)?.is_none() {
        return Err(EncodeError::DecodeFailed {
            format: WireFormat::Gif,
            reason: "no frames".into(),
        });
    }
    let mut buf = vec![0u8; decoder.buffer_size()];
    decoder.read_into_buffer(&mut buf).map_err(fail)?;
    let (w, h) = (u32::from(decoder.width()), u32::from(decoder.height()));
    // RGBA -> RGB
    let mut rgb = Vec::with_capacity(w as usize * h as usize * 3);
    for px in buf.chunks_exact(4) {
        rgb.extend_from_slice(&px[..3]);
    }
    Ok((w, h, rgb))
}

/// Convert a normalized 8-bit PNG buffer to packed RGB8.
fn to_rgb8(color: png::ColorType, buf: &[u8]) -> Vec<u8> {
    match color {
        png::ColorType::Rgb => buf.to_vec(),
        png::ColorType::Rgba => buf
            .chunks_exact(4)
            .flat_map(|px| px[..3].to_vec())
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g]).collect(),
        png::ColorType::GrayscaleAlpha => buf
            .chunks_exact(2)
            .flat_map(|px| [px[0], px[0], px[0]])
            .collect(),
        // EXPAND turns palettes into RGB before we see them.
        png::ColorType::Indexed => unreachable!("palette expanded by normalize_to_color8"),
    }
}

// ---------------------------------------------------------------------------
// JPEG structural validation: baseline SOF0 + standard Huffman tables only
// ---------------------------------------------------------------------------

/// The standard Annex K Huffman tables (JPEG spec K.3, same tables jpeg-encoder writes by
/// default): `(class<<4 | id, [16 code lengths], values)`.
const STANDARD_HUFFMAN_TABLES: [([u8; 16], &[u8]); 4] = [
    (
        [
            0x00, 0x01, 0x05, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00,
        ],
        &[
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B,
        ],
    ),
    (
        [
            0x00, 0x03, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00,
            0x00, 0x00,
        ],
        &[
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B,
        ],
    ),
    (
        [
            0x00, 0x02, 0x01, 0x03, 0x03, 0x02, 0x04, 0x03, 0x05, 0x05, 0x04, 0x04, 0x00, 0x00,
            0x01, 0x7D,
        ],
        &[
            0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51,
            0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xA1, 0x08, 0x23, 0x42, 0xB1, 0xC1,
            0x15, 0x52, 0xD1, 0xF0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0A, 0x16, 0x17, 0x18,
            0x19, 0x1A, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39,
            0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x53, 0x54, 0x55, 0x56, 0x57,
            0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6A, 0x73, 0x74, 0x75,
            0x76, 0x77, 0x78, 0x79, 0x7A, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8A, 0x92,
            0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7,
            0xA8, 0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3,
            0xC4, 0xC5, 0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8,
            0xD9, 0xDA, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xF1, 0xF2,
            0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA,
        ],
    ),
    (
        [
            0x00, 0x02, 0x01, 0x02, 0x04, 0x04, 0x03, 0x04, 0x07, 0x05, 0x04, 0x04, 0x00, 0x01,
            0x02, 0x77,
        ],
        &[
            0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07,
            0x61, 0x71, 0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xA1, 0xB1, 0xC1, 0x09,
            0x23, 0x33, 0x52, 0xF0, 0x15, 0x62, 0x72, 0xD1, 0x0A, 0x16, 0x24, 0x34, 0xE1, 0x25,
            0xF1, 0x17, 0x18, 0x19, 0x1A, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x35, 0x36, 0x37, 0x38,
            0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x53, 0x54, 0x55, 0x56,
            0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6A, 0x73, 0x74,
            0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
            0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5,
            0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA,
            0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6,
            0xD7, 0xD8, 0xD9, 0xDA, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xF2,
            0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA,
        ],
    ),
];

/// Standard table for `(class, id)`: `(0,0)` luma DC, `(0,1)` chroma DC, `(1,0)` luma AC,
/// `(1,1)` chroma AC. `None` for table ids the standard does not define.
fn standard_huffman_table(class: u8, id: u8) -> Option<&'static ([u8; 16], &'static [u8])> {
    match (class, id) {
        (0, 0) => Some(&STANDARD_HUFFMAN_TABLES[0]),
        (0, 1) => Some(&STANDARD_HUFFMAN_TABLES[1]),
        (1, 0) => Some(&STANDARD_HUFFMAN_TABLES[2]),
        (1, 1) => Some(&STANDARD_HUFFMAN_TABLES[3]),
        _ => None,
    }
}

/// Structurally validate a JPEG: SOI, baseline SOF0 (any other SOF → not baseline), and
/// every DHT table exactly equal to its standard Annex K counterpart (anything else is an
/// optimized table → reject). Entropy-coded data after SOS is not scanned.
pub fn validate_jpeg_baseline_standard(data: &[u8]) -> Result<(), EncodeError> {
    if !data.starts_with(&[0xFF, 0xD8]) {
        return Err(EncodeError::DecodeFailed {
            format: WireFormat::Jpeg,
            reason: "no SOI marker".into(),
        });
    }
    let mut pos = 2usize;
    let mut sof: Option<u8> = None;
    let mut saw_dht = false;
    while pos < data.len() {
        if data[pos] != 0xFF {
            return Err(EncodeError::DecodeFailed {
                format: WireFormat::Jpeg,
                reason: format!("expected a marker at byte {pos}"),
            });
        }
        let mut m = pos + 1;
        while m < data.len() && data[m] == 0xFF {
            m += 1; // fill bytes
        }
        if m >= data.len() {
            break;
        }
        let marker = data[m];
        pos = m + 1;
        // Standalone markers carry no length.
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            continue;
        }
        if pos + 2 > data.len() {
            return Err(EncodeError::DecodeFailed {
                format: WireFormat::Jpeg,
                reason: "truncated segment length".into(),
            });
        }
        let len = usize::from(u16::from_be_bytes([data[pos], data[pos + 1]]));
        if len < 2 || pos + len > data.len() {
            return Err(EncodeError::DecodeFailed {
                format: WireFormat::Jpeg,
                reason: "segment length overruns the payload".into(),
            });
        }
        let seg = &data[pos + 2..pos + len];
        match marker {
            0xDA => break, // SOS: all header segments are behind us
            0xC4 => {
                saw_dht = true;
                parse_dht(seg)?;
            }
            0xC0 => {
                if sof.is_none() {
                    sof = Some(marker);
                }
            }
            0xC1..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                return Err(EncodeError::JpegNotBaseline { sof_marker: marker });
            }
            _ => {}
        }
        pos += len;
    }
    match sof {
        None => Err(EncodeError::DecodeFailed {
            format: WireFormat::Jpeg,
            reason: "no SOF segment".into(),
        }),
        Some(0xC0) if saw_dht => Ok(()),
        Some(0xC0) => Err(EncodeError::JpegOptimizedHuffman), // no tables at all: not standard
        Some(marker) => Err(EncodeError::JpegNotBaseline { sof_marker: marker }),
    }
}

/// Check every table in one DHT segment against the standard tables.
fn parse_dht(seg: &[u8]) -> Result<(), EncodeError> {
    let mut pos = 0usize;
    while pos < seg.len() {
        if pos + 17 > seg.len() {
            return Err(EncodeError::DecodeFailed {
                format: WireFormat::Jpeg,
                reason: "truncated DHT table header".into(),
            });
        }
        let tcth = seg[pos];
        let counts: [u8; 16] = seg[pos + 1..pos + 17].try_into().expect("17 bytes sliced");
        let n = counts.iter().map(|&c| usize::from(c)).sum::<usize>();
        if pos + 17 + n > seg.len() {
            return Err(EncodeError::DecodeFailed {
                format: WireFormat::Jpeg,
                reason: "truncated DHT table values".into(),
            });
        }
        let values = &seg[pos + 17..pos + 17 + n];
        let Some((std_counts, std_values)) = standard_huffman_table(tcth >> 4, tcth & 0x0F) else {
            // A table id the standard does not define is by definition not standard.
            return Err(EncodeError::JpegOptimizedHuffman);
        };
        if counts != *std_counts || values != *std_values {
            return Err(EncodeError::JpegOptimizedHuffman);
        }
        pos += 17 + n;
    }
    Ok(())
}

/// A tiny deterministic RGB image (gradient + blocks) shared by the frames tests.
#[cfg(test)]
pub fn test_rgb(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(w as usize * h as usize * 3);
    for y in 0..h {
        for x in 0..w {
            v.extend_from_slice(&[(x * 16 + y) as u8, (y * 16 + x) as u8, ((x + y) * 8) as u8]);
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_baseline(w: u32, h: u32, quality: u8) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = jpeg_encoder::Encoder::new(&mut out, quality);
        enc.set_sampling_factor(jpeg_encoder::SamplingFactor::F_1_1);
        enc.set_optimized_huffman_tables(false);
        enc.set_progressive(false);
        enc.encode(
            &test_rgb(w, h),
            w as u16,
            h as u16,
            jpeg_encoder::ColorType::Rgb,
        )
        .unwrap();
        out
    }

    #[test]
    fn format_detection_by_magic() {
        assert_eq!(
            detect_format(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0]),
            Some(WireFormat::Png)
        );
        assert_eq!(
            detect_format(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some(WireFormat::Jpeg)
        );
        assert_eq!(detect_format(b"GIF89a...."), Some(WireFormat::Gif));
        assert_eq!(detect_format(b"GIF87a...."), Some(WireFormat::Gif));
        assert_eq!(detect_format(b"not an image"), None);
        assert_eq!(detect_format(&[]), None);
    }

    #[test]
    fn encoded_output_is_baseline_standard_444_jpeg() {
        let jpeg = encode_baseline(72, 72, 90);
        assert!(jpeg.starts_with(&[0xFF, 0xD8]));
        assert!(jpeg.ends_with(&[0xFF, 0xD9]));
        // our own output must pass the same structural check the wire input goes through
        validate_jpeg_baseline_standard(&jpeg).unwrap();
        // SOF0 present, no other SOF; sampling factors 1x1 (4:4:4) in the SOF segment
        let sof_pos = find_marker(&jpeg, 0xC0).expect("SOF0");
        let seg = &jpeg[sof_pos + 4
            ..sof_pos + 2 + u16::from_be_bytes([jpeg[sof_pos + 2], jpeg[sof_pos + 3]]) as usize];
        assert_eq!(seg[0], 8, "8-bit precision");
        let ncomp = seg[5];
        assert_eq!(ncomp, 3, "YCbCr color JPEG");
        for c in 0..ncomp as usize {
            let hv = seg[7 + c * 3]; // per component: id, hv, tq — hv is the middle byte
            assert_eq!(hv >> 4, 1, "horizontal sampling 1");
            assert_eq!(hv & 0x0F, 1, "vertical sampling 1");
        }
        // DHT segments carry exactly the four standard tables (luma/chroma DC+AC)
        let mut dht = collect_dht(&jpeg);
        dht.sort_unstable();
        assert_eq!(dht, vec![0x00, 0x01, 0x10, 0x11]);
    }

    fn find_marker(data: &[u8], marker: u8) -> Option<usize> {
        let mut i = 0;
        while i + 1 < data.len() {
            if data[i] == 0xFF && data[i + 1] == marker {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    fn collect_dht(data: &[u8]) -> Vec<u8> {
        // one tc/th byte per table in every DHT segment before SOS
        let mut tables = Vec::new();
        let mut pos = 2;
        while pos + 4 < data.len() {
            if data[pos] != 0xFF {
                break;
            }
            let marker = data[pos + 1];
            if marker == 0xDA {
                break;
            }
            let len = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
            if marker == 0xC4 {
                let seg = &data[pos + 4..pos + 2 + len];
                let mut p = 0;
                while p < seg.len() {
                    tables.push(seg[p]);
                    let n = seg[p + 1..p + 17]
                        .iter()
                        .map(|&c| usize::from(c))
                        .sum::<usize>();
                    p += 17 + n;
                }
            }
            pos += 2 + len;
        }
        tables
    }

    #[test]
    fn encode_frame_round_trips_a_png_input() {
        // build a small PNG with the png crate encoder (test-only)
        let (w, h) = (24u32, 16u32);
        let rgb = test_rgb(w, h);
        let mut png_bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png_bytes, w, h);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().unwrap();
            writer.write_image_data(&rgb).unwrap();
        }
        let jpeg = encode_frame(FrameTarget::Key(0), &png_bytes).unwrap();
        assert!(validate_jpeg_baseline_standard(&jpeg).is_ok());
        // determinism: same input -> same bytes
        assert_eq!(jpeg, encode_frame(FrameTarget::Key(0), &png_bytes).unwrap());
    }

    #[test]
    fn encode_frame_accepts_our_own_baseline_jpeg_and_reencodes() {
        let src = encode_baseline(32, 32, 75);
        let jpeg = encode_frame(FrameTarget::Dial, &src).unwrap();
        assert!(validate_jpeg_baseline_standard(&jpeg).is_ok());
        // re-encoded at the dial quality, so not byte-identical to the q75 source
        assert_ne!(jpeg, src);
    }

    #[test]
    fn encode_frame_accepts_a_gif_input() {
        // 4x4 solid first colour, produced with the gif crate's own encoder (test-only)
        let mut gif_bytes = Vec::new();
        {
            let mut enc =
                gif::Encoder::new(&mut gif_bytes, 4, 4, &[0xFF, 0x00, 0x00, 0x00, 0x00, 0x00])
                    .unwrap();
            let frame = gif::Frame {
                width: 4,
                height: 4,
                buffer: std::borrow::Cow::Borrowed(&[0u8; 16]),
                ..Default::default()
            };
            enc.write_frame(&frame).unwrap();
        }
        assert_eq!(detect_format(&gif_bytes), Some(WireFormat::Gif));
        let (w, h, rgb) = decode_gif(&gif_bytes).unwrap();
        assert_eq!((w, h), (4, 4));
        assert_eq!(rgb.len(), 4 * 4 * 3);
        assert!(rgb.chunks_exact(3).all(|px| px == [0xFF, 0x00, 0x00]));
        let jpeg = encode_frame(FrameTarget::Key(3), &gif_bytes).unwrap();
        assert!(validate_jpeg_baseline_standard(&jpeg).is_ok());
    }

    #[test]
    fn rejections_map_to_e_format() {
        // empty / oversized / unknown magic
        assert_eq!(
            encode_frame(FrameTarget::Panel, &[]),
            Err(EncodeError::Empty)
        );
        let big = vec![0u8; MAX_WIRE_BYTES + 1];
        assert_eq!(
            encode_frame(FrameTarget::Panel, &big),
            Err(EncodeError::PayloadTooLarge {
                len: MAX_WIRE_BYTES + 1,
                max: MAX_WIRE_BYTES
            })
        );
        assert_eq!(
            encode_frame(FrameTarget::Panel, b"\x00\x01\x02\x03garbage"),
            Err(EncodeError::UnsupportedFormat)
        );
        // every error carries the E_FORMAT code
        for e in [
            EncodeError::Empty,
            EncodeError::PayloadTooLarge { len: 1, max: 0 },
            EncodeError::UnsupportedFormat,
            EncodeError::DecodeFailed {
                format: WireFormat::Png,
                reason: "x".into(),
            },
            EncodeError::JpegNotBaseline { sof_marker: 0xC2 },
            EncodeError::JpegOptimizedHuffman,
            EncodeError::ImageTooLarge { w: 1, h: 1, max: 0 },
            EncodeError::EncodeFailed("x".into()),
        ] {
            assert_eq!(e.code(), "E_FORMAT");
            assert!(!e.to_string().is_empty());
        }
    }

    #[test]
    fn progressive_jpeg_is_rejected() {
        let mut jpeg = encode_baseline(32, 32, 80);
        // flip SOF0 (0xC0) to SOF2 (progressive)
        let pos = find_marker(&jpeg, 0xC0).unwrap();
        jpeg[pos + 1] = 0xC2;
        assert_eq!(
            encode_frame(FrameTarget::Panel, &jpeg),
            Err(EncodeError::JpegNotBaseline { sof_marker: 0xC2 })
        );
    }

    #[test]
    fn optimized_huffman_jpeg_is_rejected() {
        let jpeg = encode_baseline(32, 32, 80);
        // rewrite the first DHT table's code lengths so they differ from the standard table
        let mut pos = 2;
        let mut patched = jpeg.clone();
        while pos + 4 < patched.len() {
            if patched[pos] != 0xFF {
                break;
            }
            let marker = patched[pos + 1];
            if marker == 0xDA {
                break;
            }
            let len = u16::from_be_bytes([patched[pos + 2], patched[pos + 3]]) as usize;
            if marker == 0xC4 {
                // first table: tc/th at pos+4, 16 counts at pos+5..pos+21, values from pos+21;
                // corrupt one value (not a count, so the table framing stays parseable)
                patched[pos + 21] = patched[pos + 21].wrapping_add(1);
                break;
            }
            pos += 2 + len;
        }
        assert_eq!(
            encode_frame(FrameTarget::Key(1), &patched),
            Err(EncodeError::JpegOptimizedHuffman)
        );
        // the unpatched original is accepted
        assert!(encode_frame(FrameTarget::Key(1), &jpeg).is_ok());
    }

    #[test]
    fn decode_caps_are_enforced() {
        // a PNG header claiming 4097 px width is refused before decoding
        let mut png_bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png_bytes, 4097, 1);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().unwrap();
            let row = vec![0u8; 4097 * 3];
            writer.write_image_data(&row).unwrap();
        }
        assert_eq!(
            encode_frame(FrameTarget::Dial, &png_bytes),
            Err(EncodeError::ImageTooLarge {
                w: 4097,
                h: 1,
                max: MAX_DECODE_PX
            })
        );
    }

    #[test]
    fn solid_color_encodes_at_target_size_and_quality() {
        for target in [FrameTarget::Panel, FrameTarget::Key(5), FrameTarget::Dial] {
            let jpeg = encode_solid(target, [0x3c, 0xc8, 0x6e]);
            assert!(validate_jpeg_baseline_standard(&jpeg).is_ok());
            // SOF carries the target's native dimensions
            let pos = find_marker(&jpeg, 0xC0).unwrap();
            let seg = &jpeg[pos + 4..];
            let h = u16::from_be_bytes([seg[1], seg[2]]) as u32;
            let w = u16::from_be_bytes([seg[3], seg[4]]) as u32;
            assert_eq!((w, h), (target.width(), target.height()));
        }
        // deterministic
        assert_eq!(
            encode_solid(FrameTarget::Key(0), [1, 2, 3]),
            encode_solid(FrameTarget::Key(0), [1, 2, 3])
        );
    }

    #[test]
    fn quality_differs_per_target() {
        let rgb = test_rgb(72, 72);
        let panel = encode_rgb(FrameTarget::Panel, &rgb, 72, 72).unwrap();
        let key = encode_rgb(FrameTarget::Key(0), &rgb, 72, 72).unwrap();
        let dial = encode_rgb(FrameTarget::Dial, &rgb, 72, 72).unwrap();
        assert_ne!(panel, key);
        assert_ne!(key, dial);
        // higher quality => larger (or equal) file for this image
        assert!(key.len() >= dial.len());
        assert!(dial.len() >= panel.len());
    }
}
