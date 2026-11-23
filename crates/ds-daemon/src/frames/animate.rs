//! The animation plane (DS-034): multi-frame decode of **animated GIF / APNG** and of a
//! **directory of stills at `--fps`**, the contractual animation limits, frame pacing, and
//! the paced push of the decoded frames through the DS-031 frame service
//! (`docs/contracts/streaming.md` §2/§5/§7, `docs/components/frame-service.md`,
//! `schemas/stream-frame.json`).
//!
//! - **Decode caps (enforced here, at decode time):** wire payload ≤ 16 MB
//!   ([`MAX_PAYLOAD_BYTES`], checked *before* any decoding), decoded dimensions ≤
//!   4096×4096 ([`MAX_DECODE_PX`]), ≤ 300 frames ([`MAX_ANIMATION_FRAMES`]), ≤ 30 fps
//!   ([`MAX_FPS`]), minimum frame duration 0.02 s ([`MIN_FRAME_INTERVAL`]), and a bounded
//!   total decoded size ([`MAX_TOTAL_DECODE_BYTES`]) so a small wire file cannot decompress
//!   into unbounded memory — one frame is decoded at a time, never the whole animation at
//!   once. Over-limit input is rejected with the documented codes: [`AnimationError::code`]
//!   maps decode failures and over-decode-caps to **`E_FORMAT`**, and over-count / over-fps /
//!   under-duration / over-wire / over-total-bytes to **`E_LIMIT`** — the frame never reaches
//!   the device.
//! - **Queue caps (enforced in the DS-031 service, `frames::service`):** per-target queue
//!   depth 2 (1 in flight + 1 waiting, latest-frame-wins) and the global 256 MiB frame-queue
//!   budget. Every target — the panel `keys`, **each `key:N` with its own queue**, and the
//!   `dial` — runs under the *same* caps values; `key:N` and the panel share one module (one
//!   writer thread), the dial another, but the caps are per-target either way. The decode caps
//!   above are what this module adds on top: they bound what may enter those queues.
//! - **Pacing:** [`Animation::interval`] is `max(0.02 s, 1/fps)`; [`push_animation`] sends the
//!   **first frame immediately** and then one frame per interval, so a producer never bursts
//!   past 30 fps. Each frame still goes through the service queue individually, so
//!   latest-frame-wins is preserved: a slow device drops *waiting* frames (counted in
//!   `telemetry.dropped`) and never tears the in-flight one (streaming.md §3).
//! - **Loop meta:** the GIF NETSCAPE / APNG `num_plays` loop flag is preserved into the DSF1
//!   meta (`loop`, with `frames`/`fps`) by [`Animation::metas`]; a producer `--loop` overrides
//!   it to `true`.
//!
//! GIF frames shorter than the 0.02 s contract minimum are not emitted as sub-0.02 s frames:
//! their delays are merged into the next emitted frame (ticket DS-034 implementation note).
//!
//! Wiring note: the stream-socket call site (`FrameSink::on_frame` in `frames::service`)
//! expands an incoming animated payload with [`decode_animation`] + [`push_animation`]; the
//! function pair is complete and queue-tested here against the real DS-031 service and ds-vdev.

use super::service::FrameService;
use super::{encode, FrameTarget};
use ds_ipc::frames::codec::FrameMeta;
use ds_ipc::frames::limits::{MAX_ANIMATION_FRAMES, MAX_PAYLOAD_BYTES};
use ds_ipc::frames::FrameFormat;
use std::fmt;
use std::io::Cursor;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Contract playback-rate cap: **≤ 30 fps** (`docs/contracts/streaming.md` §2, schema `fps`
/// maximum 30) — i.e. a minimum of 0.02 s between two frames.
pub const MAX_FPS: u32 = 30;

/// Contract minimum frame duration: **0.02 s** (`MIN_FRAME_INTERVAL` in
/// `ds_ipc::frames::limits`).
pub const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(20);

/// The same minimum as milliseconds.
pub const MIN_FRAME_INTERVAL_MS: u64 = 20;

/// Decode cap per axis: 4096 px (`docs/contracts/streaming.md` §2).
pub const MAX_DECODE_PX: u32 = encode::MAX_DECODE_PX;

/// Bound on the total decoded animation size (sum of frame RGB buffers): 256 MiB, matching
/// the frame-queue budget scale (`docs/contracts/streaming.md` §5). A ≤ 16 MB wire file can
/// otherwise decompress to gigabytes; decode stays bounded by refusing beyond this.
pub const MAX_TOTAL_DECODE_BYTES: usize = 256 * 1024 * 1024;

/// Why an animation input was refused. [`AnimationError::code`] gives the stable wire code:
/// **`E_FORMAT`** for undecodable input / over the decode caps, **`E_LIMIT`** for over the
/// animation limits (frame count / fps / frame duration / wire bytes / total decoded bytes) —
/// `docs/contracts/streaming.md` §2/§5, ticket DS-034.
#[derive(Debug, Clone, PartialEq)]
pub enum AnimationError {
    /// Empty payload / empty animation.
    Empty,
    /// Wire payload over the 16 MB cap — checked before any decoding.
    PayloadTooLarge { len: usize, max: usize },
    /// More than [`MAX_ANIMATION_FRAMES`] frames.
    TooManyFrames { n: u32, max: u32 },
    /// Requested playback rate over [`MAX_FPS`] (or zero).
    FpsTooHigh { fps: f64, max: u32 },
    /// A frame shorter than the 0.02 s contract minimum survived merging.
    FrameTooShort { delay_ms: u64, min: u64 },
    /// Total decoded frame bytes over [`MAX_TOTAL_DECODE_BYTES`].
    TotalDecodeTooLarge { bytes: usize, max: usize },
    /// Magic bytes are none of PNG/JPEG/GIF (or the directory holds a non-image).
    UnsupportedFormat,
    /// The container decoded badly (truncated/corrupt/over memory limit).
    DecodeFailed {
        format: &'static str,
        reason: String,
    },
    /// Decoded image over the 4096×4096 decode cap.
    ImageTooLarge { w: u32, h: u32, max: u32 },
}

impl AnimationError {
    /// The stable error code this maps to: `E_FORMAT` or `E_LIMIT`.
    pub fn code(&self) -> &'static str {
        match self {
            AnimationError::UnsupportedFormat
            | AnimationError::DecodeFailed { .. }
            | AnimationError::ImageTooLarge { .. }
            | AnimationError::Empty => "E_FORMAT",
            AnimationError::PayloadTooLarge { .. }
            | AnimationError::TooManyFrames { .. }
            | AnimationError::FpsTooHigh { .. }
            | AnimationError::FrameTooShort { .. }
            | AnimationError::TotalDecodeTooLarge { .. } => "E_LIMIT",
        }
    }
}

impl fmt::Display for AnimationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AnimationError::Empty => write!(f, "empty animation payload"),
            AnimationError::PayloadTooLarge { len, max } => {
                write!(
                    f,
                    "animation payload {len} bytes exceeds the {max}-byte wire cap"
                )
            }
            AnimationError::TooManyFrames { n, max } => {
                write!(f, "animation has {n} frames, over the {max}-frame cap")
            }
            AnimationError::FpsTooHigh { fps, max } => {
                write!(f, "animation rate {fps} fps exceeds the {max} fps cap")
            }
            AnimationError::FrameTooShort { delay_ms, min } => {
                write!(
                    f,
                    "frame duration {delay_ms} ms is under the {min} ms minimum"
                )
            }
            AnimationError::TotalDecodeTooLarge { bytes, max } => {
                write!(
                    f,
                    "decoded animation {bytes} bytes exceeds the {max}-byte total cap"
                )
            }
            AnimationError::UnsupportedFormat => {
                write!(f, "payload is not a PNG, JPEG or GIF image")
            }
            AnimationError::DecodeFailed { format, reason } => {
                write!(f, "{format} decode failed: {reason}")
            }
            AnimationError::ImageTooLarge { w, h, max } => {
                write!(
                    f,
                    "decoded image {w}x{h} exceeds the {max}x{max} decode cap"
                )
            }
        }
    }
}

impl std::error::Error for AnimationError {}

/// One decoded animation frame: full-canvas 8-bit RGB plus its display duration.
#[derive(Debug, Clone)]
pub struct AnimFrame {
    /// Packed RGB8, `w * h * 3` bytes, row-major, full canvas.
    pub rgb: Vec<u8>,
    /// Display duration of this frame in milliseconds (≥ 20 ms after merging, when the
    /// animation has more than one frame).
    pub delay_ms: u64,
}

