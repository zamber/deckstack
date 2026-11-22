//! Source loading and validation for `ds stream push` (`docs/contracts/streaming.md`
//! §2/§5/§7): one frame file, or a directory of frames at a requested rate, or an
//! external video decoded by **ffmpeg as an argv-only external process** (R4 — the core
//! never links ffmpeg and never spawns a shell; a missing ffmpeg is a clear error).

use crate::output::Failure;
use ds_client::ClientError;
use ds_ipc::frames::limits::{MAX_ANIMATION_FRAMES, MAX_PAYLOAD_BYTES};
use ds_ipc::frames::{FrameFormat, FrameMeta};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Contract rate cap: ≤ 30 fps (`docs/contracts/streaming.md` §2, schema `fps` maximum).
pub const MAX_FPS: u32 = 30;

/// Wire payload format detected from the magic bytes (`schemas/stream-frame.json`
/// `format` enum: png|jpeg|gif).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireFormat {
    Png,
    Jpeg,
    Gif,
}

impl WireFormat {
    /// The `FrameFormat` of the DSF1 meta.
    pub fn frame_format(self) -> FrameFormat {
        match self {
            Self::Png => FrameFormat::Png,
            Self::Jpeg => FrameFormat::Jpeg,
            Self::Gif => FrameFormat::Gif,
        }
    }
}

/// Detect the wire format from the leading magic bytes; `None` = not an image (a video
/// or other container → external ffmpeg decode).
pub fn detect_format(bytes: &[u8]) -> Option<WireFormat> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(WireFormat::Png)
    } else if bytes.starts_with(b"\xFF\xD8\xFF") {
        Some(WireFormat::Jpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(WireFormat::Gif)
    } else {
        None
    }
}

/// Best-effort pixel size from the container header (PNG IHDR / GIF logical screen).
/// JPEG needs a SOF scan; the fields are optional in the schema, so `None` is fine.
pub fn image_size(format: WireFormat, bytes: &[u8]) -> Option<(u32, u32)> {
    match format {
        WireFormat::Png if bytes.len() >= 24 => {
            let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
            let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
            Some((w, h))
        }
        WireFormat::Gif if bytes.len() >= 10 => {
            let w = u16::from_le_bytes([bytes[6], bytes[7]]) as u32;
            let h = u16::from_le_bytes([bytes[8], bytes[9]]) as u32;
            Some((w, h))
        }
        _ => None,
    }
}

/// One frame ready to send: payload bytes + wire format (+ optional pixel size).
#[derive(Debug, Clone)]
pub struct FrameFile {
    pub payload: Vec<u8>,
    pub format: WireFormat,
    pub size: Option<(u32, u32)>,
}

/// A validated push plan: the frames to send, the animation rate, the loop flag.
#[derive(Debug)]
pub struct Plan {
    pub frames: Vec<FrameFile>,
    /// `Some(fps)` in directory/animation mode, `None` for a single still frame.
    pub fps: Option<u32>,
    pub loop_: bool,
    /// Temp dir holding ffmpeg-decoded frames (removed on drop).
    temp_dir: Option<PathBuf>,
}

