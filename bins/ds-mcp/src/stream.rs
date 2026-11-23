//! The frame-plane half of `deck_stream_push`: control-plane `stream-open` (best
//! effort — a phase-1 daemon may not serve the control op, exactly like
//! `ds stream push`), then one `DSctl stream-open` + one DSF1 binary frame on
//! `<dir>/stream.sock`, honouring the credit grant and waiting for the ack
//! (`docs/contracts/ipc.md` §1.2/§2, `docs/contracts/streaming.md`). Never a plain
//! NDJSON op. No shell, no eval — the payload is read from a file path only.

use crate::conn::{connect, Config, ToolFailure};
use ds_client::ClientError;
use ds_ipc::error::{ErrorCode, IpcError};
use ds_ipc::frames::codec::{write_frame, Frame, FrameMeta, DSCTL_MAGIC};
use ds_ipc::frames::limits::{MAX_DSCTL_LINE_BYTES, MAX_PAYLOAD_BYTES};
use ds_ipc::frames::FrameFormat;
use ds_ipc::framing;
use serde_json::{json, Value};
use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long the push waits for the credit grant / the frame ack (bounded — the
/// per-tool-call timeout bounds the whole call as well).
const ACK_TIMEOUT: Duration = Duration::from_secs(8);

/// Wire format detected from the magic bytes (`schemas/stream-frame.json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WireFormat {
    Png,
    Jpeg,
    Gif,
}

impl WireFormat {
    fn frame_format(self) -> FrameFormat {
        match self {
            Self::Png => FrameFormat::Png,
            Self::Jpeg => FrameFormat::Jpeg,
            Self::Gif => FrameFormat::Gif,
        }
    }
}