/// A decoded animation: frames, canvas size, nominal playback rate, loop flag and the source
/// wire format (for the DSF1 meta).
#[derive(Debug, Clone)]
pub struct Animation {
    /// Decoded frames in playback order (≤ [`MAX_ANIMATION_FRAMES`]).
    pub frames: Vec<AnimFrame>,
    /// Canvas width in pixels (≤ [`MAX_DECODE_PX`]).
    pub w: u32,
    /// Canvas height in pixels (≤ [`MAX_DECODE_PX`]).
    pub h: u32,
    /// Nominal playback rate (`Some` for multi-frame input, `None` for a single still).
    pub fps: Option<f64>,
    /// Loop flag (GIF NETSCAPE / APNG `num_plays`, or a producer `--loop` override).
    pub loop_: bool,
    /// The source wire format, mapped onto the DSF1 meta `format`.
    pub format: FrameFormat,
}

impl Animation {
    /// Number of frames.
    pub fn frame_count(&self) -> u32 {
        self.frames.len() as u32
    }

    /// The pacing interval between two frames: `max(0.02 s, 1/fps)`
    /// (`docs/contracts/streaming.md` §2).
    pub fn interval(&self) -> Duration {
        Duration::from_millis(self.interval_ms())
    }

    /// The pacing interval in milliseconds.
    pub fn interval_ms(&self) -> u64 {
        match self.fps {
            Some(f) if f > 0.0 => (1000.0 / f).round().max(MIN_FRAME_INTERVAL_MS as f64) as u64,
            _ => MIN_FRAME_INTERVAL_MS,
        }
    }

    /// Total decoded frame bytes (against [`MAX_TOTAL_DECODE_BYTES`]).
    pub fn total_bytes(&self) -> usize {
        self.frames.iter().map(|f| f.rgb.len()).sum()
    }

    /// Enforce the contractual animation limits (decode side). Called by
    /// [`decode_animation`] and by [`push_animation`] before anything is queued.
    pub fn validate(&self) -> Result<(), AnimationError> {
        let n = self.frames.len() as u32;
        if n == 0 {
            return Err(AnimationError::Empty);
        }
        if n > MAX_ANIMATION_FRAMES {
            return Err(AnimationError::TooManyFrames {
                n,
                max: MAX_ANIMATION_FRAMES,
            });
        }
        if self.w > MAX_DECODE_PX || self.h > MAX_DECODE_PX {
            return Err(AnimationError::ImageTooLarge {
                w: self.w,
                h: self.h,
                max: MAX_DECODE_PX,
            });
        }
        if let Some(f) = self.fps {
            if !(1.0..=f64::from(MAX_FPS)).contains(&f) {
                return Err(AnimationError::FpsTooHigh {
                    fps: f,
                    max: MAX_FPS,
                });
            }
        }
        if n > 1 {
            for fr in &self.frames {
                if fr.delay_ms < MIN_FRAME_INTERVAL_MS {
                    return Err(AnimationError::FrameTooShort {
                        delay_ms: fr.delay_ms,
                        min: MIN_FRAME_INTERVAL_MS,
                    });
                }
            }
        }
        let total = self.total_bytes();
        if total > MAX_TOTAL_DECODE_BYTES {
            return Err(AnimationError::TotalDecodeTooLarge {
                bytes: total,
                max: MAX_TOTAL_DECODE_BYTES,
            });
        }
        Ok(())
    }

    /// The DSF1 metas for this animation (one per frame), per `schemas/stream-frame.json`:
    /// `frames` = frame count, `fps` = nominal rate, `loop` preserved, per-frame `seq` =
    /// `seq_base + i` and `ts_ms` = `ts_base + i * interval` (paced, first frame at
    /// `ts_base`). `crc32` is left 0 — [`ds_ipc::frames::codec::Frame::new`] fills it from the
    /// payload.
    pub fn metas(&self, target: &str, seq_base: u64, ts_base_ms: u64) -> Vec<FrameMeta> {
        let n = self.frame_count();
        let interval = self.interval_ms();
        self.frames
            .iter()
            .enumerate()
            .map(|(i, _)| FrameMeta {
                target: target.to_string(),
                seq: seq_base + i as u64,
                ts_ms: ts_base_ms + i as u64 * interval,
                format: self.format,
                w: Some(self.w),
                h: Some(self.h),
                frames: Some(n),
                fps: self.fps,
                loop_: if self.loop_ { Some(true) } else { None },
                crc32: 0,
                max_payload_bytes: MAX_PAYLOAD_BYTES as u32,
            })
            .collect()
    }

    /// Re-encode every frame to the device wire format (baseline JPEG, standard Huffman,
    /// 4:4:4, target quality) — the bytes [`push_animation`] queues with `pre_encoded = true`.
    pub fn encode_frames(&self, target: FrameTarget) -> Result<Vec<Vec<u8>>, encode::EncodeError> {
        self.frames
            .iter()
            .map(|fr| encode::encode_rgb(target, &fr.rgb, self.w, self.h))
            .collect()
    }
}

/// Decode one wire payload into an animation: animated GIF / APNG expand to all their frames
/// (delays, loop count); a still PNG/JPEG (or a single-frame GIF) is a 1-frame animation.
/// `loop_override` is the producer `--loop` flag (`Some(true)` forces looping).
///
/// Limits are enforced here (decode side, see module docs): the 16 MB wire cap is checked
/// **before** any decoding; over-limit input is refused with `E_LIMIT`/`E_FORMAT` and never
/// reaches the device.
pub fn decode_animation(
    payload: &[u8],
    loop_override: Option<bool>,
) -> Result<Animation, AnimationError> {
    if payload.is_empty() {
        return Err(AnimationError::Empty);
    }
    // The wire cap first: an oversized animation must hit the 16 MB cap before decode.
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(AnimationError::PayloadTooLarge {
            len: payload.len(),
            max: MAX_PAYLOAD_BYTES,
        });
    }
    let wire = encode::detect_format(payload).ok_or(AnimationError::UnsupportedFormat)?;
    let (frames, w, h, loop_) = match wire {
        encode::WireFormat::Gif => {
            decode_gif_animation(payload, MAX_ANIMATION_FRAMES as usize + 1)?
        }
        encode::WireFormat::Png => {
            decode_png_animation(payload, MAX_ANIMATION_FRAMES as usize + 1)?
        }
        encode::WireFormat::Jpeg => decode_jpeg_still(payload)?,
    };
    if frames.len() as u32 > MAX_ANIMATION_FRAMES {
        return Err(AnimationError::TooManyFrames {
            n: frames.len() as u32,
            max: MAX_ANIMATION_FRAMES,
        });
    }
    let loop_ = loop_override.unwrap_or(loop_);
    let format = match wire {
        encode::WireFormat::Png => FrameFormat::Png,
        encode::WireFormat::Jpeg => FrameFormat::Jpeg,
        encode::WireFormat::Gif => FrameFormat::Gif,
    };
    let anim = finalize(frames, w, h, loop_, format);
    anim.validate()?;
    Ok(anim)
}