impl Drop for Plan {
    fn drop(&mut self) {
        if let Some(d) = &self.temp_dir {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

impl Plan {
    /// The DSF1 meta for frame `seq` (per `schemas/stream-frame.json`).
    pub fn meta(&self, seq: u64, fr: &FrameFile) -> FrameMeta {
        let n = self.frames.len() as u32;
        FrameMeta {
            target: String::new(), // filled by the caller (verb_push owns the target)
            seq,
            ts_ms: now_ms(),
            format: fr.format.frame_format(),
            w: fr.size.map(|s| s.0),
            h: fr.size.map(|s| s.1),
            frames: if self.fps.is_some() { Some(n) } else { None },
            fps: self.fps.map(f64::from),
            loop_: if self.loop_ { Some(true) } else { None },
            crc32: 0, // filled by Frame::new
            max_payload_bytes: MAX_PAYLOAD_BYTES as u32,
        }
    }

    /// The pacing interval between two frames of an animation (`1/fps`, ≥ the 0.02 s
    /// contract minimum); zero for a single frame.
    pub fn frame_interval(&self) -> Duration {
        match self.fps {
            Some(f) => Duration::from_secs_f64(1.0 / f64::from(f)).max(Duration::from_millis(20)),
            None => Duration::ZERO,
        }
    }
}

/// ms since the Unix epoch (the meta `ts_ms`).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn usage(msg: String) -> Failure {
    Failure::Usage(msg)
}

fn check_fps(fps: Option<u32>) -> Result<Option<u32>, Failure> {
    match fps {
        None => Ok(None),
        Some(f) if (1..=MAX_FPS).contains(&f) => Ok(Some(f)),
        Some(f) => Err(usage(format!(
            "--fps must be an integer in 1..={MAX_FPS} (contract cap {MAX_FPS} fps), got {f}"
        ))),
    }
}

fn read_frame_file(path: &Path) -> Result<FrameFile, Failure> {
    let payload = std::fs::read(path)
        .map_err(|e| usage(format!("cannot read frame file {}: {e}", path.display())))?;
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(Failure::Client(ClientError::Limit(format!(
            "frame {} is {} bytes, over the 16 MB wire cap ({MAX_PAYLOAD_BYTES})",
            path.display(),
            payload.len()
        ))));
    }
    let format = detect_format(&payload).ok_or_else(|| {
        usage(format!(
            "{} is not a PNG/JPEG/GIF image (unknown magic bytes) — pass an image, or a \
             video file to decode with ffmpeg: ds stream push <target> --file video.mp4",
            path.display()
        ))
    })?;
    let size = image_size(format, &payload);
    Ok(FrameFile {
        payload,
        format,
        size,
    })
}

/// List a frame directory in filename order (the `%04d`-style sequence order).
fn list_dir(dir: &Path) -> Result<Vec<PathBuf>, Failure> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        usage(format!(
            "cannot read frame directory {}: {e}",
            dir.display()
        ))
    })?;
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| usage(format!("cannot list {}: {e}", dir.display())))?;
        let p = entry.path();
        if p.is_file() {
            files.push(p);
        }
    }
    files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    Ok(files)
}

fn load_dir(dir: &Path, fps: u32, loop_: bool) -> Result<Plan, Failure> {
    let files = list_dir(dir)?;
    if files.is_empty() {
        return Err(usage(format!(
            "frame directory {} contains no files",
            dir.display()
        )));
    }
    if files.len() as u32 > MAX_ANIMATION_FRAMES {
        return Err(usage(format!(
            "directory mode is capped at {MAX_ANIMATION_FRAMES} frames (contract), {} has {}",
            dir.display(),
            files.len()
        )));
    }
    let mut frames = Vec::with_capacity(files.len());
    for p in &files {
        frames.push(read_frame_file(p)?);
    }
    Ok(Plan {
        frames,
        fps: Some(fps),
        loop_,
        temp_dir: None,
    })
}

/// The ffmpeg argv for decoding a non-image input into `%04d`-style PNG frames.
/// Argv-only: no shell, no eval, no string interpolation into a command line (R4).
pub fn ffmpeg_argv(input: &Path, out_pattern: &Path) -> Vec<String> {
    vec![
        "ffmpeg".to_string(),
        "-y".to_string(),
        "-i".to_string(),
        input.to_string_lossy().into_owned(),
        out_pattern.to_string_lossy().into_owned(),
    ]
}

/// Decode a non-image `--file` (video) with an external ffmpeg into a temp dir of PNG
/// frames, then load that dir like `--dir`. A missing ffmpeg is a clear error.
fn decode_with_ffmpeg(input: &Path, fps: Option<u32>) -> Result<Plan, Failure> {
    let unique = format!("ds-stream-{}-{}", std::process::id(), now_ms());
    let temp = std::env::temp_dir().join(unique).join("frames");
    std::fs::create_dir_all(&temp).map_err(|e| {
        usage(format!(
            "cannot create temp frame dir {}: {e}",
            temp.display()
        ))
    })?;
    let pattern = temp.join("%04d.png");
    let argv = ffmpeg_argv(input, &pattern);
    let out = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    let out = match out {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let _ = std::fs::remove_dir_all(temp.parent().unwrap_or(&temp));
            return Err(usage(format!(
                "ffmpeg is required to decode the non-image input {} but was not found on PATH \
                 — install ffmpeg, or extract the frames yourself and use --dir: \
                 ffmpeg -i {} frames/%04d.png && ds stream push <target> --dir frames/ --fps N",
                input.display(),
                input.display()
            )));
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(temp.parent().unwrap_or(&temp));
            return Err(usage(format!(
                "cannot spawn ffmpeg (argv {:?}): {e}",
                &argv[..2]
            )));
        }
        Ok(o) => o,
    };
    if !out.status.success() {
        let tail = String::from_utf8_lossy(&out.stderr);
        let tail = tail
            .trim()
            .lines()
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .join("\n");
        let _ = std::fs::remove_dir_all(temp.parent().unwrap_or(&temp));
        return Err(usage(format!(
            "ffmpeg failed to decode {} (exit {:?}): {}",
            input.display(),
            out.status.code(),
            tail
        )));
    }
    let fps = fps.unwrap_or(MAX_FPS);
    let mut plan = load_dir(&temp, fps, false)?;
    plan.temp_dir = temp.parent().map(|p| p.to_path_buf());
    Ok(plan)
}