/// Detect png/jpeg/gif from the leading magic bytes.
fn detect_format(bytes: &[u8]) -> Option<WireFormat> {
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
fn image_size(format: WireFormat, bytes: &[u8]) -> Option<(u32, u32)> {
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

type PushResult = Result<Value, crate::tools::ToolError>;

/// Push one frame file to `target`. Returns the tool result JSON.
pub fn push_one_frame(cfg: &Config, target: &str, file: &str, credits: Option<u64>) -> PushResult {
    let payload = std::fs::read(file).map_err(|e| {
        crate::tools::ToolError::Daemon(ToolFailure::new(
            ErrorCode::EPath,
            format!("cannot read file {file}: {e}"),
        ))
    })?;
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(crate::tools::ToolError::Daemon(ToolFailure::new(
            ErrorCode::ELimit,
            format!(
                "file {file} is {} bytes (cap {MAX_PAYLOAD_BYTES})",
                payload.len()
            ),
        )));
    }
    let format = detect_format(&payload).ok_or_else(|| {
        crate::tools::ToolError::Daemon(ToolFailure::new(
            ErrorCode::ESchema,
            format!("file {file} is not a png/jpeg/gif image (bad magic bytes)"),
        ))
    })?;
    let size = image_size(format, &payload);

    // Control plane first: daemon reachability (E_UNREACHABLE when down) + token auth,
    // and the contract's control-plane stream-open. A phase-1 daemon that does not
    // serve the control op answers E_INTERNAL/E_SCHEMA — the DSctl stream-open on
    // stream.sock below is what opens the session there, so those two are not fatal.
    let mut c = connect(cfg)?;
    let mut open_credits = credits.unwrap_or(1).clamp(1, 1024) as u32;
    match c.request(json!({"op": "stream-open", "target": target, "credits": open_credits})) {
        Ok(resp) => {
            if let Some(cr) = resp["result"]["credits"].as_u64() {
                open_credits = u32::try_from(cr).unwrap_or(open_credits).clamp(1, 1024);
            }
        }
        Err(ClientError::Server(e))
            if e.code == ErrorCode::EInternal || e.code == ErrorCode::ESchema => {}
        Err(e) => return Err(e.into()),
    }

    let stream_sock = cfg.stream_sock();
    if !stream_sock.exists() {
        return Err(ClientError::Unreachable {
            path: stream_sock.clone(),
            hint: "the daemon's frame stream socket is missing — start it with: ds daemon start"
                .to_string(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "stream.sock missing"),
        }
        .into());
    }
    let mut sc = StreamConn::connect(&stream_sock)?;
    sc.open(target, open_credits)?;
    let meta = FrameMeta {
        target: target.to_string(),
        seq: 1,
        ts_ms: now_ms(),
        format: format.frame_format(),
        w: size.map(|s| s.0),
        h: size.map(|s| s.1),
        frames: None,
        fps: None,
        loop_: None,
        crc32: 0, // filled by Frame::new
        max_payload_bytes: MAX_PAYLOAD_BYTES as u32,
    };
    meta.to_value()
        .map_err(|e| crate::tools::ToolError::Daemon(ToolFailure::from(ClientError::Server(e))))?;
    let frame = Frame::new(meta, &payload);
    sc.send_frame_and_wait_ack(&frame)?;
    // dropping the connection closes the producer session daemon-side
    Ok(json!({
        "ok": true,
        "target": target,
        "sent": 1,
        "seq": 1,
        "bytes": payload.len(),
        "format": match format {
            WireFormat::Png => "png",
            WireFormat::Jpeg => "jpeg",
            WireFormat::Gif => "gif",
        },
    }))
}

/// One connection to the daemon's stream socket: `DSctl\n` control lines + DSF1 frames.
struct StreamConn {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl StreamConn {
    fn connect(path: &Path) -> Result<Self, crate::tools::ToolError> {
        let writer = UnixStream::connect(path).map_err(|source| {
            crate::tools::ToolError::Daemon(ToolFailure::from(ClientError::Unreachable {
                path: path.to_path_buf(),
                hint: "the daemon's frame stream socket is unreachable — start it with: ds daemon start".to_string(),
                source,
            }))
        })?;
        let _ = writer.set_read_timeout(Some(ACK_TIMEOUT));
        let reader = BufReader::new(writer.try_clone().map_err(|e| {
            crate::tools::ToolError::Daemon(ToolFailure::new(
                ErrorCode::EInternal,
                format!("cannot clone stream socket: {e}"),
            ))
        })?);
        Ok(Self { reader, writer })
    }

    fn send_dsctl(&mut self, v: &Value) -> Result<(), crate::tools::ToolError> {
        let line = serde_json::to_vec(v).map_err(|e| {
            crate::tools::ToolError::Daemon(ToolFailure::new(
                ErrorCode::EInternal,
                format!("cannot serialize DSctl line: {e}"),
            ))
        })?;
        if line.len() + 1 > MAX_DSCTL_LINE_BYTES {
            return Err(crate::tools::ToolError::Daemon(ToolFailure::new(
                ErrorCode::ELimit,
                format!(
                    "DSctl line would be {} bytes (cap {MAX_DSCTL_LINE_BYTES})",
                    line.len() + 1
                ),
            )));
        }
        self.writer
            .write_all(DSCTL_MAGIC)
            .and_then(|_| self.writer.write_all(&line))
            .and_then(|_| self.writer.write_all(b"\n"))
            .and_then(|_| self.writer.flush())
            .map_err(|e| {
                crate::tools::ToolError::Daemon(ToolFailure::new(
                    ErrorCode::EInternal,
                    format!("stream socket: {e}"),
                ))
            })?;
        Ok(())
    }

    fn recv_dsctl(&mut self) -> Result<Value, crate::tools::ToolError> {
        let mut magic = [0u8; 6];
        self.reader.read_exact(&mut magic).map_err(|e| {
            crate::tools::ToolError::Daemon(ToolFailure::new(
                ErrorCode::EInternal,
                format!("stream socket read: {e}"),
            ))
        })?;
        if &magic[..4] == b"DSF1" {
            return Err(crate::tools::ToolError::Daemon(ToolFailure::new(
                ErrorCode::EInternal,
                "unexpected DSF1 frame from the daemon on the stream socket",
            )));
        }
        if &magic != b"DSctl\n" {
            return Err(crate::tools::ToolError::Daemon(ToolFailure::new(
                ErrorCode::EInternal,
                "stream socket message is neither DSF1 nor DSctl",
            )));
        }
        let line = framing::read_line(&mut self.reader, MAX_DSCTL_LINE_BYTES).map_err(|e| {
            crate::tools::ToolError::Daemon(ToolFailure::new(
                ErrorCode::EInternal,
                format!("bad DSctl line: {}", e.to_ipc_error().message),
            ))
        })?;
        let v: Value = serde_json::from_str(line.trim()).map_err(|e| {
            crate::tools::ToolError::Daemon(ToolFailure::new(
                ErrorCode::EInternal,
                format!("bad DSctl line: {e}"),
            ))
        })?;
        Ok(v)
    }

    /// `DSctl stream-open {target, credits}` then wait for the `stream-credit` grant.
    fn open(&mut self, target: &str, credits: u32) -> Result<(), crate::tools::ToolError> {
        self.send_dsctl(&json!({"op": "stream-open", "target": target, "credits": credits}))?;
        let deadline = Instant::now() + ACK_TIMEOUT;
        loop {
            let msg = self.recv_dsctl()?;
            if msg.get("ok") == Some(&Value::Bool(false)) {
                let err = msg
                    .get("error")
                    .and_then(IpcError::from_value)
                    .unwrap_or_else(|| IpcError::new(ErrorCode::EInternal, "stream-open refused"));
                return Err(crate::tools::ToolError::Daemon(ToolFailure::from(
                    ClientError::Server(err),
                )));
            }
            if msg.get("op").and_then(Value::as_str) == Some("stream-credit")
                && msg.get("target").and_then(Value::as_str) == Some(target)
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(crate::tools::ToolError::Daemon(ToolFailure::new(
                    ErrorCode::ELimit,
                    "stream-open: no stream-credit grant within the timeout (E_LIMIT backpressure timeout)",
                )));
            }
        }
    }

    /// Write one DSF1 frame and wait for its `stream-ack {seq}` (bounded).
    fn send_frame_and_wait_ack(&mut self, frame: &Frame) -> Result<(), crate::tools::ToolError> {
        write_frame(&mut self.writer, frame).map_err(|e| {
            crate::tools::ToolError::Daemon(ToolFailure::from(ClientError::Server(e)))
        })?;
        self.writer.flush().map_err(|e| {
            crate::tools::ToolError::Daemon(ToolFailure::new(
                ErrorCode::EInternal,
                format!("stream socket: {e}"),
            ))
        })?;
        let deadline = Instant::now() + ACK_TIMEOUT;
        loop {
            let msg = self.recv_dsctl()?;
            match msg.get("op").and_then(Value::as_str) {
                Some("stream-ack")
                    if msg.get("seq").and_then(Value::as_u64) == Some(frame.meta.seq) =>
                {
                    return Ok(());
                }
                Some("stream-nack") => {
                    let err = msg
                        .get("error")
                        .and_then(IpcError::from_value)
                        .unwrap_or_else(|| IpcError::new(ErrorCode::ELimit, "stream-nack"));
                    return Err(crate::tools::ToolError::Daemon(ToolFailure::from(
                        ClientError::Server(err),
                    )));
                }
                _ if msg.get("ok") == Some(&Value::Bool(false)) => {
                    let err = msg
                        .get("error")
                        .and_then(IpcError::from_value)
                        .unwrap_or_else(|| IpcError::new(ErrorCode::EInternal, "stream error"));
                    return Err(crate::tools::ToolError::Daemon(ToolFailure::from(
                        ClientError::Server(err),
                    )));
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err(crate::tools::ToolError::Daemon(ToolFailure::new(
                    ErrorCode::ELimit,
                    "no stream-ack within the timeout (E_LIMIT backpressure timeout)",
                )));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_image_formats_from_magic_bytes() {
        let mut png = vec![0u8; 24];
        png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        png[16..20].copy_from_slice(&480u32.to_be_bytes());
        png[20..24].copy_from_slice(&272u32.to_be_bytes());
        assert_eq!(detect_format(&png), Some(WireFormat::Png));
        assert_eq!(image_size(WireFormat::Png, &png), Some((480, 272)));
        assert_eq!(
            detect_format(b"\xFF\xD8\xFF\xE0jpeg"),
            Some(WireFormat::Jpeg)
        );
        assert_eq!(detect_format(b"GIF89a...."), Some(WireFormat::Gif));
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&[0x20, 0x00, 0x10, 0x00]); // 32 x 16 LE
        gif.resize(10, 0);
        assert_eq!(image_size(WireFormat::Gif, &gif), Some((32, 16)));
        assert_eq!(detect_format(b"not an image"), None);
        assert_eq!(detect_format(b""), None);
    }

    #[test]
    fn frame_meta_for_a_single_still_is_valid() {
        let meta = FrameMeta {
            target: "keys".to_string(),
            seq: 1,
            ts_ms: 42,
            format: FrameFormat::Png,
            w: Some(480),
            h: Some(272),
            frames: None,
            fps: None,
            loop_: None,
            crc32: 0,
            max_payload_bytes: MAX_PAYLOAD_BYTES as u32,
        };
        let v = meta.to_value().unwrap();
        assert_eq!(v["target"], json!("keys"));
        assert_eq!(v["seq"], json!(1));
        assert!(v.get("fps").is_none() || v["fps"].is_null());
        let frame = Frame::new(meta, b"\x89PNG-fake");
        let wire = frame.encode().unwrap();
        assert_eq!(&wire[..4], b"DSF1");
    }
}