/// Build an animation from a directory of stills at `fps` (the `--dir --fps N` source):
/// files in filename order (the `%04d` sequence), each ≤ 16 MB wire and ≤ 4096×4096, at most
/// 300 files, each shown for `round(1000/fps)` ms (≥ 20 ms). `fps` over 30 (or 0) is refused
/// with `E_LIMIT` before any file is read.
pub fn animation_from_dir(dir: &Path, fps: u32, loop_: bool) -> Result<Animation, AnimationError> {
    if fps == 0 || fps > MAX_FPS {
        return Err(AnimationError::FpsTooHigh {
            fps: f64::from(fps),
            max: MAX_FPS,
        });
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| AnimationError::DecodeFailed {
            format: "dir",
            reason: format!("cannot read directory {}: {e}", dir.display()),
        })?
        .filter_map(|e| e.ok().map(|en| en.path()))
        .filter(|p| p.is_file())
        .collect();
    files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    if files.is_empty() {
        return Err(AnimationError::DecodeFailed {
            format: "dir",
            reason: format!("frame directory {} contains no files", dir.display()),
        });
    }
    if files.len() as u32 > MAX_ANIMATION_FRAMES {
        return Err(AnimationError::TooManyFrames {
            n: files.len() as u32,
            max: MAX_ANIMATION_FRAMES,
        });
    }
    let delay_ms = (1000.0 / f64::from(fps))
        .round()
        .max(MIN_FRAME_INTERVAL_MS as f64) as u64;
    let mut frames: Vec<AnimFrame> = Vec::with_capacity(files.len());
    let mut total = 0usize;
    let mut w = 0u32;
    let mut h = 0u32;
    let mut format = FrameFormat::Png;
    for (i, p) in files.iter().enumerate() {
        let payload = std::fs::read(p).map_err(|e| AnimationError::DecodeFailed {
            format: "dir",
            reason: format!("cannot read frame {}: {e}", p.display()),
        })?;
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(AnimationError::PayloadTooLarge {
                len: payload.len(),
                max: MAX_PAYLOAD_BYTES,
            });
        }
        let wire = encode::detect_format(&payload).ok_or(AnimationError::UnsupportedFormat)?;
        // a directory holds stills: only the first frame of a container is used
        let (mut fr, fw, fh, _) = match wire {
            encode::WireFormat::Gif => decode_gif_animation(&payload, 1)?,
            encode::WireFormat::Png => decode_png_animation(&payload, 1)?,
            encode::WireFormat::Jpeg => decode_jpeg_still(&payload)?,
        };
        if fw > MAX_DECODE_PX || fh > MAX_DECODE_PX {
            return Err(AnimationError::ImageTooLarge {
                w: fw,
                h: fh,
                max: MAX_DECODE_PX,
            });
        }
        if i == 0 {
            w = fw;
            h = fh;
            format = match wire {
                encode::WireFormat::Png => FrameFormat::Png,
                encode::WireFormat::Jpeg => FrameFormat::Jpeg,
                encode::WireFormat::Gif => FrameFormat::Gif,
            };
        }
        let fr = fr.remove(0);
        total += fr.rgb.len();
        if total > MAX_TOTAL_DECODE_BYTES {
            return Err(AnimationError::TotalDecodeTooLarge {
                bytes: total,
                max: MAX_TOTAL_DECODE_BYTES,
            });
        }
        frames.push(AnimFrame {
            rgb: fr.rgb,
            delay_ms,
        });
    }
    let anim = finalize(frames, w, h, loop_, format);
    anim.validate()?;
    Ok(anim)
}

/// Push a decoded animation through the DS-031 frame service: each frame is re-encoded to the
/// device wire format and queued individually (`pre_encoded`), so the per-target
/// latest-frame-wins queue of `frames::service` applies unchanged — a slow device drops
/// *waiting* frames (counted) and never tears the in-flight one. The **first frame is sent
/// immediately**; with `pace = true` the producer then waits [`Animation::interval`] between
/// frames (≤ 30 fps, ≥ 0.02 s). Returns the number of frames queued.
pub fn push_animation(
    svc: &FrameService,
    target: FrameTarget,
    anim: &Animation,
    seq_base: u64,
    pace: bool,
) -> Result<u64, String> {
    anim.validate().map_err(|e| format!("{}: {e}", e.code()))?;
    let interval = anim.interval();
    let n = anim.frames.len();
    for (i, fr) in anim.frames.iter().enumerate() {
        let jpeg = encode::encode_rgb(target, &fr.rgb, anim.w, anim.h)
            .map_err(|e| format!("{}: {e}", e.code()))?;
        svc.push_frame(target, jpeg, seq_base + i as u64, true)
            .map_err(|e| format!("{:?}: {}", e.code, e.message))?;
        if pace && i + 1 < n {
            std::thread::sleep(interval);
        }
    }
    Ok(n as u64)
}

// ---------------------------------------------------------------------------
// internals
// ---------------------------------------------------------------------------

/// Merge frames shorter than the 0.02 s contract minimum into the next emitted frame (their
/// delays accumulate; the emitted frame carries the latest content of the group), then compute
/// the nominal `fps` and assemble the [`Animation`].
fn finalize(frames: Vec<AnimFrame>, w: u32, h: u32, loop_: bool, format: FrameFormat) -> Animation {
    let frames = merge_short_frames(frames);
    let n = frames.len();
    let fps = if n > 1 {
        let mean = frames.iter().map(|f| f.delay_ms).sum::<u64>() as f64 / n as f64;
        Some((1000.0 / mean).round().clamp(1.0, f64::from(MAX_FPS)))
    } else {
        None
    };
    Animation {
        frames,
        w,
        h,
        fps,
        loop_,
        format,
    }
}

/// Drop sub-0.02 s frames by merging their delays (ticket DS-034 implementation note):
/// a group of frames whose accumulated delay is still under 20 ms is not emitted; its delay
/// rolls into the next emitted frame, whose content is the last frame of the group.
fn merge_short_frames(frames: Vec<AnimFrame>) -> Vec<AnimFrame> {
    if frames.len() <= 1 {
        let mut frames = frames;
        if let Some(f) = frames.first_mut() {
            f.delay_ms = f.delay_ms.max(MIN_FRAME_INTERVAL_MS);
        }
        return frames;
    }
    let mut out: Vec<AnimFrame> = Vec::with_capacity(frames.len());
    let mut acc = 0u64;
    let mut pending: Option<AnimFrame> = None;
    for fr in frames {
        acc += fr.delay_ms;
        pending = Some(fr);
        if acc >= MIN_FRAME_INTERVAL_MS {
            let mut f = pending.take().expect("pending set");
            f.delay_ms = acc;
            out.push(f);
            acc = 0;
        }
    }
    if let Some(mut f) = pending {
        if out.is_empty() {
            f.delay_ms = acc.max(MIN_FRAME_INTERVAL_MS);
            out.push(f);
        } else {
            // trailing group under 20 ms: merge its delay into the previous frame
            out.last_mut().expect("out non-empty").delay_ms += acc;
        }
    }
    out
}

/// An RGBA compositing canvas (GIF frames and APNG subframes are blended onto it by hand).
struct Canvas {
    w: u32,
    h: u32,
    px: Vec<u8>, // RGBA, w*h*4
}

impl Canvas {
    fn new(w: u32, h: u32) -> Self {
        Self {
            w,
            h,
            px: vec![0u8; w as usize * h as usize * 4],
        }
    }

    fn snapshot(&self) -> Vec<u8> {
        self.px.clone()
    }

    /// Blend one RGBA subframe at (x, y); fully transparent pixels keep the canvas.
    fn blend_rgba(&mut self, sub: &[u8], x: u32, y: u32, fw: u32, fh: u32) {
        for r in 0..fh {
            for c in 0..fw {
                let sx = x + c;
                let sy = y + r;
                if sx >= self.w || sy >= self.h {
                    continue;
                }
                let si = ((r * fw + c) * 4) as usize;
                if si + 4 > sub.len() {
                    return;
                }
                let di = ((sy * self.w + sx) * 4) as usize;
                if sub[si + 3] == 0 {
                    continue;
                }
                self.px[di..di + 4].copy_from_slice(&sub[si..si + 4]);
            }
        }
    }

    /// Overwrite (BlendOp::Source) one RGBA subframe at (x, y).
    fn overwrite_rgba(&mut self, sub: &[u8], x: u32, y: u32, fw: u32, fh: u32) {
        for r in 0..fh {
            for c in 0..fw {
                let sx = x + c;
                let sy = y + r;
                if sx >= self.w || sy >= self.h {
                    continue;
                }
                let si = ((r * fw + c) * 4) as usize;
                if si + 4 > sub.len() {
                    return;
                }
                let di = ((sy * self.w + sx) * 4) as usize;
                self.px[di..di + 4].copy_from_slice(&sub[si..si + 4]);
            }
        }
    }

    fn clear_rect(&mut self, x: u32, y: u32, fw: u32, fh: u32) {
        for r in 0..fh {
            for c in 0..fw {
                let sx = x + c;
                let sy = y + r;
                if sx >= self.w || sy >= self.h {
                    continue;
                }
                let di = ((sy * self.w + sx) * 4) as usize;
                self.px[di..di + 4].copy_from_slice(&[0, 0, 0, 0]);
            }
        }
    }

    fn restore(&mut self, saved: Vec<u8>) {
        self.px = saved;
    }

    /// The canvas as packed RGB8.
    fn rgb(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.w as usize * self.h as usize * 3);
        for px in self.px.chunks_exact(4) {
            out.extend_from_slice(&px[..3]);
        }
        out
    }
}