/// Build the validated push plan from the parsed verb arguments.
pub fn plan(
    file: Option<PathBuf>,
    dir: Option<PathBuf>,
    fps: Option<u32>,
    loop_: bool,
) -> Result<Plan, Failure> {
    let fps = check_fps(fps)?;
    match (file, dir) {
        (Some(f), None) => {
            let payload = std::fs::read(&f)
                .map_err(|e| usage(format!("cannot read file {}: {e}", f.display())))?;
            if payload.len() > MAX_PAYLOAD_BYTES {
                return Err(Failure::Client(ClientError::Limit(format!(
                    "file {} is {} bytes, over the 16 MB wire cap ({MAX_PAYLOAD_BYTES})",
                    f.display(),
                    payload.len()
                ))));
            }
            match detect_format(&payload) {
                Some(format) => {
                    let size = image_size(format, &payload);
                    Ok(Plan {
                        frames: vec![FrameFile {
                            payload,
                            format,
                            size,
                        }],
                        fps: None,
                        loop_,
                        temp_dir: None,
                    })
                }
                // Not an image: external decode (R4), then animation mode at --fps
                // (default 30, the contract cap).
                None => decode_with_ffmpeg(&f, fps),
            }
        }
        (None, Some(d)) => {
            let fps = fps.ok_or_else(|| {
                usage(format!(
                    "--dir {} needs --fps N (1..={MAX_FPS}) for the animation rate",
                    d.display()
                ))
            })?;
            load_dir(&d, fps, loop_)
        }
        (Some(_), Some(_)) => Err(usage(
            "stream push takes either --file <f> or --dir <d>, not both".to_string(),
        )),
        (None, None) => Err(usage(
            "stream push needs a source: --file <frame> or --dir <frames-dir> --fps N".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_formats_by_magic() {
        assert_eq!(
            detect_format(b"\x89PNG\r\n\x1a\nrest"),
            Some(WireFormat::Png)
        );
        assert_eq!(
            detect_format(b"\xFF\xD8\xFF\xE0jpeg"),
            Some(WireFormat::Jpeg)
        );
        assert_eq!(detect_format(b"GIF89a...."), Some(WireFormat::Gif));
        assert_eq!(detect_format(b"GIF87a...."), Some(WireFormat::Gif));
        assert_eq!(detect_format(b"RIFF....WEBPVP8 "), None);
        assert_eq!(detect_format(b"\x00\x00\x00\x18ftypmp42"), None);
        assert_eq!(detect_format(b""), None);
    }

    #[test]
    fn reads_png_and_gif_sizes() {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&[0, 0, 0, 13]); // IHDR len
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&480u32.to_be_bytes());
        png.extend_from_slice(&272u32.to_be_bytes());
        assert_eq!(image_size(WireFormat::Png, &png), Some((480, 272)));
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&72u16.to_le_bytes());
        gif.extend_from_slice(&72u16.to_le_bytes());
        assert_eq!(image_size(WireFormat::Gif, &gif), Some((72, 72)));
        assert_eq!(image_size(WireFormat::Jpeg, b"\xFF\xD8\xFF"), None);
    }

    #[test]
    fn ffmpeg_argv_is_argv_only() {
        let argv = ffmpeg_argv(Path::new("/tmp/v.mp4"), Path::new("/tmp/x/frames/%04d.png"));
        assert_eq!(
            argv,
            vec![
                "ffmpeg".to_string(),
                "-y".to_string(),
                "-i".to_string(),
                "/tmp/v.mp4".to_string(),
                "/tmp/x/frames/%04d.png".to_string()
            ]
        );
        // no shell metacharacters are interpreted: every element is one argv entry
        assert!(argv
            .iter()
            .all(|a| !a.contains(' ') || a == "/tmp/x/frames/%04d.png"));
    }

    #[test]
    fn fps_bounds_are_the_contract() {
        assert!(check_fps(None).unwrap().is_none());
        assert_eq!(check_fps(Some(1)).unwrap(), Some(1));
        assert_eq!(check_fps(Some(30)).unwrap(), Some(30));
        assert!(check_fps(Some(0)).is_err());
        assert!(check_fps(Some(31)).is_err());
        assert!(check_fps(Some(3000)).is_err());
    }
}