/// Decode an animated (or single-frame) GIF. `stop_at` bounds how many frames are decoded
/// (memory-bounded: one frame at a time; the caller passes `MAX_ANIMATION_FRAMES + 1` so an
/// over-count input is refused, or 1 for stills-in-a-directory).
fn decode_gif_animation(
    payload: &[u8],
    stop_at: usize,
) -> Result<(Vec<AnimFrame>, u32, u32, bool), AnimationError> {
    let fail = |e: gif::DecodingError| AnimationError::DecodeFailed {
        format: "gif",
        reason: e.to_string(),
    };
    // The logical screen size is in the fixed header: check the decode cap before the
    // decoder allocates anything (bytes 6..10, u16 LE each).
    if payload.len() >= 10 {
        let (hw, hh) = (
            u32::from(u16::from_le_bytes([payload[6], payload[7]])),
            u32::from(u16::from_le_bytes([payload[8], payload[9]])),
        );
        if hw > MAX_DECODE_PX || hh > MAX_DECODE_PX {
            return Err(AnimationError::ImageTooLarge {
                w: hw,
                h: hh,
                max: MAX_DECODE_PX,
            });
        }
    }
    let mut opts = gif::DecodeOptions::new();
    opts.set_color_output(gif::ColorOutput::RGBA);
    opts.set_memory_limit(gif::MemoryLimit::Bytes(
        NonZeroU64::new(MAX_TOTAL_DECODE_BYTES as u64).expect("non-zero"),
    ));
    let mut dec = opts.read_info(Cursor::new(payload)).map_err(fail)?;
    let (w, h) = (u32::from(dec.width()), u32::from(dec.height()));
    if w > MAX_DECODE_PX || h > MAX_DECODE_PX {
        return Err(AnimationError::ImageTooLarge {
            w,
            h,
            max: MAX_DECODE_PX,
        });
    }
    // NETSCAPE2.0 loop extension: 0 = infinite, n > 0 = n repetitions, absent = play once.
    let loop_ = match dec.repeat() {
        gif::Repeat::Infinite => true,
        gif::Repeat::Finite(n) => n > 0,
    };
    let mut canvas = Canvas::new(w, h);
    let mut frames: Vec<AnimFrame> = Vec::new();
    let mut total = 0usize;
    while let Some(info) = dec.next_frame_info().map_err(fail)?.cloned() {
        if frames.len() >= stop_at {
            break;
        }
        let delay_ms = u64::from(info.delay) * 10;
        let mut buf = vec![0u8; dec.buffer_size()];
        dec.read_into_buffer(&mut buf).map_err(fail)?;
        let saved = if info.dispose == gif::DisposalMethod::Previous {
            Some(canvas.snapshot())
        } else {
            None
        };
        let (fx, fy, fw, fh) = (
            u32::from(info.left),
            u32::from(info.top),
            u32::from(info.width),
            u32::from(info.height),
        );
        canvas.blend_rgba(&buf, fx, fy, fw, fh);
        let rgb = canvas.rgb();
        total += rgb.len();
        if total > MAX_TOTAL_DECODE_BYTES {
            return Err(AnimationError::TotalDecodeTooLarge {
                bytes: total,
                max: MAX_TOTAL_DECODE_BYTES,
            });
        }
        frames.push(AnimFrame { rgb, delay_ms });
        match info.dispose {
            gif::DisposalMethod::Background => canvas.clear_rect(fx, fy, fw, fh),
            gif::DisposalMethod::Previous => canvas.restore(saved.expect("saved for Previous")),
            _ => {}
        }
    }
    if frames.is_empty() {
        return Err(AnimationError::DecodeFailed {
            format: "gif",
            reason: "no frames".to_string(),
        });
    }
    Ok((frames, w, h, loop_))
}

/// Decode a PNG, expanding an APNG (`acTL`/`fcTL`/`fdAT`) to all its frames. `stop_at` bounds
/// the decoded frame count (see [`decode_gif_animation`]).
fn decode_png_animation(
    payload: &[u8],
    stop_at: usize,
) -> Result<(Vec<AnimFrame>, u32, u32, bool), AnimationError> {
    let fail = |e: png::DecodingError| AnimationError::DecodeFailed {
        format: "png",
        reason: e.to_string(),
    };
    let mut dec = png::Decoder::new(Cursor::new(payload));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().map_err(fail)?;
    let info = reader.info();
    let (w, h) = (info.width, info.height);
    if w > MAX_DECODE_PX || h > MAX_DECODE_PX {
        return Err(AnimationError::ImageTooLarge {
            w,
            h,
            max: MAX_DECODE_PX,
        });
    }
    // APNG loop: num_plays 0 = infinite, 1 = play once (no loop), n > 1 = loop.
    let loop_ = info
        .animation_control
        .is_some_and(|a| a.num_plays == 0 || a.num_plays > 1);
    let animated = info.animation_control.is_some();
    let mut canvas = Canvas::new(w, h);
    let mut frames: Vec<AnimFrame> = Vec::new();
    let mut total = 0usize;
    loop {
        if frames.len() >= stop_at {
            break;
        }
        // read_info positioned the reader at the first frame; its fcTL (if any) is current.
        let fc: Option<png::FrameControl> = reader.info().frame_control;
        let _ = animated;
        let delay_ms = fc.as_ref().map_or(0, delay_ms_from_fc);
        let out_size = reader
            .output_buffer_size()
            .ok_or_else(|| AnimationError::DecodeFailed {
                format: "png",
                reason: "no output buffer size".to_string(),
            })?;
        let mut buf = vec![0u8; out_size];
        let out = reader.next_frame(&mut buf).map_err(fail)?;
        let (fx, fy, fw, fh, blend_over, dispose) = match &fc {
            Some(f) => (
                f.x_offset,
                f.y_offset,
                f.width,
                f.height,
                f.blend_op == png::BlendOp::Over,
                f.dispose_op,
            ),
            None => (0, 0, w, h, false, png::DisposeOp::None),
        };
        let rgba = to_rgba(out.color_type, &buf, fw, fh)?;
        let saved = if dispose == png::DisposeOp::Previous {
            Some(canvas.snapshot())
        } else {
            None
        };
        if blend_over {
            canvas.blend_rgba(&rgba, fx, fy, fw, fh);
        } else {
            canvas.overwrite_rgba(&rgba, fx, fy, fw, fh);
        }
        let rgb = canvas.rgb();
        total += rgb.len();
        if total > MAX_TOTAL_DECODE_BYTES {
            return Err(AnimationError::TotalDecodeTooLarge {
                bytes: total,
                max: MAX_TOTAL_DECODE_BYTES,
            });
        }
        frames.push(AnimFrame { rgb, delay_ms });
        match dispose {
            png::DisposeOp::Background => canvas.clear_rect(fx, fy, fw, fh),
            png::DisposeOp::Previous => canvas.restore(saved.expect("saved for Previous")),
            png::DisposeOp::None => {}
        }
        // advance to the next frame; PolledAfterEndOfImage means there is none
        if reader.next_frame_info().is_err() {
            break;
        }
    }
    if frames.is_empty() {
        return Err(AnimationError::DecodeFailed {
            format: "png",
            reason: "no frames".to_string(),
        });
    }
    Ok((frames, w, h, loop_))
}

/// Decode a still JPEG to a single-frame animation.
fn decode_jpeg_still(payload: &[u8]) -> Result<(Vec<AnimFrame>, u32, u32, bool), AnimationError> {
    let fail = |e: zune_jpeg::errors::DecodeErrors| AnimationError::DecodeFailed {
        format: "jpeg",
        reason: e.to_string(),
    };
    let mut dec = zune_jpeg::JpegDecoder::new_with_options(
        Cursor::new(payload),
        zune_jpeg::zune_core::options::DecoderOptions::default()
            .jpeg_set_out_colorspace(zune_jpeg::zune_core::colorspace::ColorSpace::RGB),
    );
    let out = dec.decode().map_err(fail)?;
    let info = dec.info().ok_or_else(|| AnimationError::DecodeFailed {
        format: "jpeg",
        reason: "no image info after decode".to_string(),
    })?;
    let (w, h) = (u32::from(info.width), u32::from(info.height));
    if w > MAX_DECODE_PX || h > MAX_DECODE_PX {
        return Err(AnimationError::ImageTooLarge {
            w,
            h,
            max: MAX_DECODE_PX,
        });
    }
    Ok((
        vec![AnimFrame {
            rgb: out,
            delay_ms: MIN_FRAME_INTERVAL_MS,
        }],
        w,
        h,
        false,
    ))
}

/// APNG `fcTL` delay (`delay_num / delay_den` seconds, denominator 0 means 100 per spec).
fn delay_ms_from_fc(fc: &png::FrameControl) -> u64 {
    let den = if fc.delay_den == 0 {
        100
    } else {
        u32::from(fc.delay_den)
    };
    (u64::from(fc.delay_num) * 1000 + u64::from(den) / 2) / u64::from(den)
}

/// Convert a normalized 8-bit PNG subframe buffer to packed RGBA8.
fn to_rgba(color: png::ColorType, buf: &[u8], w: u32, h: u32) -> Result<Vec<u8>, AnimationError> {
    let n = w as usize * h as usize;
    let mut out = Vec::with_capacity(n * 4);
    match color {
        png::ColorType::Rgb => {
            for px in buf.chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
        }
        png::ColorType::Rgba => out.extend_from_slice(buf),
        png::ColorType::Grayscale => {
            for &g in buf {
                out.extend_from_slice(&[g, g, g, 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for px in buf.chunks_exact(2) {
                out.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
            }
        }
        png::ColorType::Indexed => {
            return Err(AnimationError::DecodeFailed {
                format: "png",
                reason: "palette not expanded (normalize_to_color8)".to_string(),
            })
        }
    }
    if out.len() < n * 4 {
        return Err(AnimationError::DecodeFailed {
            format: "png",
            reason: format!("short subframe buffer: {} < {}x4", out.len(), n),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frames::service::FrameService;
    use crate::lifecycle::Shutdown;
    use crate::router::Bus;
    use ds_device::transport::{DeviceEvent, Transport};
    use ds_protocol::Report;
    use ds_state::Store;
    use mh_device::ModuleId;
    use std::sync::{Arc, Condvar, Mutex as StdMutex};
    use std::time::{Duration, Instant};

    // ---- deterministic image builders (no binary fixtures) -----------------

    /// Build a tiny animated GIF with the gif crate's own encoder: `colors` are the palette
    /// entries, `delays` in 10 ms units, each frame a solid full-canvas colour.
    fn build_gif(
        colors: &[[u8; 3]],
        delays: &[u16],
        w: u16,
        h: u16,
        repeat: gif::Repeat,
    ) -> Vec<u8> {
        let entries = colors.len().max(2).next_power_of_two();
        let mut palette = vec![0u8; entries * 3];
        for (i, c) in colors.iter().enumerate() {
            palette[i * 3..i * 3 + 3].copy_from_slice(c);
        }
        let mut out = Vec::new();
        {
            let mut enc = gif::Encoder::new(&mut out, w, h, &palette).unwrap();
            enc.set_repeat(repeat).unwrap();
            for (i, &delay) in delays.iter().enumerate() {
                let idx = i % colors.len();
                let frame = gif::Frame {
                    delay,
                    width: w,
                    height: h,
                    buffer: std::borrow::Cow::Owned(vec![idx as u8; w as usize * h as usize]),
                    dispose: gif::DisposalMethod::Keep,
                    ..Default::default()
                };
                enc.write_frame(&frame).unwrap();
            }
        }
        out
    }

    /// Build a still PNG with the png crate's own encoder (RGB8).
    fn build_png(rgb: &[u8], w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, w, h);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().unwrap();
            writer.write_image_data(rgb).unwrap();
        }
        out
    }

    fn solid_rgb(c: [u8; 3], w: u32, h: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity(w as usize * h as usize * 3);
        for _ in 0..w * h {
            v.extend_from_slice(&c);
        }
        v
    }

    /// Splice a 2-frame APNG out of two still PNGs:
    /// sig + IHDR + acTL + fcTL + IDAT + fcTL + IDAT + IEND.
    fn build_apng(frame1: &[u8], frame2: &[u8], w: u32, h: u32, delay: (u16, u16)) -> Vec<u8> {
        fn chunks(png_bytes: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
            let mut out = Vec::new();
            let mut pos = 8;
            while pos + 8 <= png_bytes.len() {
                let len = u32::from_be_bytes([
                    png_bytes[pos],
                    png_bytes[pos + 1],
                    png_bytes[pos + 2],
                    png_bytes[pos + 3],
                ]) as usize;
                let ty = png_bytes[pos + 4..pos + 8].to_vec();
                let data = png_bytes[pos + 8..pos + 8 + len].to_vec();
                out.push((ty, data));
                pos += 12 + len;
            }
            out
        }
        fn chunk(ty: &[u8], data: &[u8]) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(ty);
            out.extend_from_slice(data);
            let mut crc_input = Vec::new();
            crc_input.extend_from_slice(ty);
            crc_input.extend_from_slice(data);
            out.extend_from_slice(&ds_ipc::frames::codec::crc32(&crc_input).to_be_bytes());
            out
        }
        let c1 = chunks(frame1);
        let c2 = chunks(frame2);
        let ihdr = c1.iter().find(|(t, _)| t == b"IHDR").expect("IHDR").clone();
        let idat1 = c1
            .iter()
            .find(|(t, _)| t == b"IDAT")
            .expect("IDAT1")
            .clone();
        let idat2 = c2
            .iter()
            .find(|(t, _)| t == b"IDAT")
            .expect("IDAT2")
            .clone();
        let mut actl = Vec::new();
        actl.extend_from_slice(&2u32.to_be_bytes()); // num_frames
        actl.extend_from_slice(&0u32.to_be_bytes()); // num_plays = 0 => loop forever
        let fctl = |seq: u32| {
            let mut d = Vec::new();
            d.extend_from_slice(&seq.to_be_bytes());
            d.extend_from_slice(&w.to_be_bytes());
            d.extend_from_slice(&h.to_be_bytes());
            d.extend_from_slice(&0u32.to_be_bytes()); // x_offset
            d.extend_from_slice(&0u32.to_be_bytes()); // y_offset
            d.extend_from_slice(&delay.0.to_be_bytes());
            d.extend_from_slice(&delay.1.to_be_bytes());
            d.push(0); // dispose: none
            d.push(0); // blend: source
            d
        };
        let mut out = Vec::new();
        out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        out.extend(chunk(b"IHDR", &ihdr.1));
        out.extend(chunk(b"acTL", &actl));
        out.extend(chunk(b"fcTL", &fctl(0)));
        out.extend(chunk(b"IDAT", &idat1.1));
        out.extend(chunk(b"fcTL", &fctl(1)));
        let mut fdat = Vec::new();
        fdat.extend_from_slice(&2u32.to_be_bytes()); // fdAT sequence number (after fcTL 0 and 1)
        fdat.extend_from_slice(&idat2.1);
        out.extend(chunk(b"fdAT", &fdat));
        out.extend(chunk(b"IEND", &[]));
        out
    }

    fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration, what: &str) {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if cond() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out after {timeout:?} waiting for {what}");
    }

    // ---- decode + limits ----------------------------------------------------

    #[test]
    fn animated_gif_decodes_to_all_frames_with_delays_and_loop() {
        let gif = build_gif(
            &[[0xFF, 0, 0], [0, 0xFF, 0], [0, 0, 0xFF]],
            &[5, 5, 10], // 50 ms, 50 ms, 100 ms
            4,
            4,
            gif::Repeat::Infinite,
        );
        let anim = decode_animation(&gif, None).unwrap();
        assert_eq!(anim.frame_count(), 3);
        assert_eq!((anim.w, anim.h), (4, 4));
        assert_eq!(anim.format, FrameFormat::Gif);
        assert!(anim.loop_, "NETSCAPE infinite loop must be preserved");
        assert_eq!(anim.frames[0].delay_ms, 50);
        assert_eq!(anim.frames[1].delay_ms, 50);
        assert_eq!(anim.frames[2].delay_ms, 100);
        assert_eq!(anim.frames[0].rgb, solid_rgb([0xFF, 0, 0], 4, 4));
        assert_eq!(anim.frames[1].rgb, solid_rgb([0, 0xFF, 0], 4, 4));
        assert_eq!(anim.frames[2].rgb, solid_rgb([0, 0, 0xFF], 4, 4));
        let fps = anim.fps.expect("multi-frame has fps");
        assert!((1.0..=30.0).contains(&fps), "{fps}");
        assert!(anim.interval() >= MIN_FRAME_INTERVAL);
        anim.validate().unwrap();
    }

    #[test]
    fn gif_loop_flag_finite_absent_and_loop_override() {
        let looped = build_gif(&[[1, 2, 3]], &[5], 2, 2, gif::Repeat::Finite(2));
        assert!(decode_animation(&looped, None).unwrap().loop_);
        let once = build_gif(&[[1, 2, 3]], &[5], 2, 2, gif::Repeat::Finite(0));
        let anim = decode_animation(&once, None).unwrap();
        assert!(!anim.loop_, "no loop extension => play once");
        assert!(decode_animation(&once, Some(true)).unwrap().loop_);
        assert!(!decode_animation(&looped, Some(false)).unwrap().loop_);
    }

    #[test]
    fn sub_20ms_gif_frames_are_merged_not_emitted() {
        // six 10 ms frames (100 fps source): merged into groups with delay >= 20 ms
        let gif = build_gif(
            &[[9, 9, 9]; 6],
            &[1, 1, 1, 1, 1, 1],
            2,
            2,
            gif::Repeat::Infinite,
        );
        let anim = decode_animation(&gif, None).unwrap();
        assert!(
            anim.frame_count() < 6,
            "sub-20ms frames must be merged: {}",
            anim.frame_count()
        );
        for fr in &anim.frames {
            assert!(
                fr.delay_ms >= MIN_FRAME_INTERVAL_MS,
                "no emitted frame under 0.02 s: {} ms",
                fr.delay_ms
            );
        }
        let fps = anim.fps.unwrap();
        assert!(fps <= 30.0, "fps clamped to 30: {fps}");
        anim.validate().unwrap();
    }

    #[test]
    fn over_300_frame_gif_is_refused_with_e_limit() {
        let delays = vec![5u16; 301];
        let colors = [[7u8, 7, 7], [8u8, 8, 8]];
        let gif = build_gif(&colors, &delays, 2, 2, gif::Repeat::Infinite);
        let err = decode_animation(&gif, None).unwrap_err();
        assert_eq!(err.code(), "E_LIMIT");
        assert_eq!(
            err,
            AnimationError::TooManyFrames {
                n: 301,
                max: MAX_ANIMATION_FRAMES
            }
        );
    }

    #[test]
    fn oversize_payload_hits_the_wire_cap_before_decode() {
        let mut big = b"GIF89a".to_vec();
        big.resize(MAX_PAYLOAD_BYTES + 1, 0xA5);
        let err = decode_animation(&big, None).unwrap_err();
        assert_eq!(err.code(), "E_LIMIT");
        assert_eq!(
            err,
            AnimationError::PayloadTooLarge {
                len: MAX_PAYLOAD_BYTES + 1,
                max: MAX_PAYLOAD_BYTES
            }
        );
    }

    #[test]
    fn oversize_gif_logical_screen_is_refused_with_e_format_before_frame_decode() {
        let mut g = b"GIF89a".to_vec();
        g.extend_from_slice(&4097u16.to_le_bytes());
        g.extend_from_slice(&1u16.to_le_bytes());
        g.push(0x80); // GCT flag, 2 entries
        g.extend_from_slice(&[0u8; 6]);
        g.push(0x3B); // trailer
        let err = decode_animation(&g, None).unwrap_err();
        assert_eq!(err.code(), "E_FORMAT");
        assert_eq!(
            err,
            AnimationError::ImageTooLarge {
                w: 4097,
                h: 1,
                max: MAX_DECODE_PX
            }
        );
    }

    #[test]
    fn corrupt_and_unknown_inputs_are_e_format() {
        let err = decode_animation(b"GIF89a\x04\x00\x04\x00\x00zzzz", None).unwrap_err();
        assert_eq!(err.code(), "E_FORMAT");
        assert!(
            matches!(err, AnimationError::DecodeFailed { .. }),
            "{err:?}"
        );
        assert_eq!(
            decode_animation(b"not an image at all", None).unwrap_err(),
            AnimationError::UnsupportedFormat
        );
        assert_eq!(
            decode_animation(&[], None).unwrap_err(),
            AnimationError::Empty
        );
        assert_eq!(decode_animation(&[], None).unwrap_err().code(), "E_FORMAT");
    }

    #[test]
    fn apng_decodes_to_all_frames_with_loop_and_delay() {
        let f1 = build_png(&solid_rgb([0x10, 0x20, 0x30], 4, 4), 4, 4);
        let f2 = build_png(&solid_rgb([0x40, 0x50, 0x60], 4, 4), 4, 4);
        let apng = build_apng(&f1, &f2, 4, 4, (1, 20)); // 1/20 s = 50 ms, num_plays 0 => loop
        assert_eq!(encode::detect_format(&apng), Some(encode::WireFormat::Png));
        let anim = decode_animation(&apng, None).unwrap();
        assert_eq!(anim.frame_count(), 2);
        assert_eq!((anim.w, anim.h), (4, 4));
        assert!(anim.loop_, "APNG num_plays=0 loops");
        assert_eq!(anim.frames[0].delay_ms, 50);
        assert_eq!(anim.frames[1].delay_ms, 50);
        assert_eq!(anim.frames[0].rgb, solid_rgb([0x10, 0x20, 0x30], 4, 4));
        assert_eq!(anim.frames[1].rgb, solid_rgb([0x40, 0x50, 0x60], 4, 4));
        assert_eq!(anim.fps, Some(20.0));
        anim.validate().unwrap();
    }

    #[test]
    fn still_png_and_jpeg_are_single_frame_animations() {
        let png = build_png(&solid_rgb([1, 2, 3], 6, 5), 6, 5);
        let anim = decode_animation(&png, None).unwrap();
        assert_eq!(anim.frame_count(), 1);
        assert_eq!(anim.fps, None);
        assert!(!anim.loop_);
        assert_eq!(anim.frames[0].rgb, solid_rgb([1, 2, 3], 6, 5));
        assert_eq!(anim.format, FrameFormat::Png);
        let jpeg = encode::encode_solid(FrameTarget::Key(0), [9, 9, 9]);
        let anim = decode_animation(&jpeg, None).unwrap();
        assert_eq!(anim.frame_count(), 1);
        assert_eq!((anim.w, anim.h), (72, 72));
        assert_eq!(anim.format, FrameFormat::Jpeg);
    }

    #[test]
    fn metas_map_onto_the_dsf1_schema_with_frames_fps_loop_and_paced_seq_ts() {
        let gif = build_gif(
            &[[1, 1, 1], [2, 2, 2], [3, 3, 3]],
            &[5, 5, 5], // 50 ms => 20 fps
            4,
            4,
            gif::Repeat::Infinite,
        );
        let anim = decode_animation(&gif, None).unwrap();
        assert_eq!(anim.fps, Some(20.0));
        let metas = anim.metas("key:0", 100, 1_700_000_000_000);
        assert_eq!(metas.len(), 3);
        for (i, m) in metas.iter().enumerate() {
            assert_eq!(m.target, "key:0");
            assert_eq!(m.seq, 100 + i as u64);
            assert_eq!(m.ts_ms, 1_700_000_000_000 + i as u64 * 50);
            assert_eq!(m.frames, Some(3));
            assert_eq!(m.fps, Some(20.0));
            assert_eq!(m.loop_, Some(true));
            assert_eq!(m.format, FrameFormat::Gif);
            assert_eq!(m.w, Some(4));
            assert_eq!(m.h, Some(4));
            assert_eq!(m.max_payload_bytes, MAX_PAYLOAD_BYTES as u32);
            m.to_value()
                .expect("meta validates against stream-frame.json");
        }
        let once = build_gif(&[[1, 1, 1]], &[5], 2, 2, gif::Repeat::Finite(0));
        let anim = decode_animation(&once, None).unwrap();
        let m = &anim.metas("dial", 0, 0)[0];
        assert_eq!(m.loop_, None, "no loop => flag absent, not false");
        m.to_value().unwrap();
    }

    // ---- directory mode -----------------------------------------------------

    #[test]
    fn dir_animation_at_fps_decodes_in_filename_order_with_equal_delays() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5u32 {
            let png = build_png(&solid_rgb([i as u8 * 10, 0, 0], 4, 4), 4, 4);
            std::fs::write(dir.path().join(format!("{i:04}.png")), png).unwrap();
        }
        let anim = animation_from_dir(dir.path(), 10, true).unwrap();
        assert_eq!(anim.frame_count(), 5);
        assert_eq!(anim.fps, Some(10.0));
        assert!(anim.loop_);
        assert_eq!(anim.frames[0].delay_ms, 100);
        for (i, fr) in anim.frames.iter().enumerate() {
            assert_eq!(fr.rgb, solid_rgb([i as u8 * 10, 0, 0], 4, 4), "frame {i}");
        }
        anim.validate().unwrap();
    }

    #[test]
    fn dir_animation_limits_are_enforced() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..3u32 {
            let png = build_png(&solid_rgb([i as u8, 0, 0], 4, 4), 4, 4);
            std::fs::write(dir.path().join(format!("{i:04}.png")), png).unwrap();
        }
        // 30 fps is the cap; 40 fps and 0 fps are refused with E_LIMIT before reading files
        assert_eq!(
            animation_from_dir(dir.path(), 40, false)
                .unwrap_err()
                .code(),
            "E_LIMIT"
        );
        assert_eq!(
            animation_from_dir(dir.path(), 0, false).unwrap_err(),
            AnimationError::FpsTooHigh {
                fps: 0.0,
                max: MAX_FPS
            }
        );
        // 301 files refused with E_LIMIT
        let big = tempfile::tempdir().unwrap();
        for i in 0..301u32 {
            let png = build_png(&solid_rgb([0, 0, 0], 2, 2), 2, 2);
            std::fs::write(big.path().join(format!("{i:04}.png")), png).unwrap();
        }
        assert_eq!(
            animation_from_dir(big.path(), 30, false).unwrap_err(),
            AnimationError::TooManyFrames {
                n: 301,
                max: MAX_ANIMATION_FRAMES
            }
        );
        // a non-image file in the dir is E_FORMAT
        let bad = tempfile::tempdir().unwrap();
        std::fs::write(bad.path().join("0000.png"), b"not an image").unwrap();
        assert_eq!(
            animation_from_dir(bad.path(), 30, false)
                .unwrap_err()
                .code(),
            "E_FORMAT"
        );
        // an oversize file is E_LIMIT (wire cap, before decode)
        let over = tempfile::tempdir().unwrap();
        let mut big_file = b"\x89PNG\r\n\x1a\n".to_vec();
        big_file.resize(MAX_PAYLOAD_BYTES + 1, 0);
        std::fs::write(over.path().join("0000.png"), &big_file).unwrap();
        assert_eq!(
            animation_from_dir(over.path(), 30, false)
                .unwrap_err()
                .code(),
            "E_LIMIT"
        );
        // empty dir is an error
        let empty = tempfile::tempdir().unwrap();
        assert!(animation_from_dir(empty.path(), 30, false).is_err());
    }

    // ---- queue integration (DS-031 service + ds-vdev) ------------------------

    /// A mock transport whose `send` blocks until the test opens the gate.
    struct GateTransport {
        sent: StdMutex<Vec<(ModuleId, Vec<u8>)>>,
        open: StdMutex<bool>,
        cv: Condvar,
    }

    impl GateTransport {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                sent: StdMutex::new(Vec::new()),
                open: StdMutex::new(false),
                cv: Condvar::new(),
            })
        }
        fn open(&self) {
            let mut g = self.open.lock().unwrap();
            *g = true;
            self.cv.notify_all();
        }
        fn transfers(&self, cmd: u8, index: u8) -> Vec<Vec<u8>> {
            let sent = self.sent.lock().unwrap();
            let mut out: Vec<Vec<u8>> = Vec::new();
            let mut cur: Vec<u8> = Vec::new();
            for (_, raw) in sent.iter() {
                if raw.len() < 8 || raw[0] != 0x02 || raw[1] != cmd || raw[6] != index {
                    continue;
                }
                let len = u16::from_le_bytes([raw[4], raw[5]]) as usize;
                cur.extend_from_slice(&raw[8..8 + len]);
                if raw[7] == 1 {
                    out.push(std::mem::take(&mut cur));
                }
            }
            out
        }
    }

    impl Transport for GateTransport {
        fn send(&self, module: &ModuleId, report: &Report) -> ds_device::Result<usize> {
            let bytes = report.bytes().to_vec();
            let mut g = self.open.lock().unwrap();
            while !*g {
                g = self.cv.wait(g).unwrap();
            }
            drop(g);
            let n = bytes.len();
            self.sent.lock().unwrap().push((module.clone(), bytes));
            Ok(n)
        }
        fn query(
            &self,
            _module: &ModuleId,
            _feature_id: u8,
            _timeout: Duration,
        ) -> ds_device::Result<Vec<u8>> {
            Ok(vec![])
        }
        fn recv(&self, _timeout: Duration) -> ds_device::Result<DeviceEvent> {
            Err(ds_device::Error::Timeout(Duration::from_millis(1)))
        }
        fn close(&self) -> ds_device::Result<()> {
            Ok(())
        }
        fn module_id(&self, kind: ds_protocol::ModuleKind) -> Option<ModuleId> {
            // the gate stands in for a whole deck: every kind resolves to its virtual id
            Some(ds_device::vtransport::virtual_module_id(kind))
        }
    }

    fn test_service(transport: Option<Arc<dyn Transport>>) -> (FrameService, Arc<Store>, Shutdown) {
        let store = Arc::new(Store::fresh());
        let shutdown = Shutdown::manual();
        let svc = FrameService::spawn(store.clone(), transport, Bus::new(), shutdown.clone());
        (svc, store, shutdown)
    }

    fn three_frame_gif() -> Vec<u8> {
        build_gif(
            &[[0xFF, 0, 0], [0, 0xFF, 0], [0, 0, 0xFF]],
            &[5, 5, 5],
            8,
            8,
            gif::Repeat::Infinite,
        )
    }

    #[test]
    fn push_animation_paces_frames_and_sends_first_immediately() {
        let gate = GateTransport::new();
        let (svc, _store, shutdown) = test_service(Some(gate.clone()));
        let target = FrameTarget::Key(0);
        let anim = decode_animation(&three_frame_gif(), None).unwrap();
        let handle = {
            let svc = svc.clone();
            let anim = anim.clone();
            std::thread::spawn(move || push_animation(&svc, target, &anim, 0, true).unwrap())
        };
        // the FIRST frame goes in flight immediately, before one 50 ms interval can pass
        wait_until(
            || svc.queue_state(target).0 == Some(0),
            Duration::from_millis(40),
            "first frame in flight immediately (before the first interval)",
        );
        // while frame 0 is stuck in flight, frames 1 and 2 replace only the waiting one
        wait_until(
            || svc.queue_state(target).1 == Some(2),
            Duration::from_secs(5),
            "latest frame waiting",
        );
        let (in_flight, waiting, _sent, dropped, _q) = svc.queue_state(target);
        assert_eq!(in_flight, Some(0), "the in-flight frame is never dropped");
        assert_eq!(waiting, Some(2));
        assert_eq!(
            dropped, 1,
            "frame 1 was replaced (counted) — latest-frame-wins"
        );
        gate.open();
        let n = handle.join().unwrap();
        assert_eq!(n, 3, "all three frames were queued");
        wait_until(
            || {
                let (_, _, sent, _, q) = svc.queue_state(target);
                sent == 2 && q == 0
            },
            Duration::from_secs(5),
            "drain",
        );
        let transfers = gate.transfers(0x36, 0);
        assert_eq!(
            transfers.len(),
            2,
            "frame 0 whole, then the latest (frame 2)"
        );
        let enc = anim.encode_frames(target).unwrap();
        assert_eq!(transfers[0], enc[0]);
        assert_eq!(transfers[1], enc[2], "final frame is the last one");
        shutdown.trigger();
        svc.join();
    }

    #[test]
    fn push_animation_pacing_wall_clock_respects_fps() {
        let gate = GateTransport::new();
        gate.open();
        let (svc, _store, shutdown) = test_service(Some(gate.clone()));
        let target = FrameTarget::Dial;
        let gif = build_gif(
            &[[1, 1, 1], [2, 2, 2], [3, 3, 3], [4, 4, 4]],
            &[5, 5, 5, 5],
            8,
            8,
            gif::Repeat::Finite(0),
        );
        let anim = decode_animation(&gif, None).unwrap();
        assert_eq!(anim.fps, Some(20.0));
        let start = Instant::now();
        let n = push_animation(&svc, target, &anim, 0, true).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(n, 4);
        assert!(
            elapsed >= Duration::from_millis(140),
            "paced push of 4 frames at 20 fps must take >= ~150 ms, took {elapsed:?}"
        );
        wait_until(
            || svc.queue_state(target).2 == 4,
            Duration::from_secs(5),
            "all four frames sent",
        );
        shutdown.trigger();
        svc.join();
    }

    #[test]
    fn per_target_queues_are_independent_but_share_the_same_caps() {
        let gate = GateTransport::new();
        gate.open();
        let (svc, _store, shutdown) = test_service(Some(gate.clone()));
        let anim = decode_animation(&three_frame_gif(), None).unwrap();
        for target in [FrameTarget::Key(0), FrameTarget::Key(1), FrameTarget::Dial] {
            let n = push_animation(&svc, target, &anim, 0, true).unwrap();
            assert_eq!(n, 3);
        }
        wait_until(
            || {
                svc.queue_state(FrameTarget::Key(0)).2
                    + svc.queue_state(FrameTarget::Key(1)).2
                    + svc.queue_state(FrameTarget::Dial).2
                    == 9
            },
            Duration::from_secs(10),
            "all 9 frames sent (3 per target)",
        );
        for target in [FrameTarget::Key(0), FrameTarget::Key(1), FrameTarget::Dial] {
            let (in_flight, waiting, sent, dropped, _q) = svc.queue_state(target);
            assert_eq!((in_flight, waiting, sent, dropped), (None, None, 3, 0));
        }
        assert_eq!(gate.transfers(0x36, 0).len(), 3);
        assert_eq!(gate.transfers(0x36, 1).len(), 3);
        assert_eq!(gate.transfers(0x45, 0).len(), 3);
        shutdown.trigger();
        svc.join();
    }

    /// In-process ds-vdev integration: the real frame service + writer + vdev reassembly.
    #[test]
    fn vdev_integration_animation_lands_with_final_frame_on_device_no_rev() {
        use ds_device::uri::DeviceUri;
        use ds_vdev::{HubConfig, Throttle, VdevServer};

        let tmp = tempfile::tempdir().unwrap();
        let vdev_sock = tmp.path().join("vdev.sock");
        let vdev = VdevServer::start(
            &vdev_sock,
            HubConfig {
                modules: ds_protocol::ModuleKind::ALL.to_vec(),
                throttle: Throttle::none(),
            },
        )
        .unwrap();
        let uri = DeviceUri::parse(&format!("virtual:{}", vdev_sock.display())).unwrap();
        let transport: Arc<dyn Transport> = Arc::from(ds_device::open(&uri).unwrap());
        for kind in [ds_protocol::ModuleKind::Keys, ds_protocol::ModuleKind::Dial] {
            if let Some((report, _mode)) = ds_protocol::enter_software_mode(kind) {
                let id = ds_device::transport::module_id_for_send(&*transport, kind)
                    .expect("screen module connected on the transport");
                transport.send(&id, &report).unwrap();
            }
        }
        let store = Arc::new(Store::fresh());
        let shutdown = Shutdown::manual();
        let svc = FrameService::spawn(store.clone(), Some(transport), Bus::new(), shutdown.clone());

        let target = FrameTarget::Key(0);
        let anim = decode_animation(&three_frame_gif(), None).unwrap();
        let expected: Vec<String> = anim
            .encode_frames(target)
            .unwrap()
            .iter()
            .map(|j| ds_vdev::sha256_hex(j))
            .collect();
        let n = push_animation(&svc, target, &anim, 0, true).unwrap();
        assert_eq!(n, 3);

        wait_until(
            || {
                let (_, _, sent, dropped, q) = svc.queue_state(target);
                sent == 3 && dropped == 0 && q == 0
            },
            Duration::from_secs(10),
            "all animation frames sent",
        );
        svc.sample_now();
        assert_eq!(store.rev(), 0, "animation pushes never bump rev (S1)");
        let tel = store.get("/telemetry/streams/key:0").unwrap();
        assert_eq!(tel.rev, None);
        assert_eq!(tel.value["sent"], serde_json::json!(3));
        assert_eq!(tel.value["last_seq"], serde_json::json!(2));

        // the final reassembled frame on the vdev is the encoder output of the LAST frame
        let line = ds_vdev::ctl::request(&vdev_sock, "frames key:0")
            .unwrap()
            .remove(0);
        let want = &expected[2];
        assert!(
            line.contains(want),
            "vdev final frame {line:?} must be the last frame's encoded sha {want}"
        );
        assert_eq!(
            tel.value["last_sha256"].as_str().unwrap(),
            want,
            "last_sha256 == encoder output of the last frame"
        );
        shutdown.trigger();
        svc.join();
        vdev.shutdown();
    }

    /// Latest-frame-wins under a slow device: an unpaced animation push against a throttled
    /// vdev drops waiting frames (counted) and the device's final frame is the LAST one.
    #[test]
    fn vdev_throttled_animation_latest_frame_wins_in_flight_never_torn() {
        use ds_device::uri::DeviceUri;
        use ds_vdev::{HubConfig, Throttle, VdevServer};

        let tmp = tempfile::tempdir().unwrap();
        let vdev_sock = tmp.path().join("vdev.sock");
        let vdev = VdevServer::start(
            &vdev_sock,
            HubConfig {
                modules: ds_protocol::ModuleKind::ALL.to_vec(),
                throttle: Throttle {
                    kbps: Some(64),
                    drop_after: None,
                },
            },
        )
        .unwrap();
        let uri = DeviceUri::parse(&format!("virtual:{}", vdev_sock.display())).unwrap();
        let transport: Arc<dyn Transport> = Arc::from(ds_device::open(&uri).unwrap());
        for kind in [ds_protocol::ModuleKind::Keys, ds_protocol::ModuleKind::Dial] {
            if let Some((report, _mode)) = ds_protocol::enter_software_mode(kind) {
                let id = ds_device::transport::module_id_for_send(&*transport, kind)
                    .expect("screen module connected on the transport");
                transport.send(&id, &report).unwrap();
            }
        }
        let store = Arc::new(Store::fresh());
        let shutdown = Shutdown::manual();
        let svc = FrameService::spawn(store.clone(), Some(transport), Bus::new(), shutdown.clone());

        let target = FrameTarget::Key(2);
        let colors: Vec<[u8; 3]> = (0..8u8).map(|i| [i * 30, 0x80, 0x20]).collect();
        let delays = vec![5u16; 8];
        let gif = build_gif(&colors, &delays, 8, 8, gif::Repeat::Infinite);
        let anim = decode_animation(&gif, None).unwrap();
        assert_eq!(anim.frame_count(), 8);
        let expected_last = ds_vdev::sha256_hex(&anim.encode_frames(target).unwrap()[7]);

        let n = push_animation(&svc, target, &anim, 0, false).unwrap();
        assert_eq!(n, 8);
        wait_until(
            || {
                let (in_flight, waiting, sent, dropped, _q) = svc.queue_state(target);
                in_flight.is_none() && waiting.is_none() && sent + dropped == 8
            },
            Duration::from_secs(30),
            "queue drained (sent + dropped == 8)",
        );
        let (_, _, sent, dropped, _) = svc.queue_state(target);
        assert!(
            dropped > 0,
            "a slow device drops waiting frames: dropped={dropped} sent={sent}"
        );
        assert_eq!(store.rev(), 0, "S1: no rev churn during the animation");

        wait_until(
            || {
                ds_vdev::ctl::request(&vdev_sock, "frames key:2")
                    .ok()
                    .and_then(|l| l.first().cloned())
                    .is_some_and(|l| l.contains(&expected_last))
            },
            Duration::from_secs(30),
            "the last frame to be the device's final frame",
        );
        let line = ds_vdev::ctl::request(&vdev_sock, "frames key:2")
            .unwrap()
            .remove(0);
        assert!(line.contains(&expected_last), "{line:?}");
        shutdown.trigger();
        svc.join();
        vdev.shutdown();
    }

    #[test]
    fn push_animation_validates_limits_before_queueing() {
        let (svc, _store, shutdown) = test_service(None);
        let mut anim = Animation {
            frames: vec![AnimFrame {
                rgb: solid_rgb([0, 0, 0], 2, 2),
                delay_ms: 50,
            }],
            w: 2,
            h: 2,
            fps: Some(31.0),
            loop_: false,
            format: FrameFormat::Gif,
        };
        let err = push_animation(&svc, FrameTarget::Key(0), &anim, 0, true).unwrap_err();
        assert!(err.starts_with("E_LIMIT"), "{err}");
        assert_eq!(
            svc.queue_state(FrameTarget::Key(0)).1,
            None,
            "nothing queued"
        );
        anim.fps = Some(20.0);
        anim.frames.push(AnimFrame {
            rgb: solid_rgb([1, 1, 1], 2, 2),
            delay_ms: 5, // under 0.02 s
        });
        let err = push_animation(&svc, FrameTarget::Key(0), &anim, 0, true).unwrap_err();
        assert!(err.starts_with("E_LIMIT"), "{err}");
        assert!(err.contains("under"), "{err}");
        shutdown.trigger();
        svc.join();
    }
}
