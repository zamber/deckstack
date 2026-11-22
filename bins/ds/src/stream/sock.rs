//! The producer-side stream-socket client: `DSctl\n` NDJSON control lines plus DSF1
//! binary frames on `<dir>/stream.sock` (`docs/contracts/ipc.md` §2,
//! `docs/contracts/streaming.md` §7, `schemas/stream-frame.json`).
//!
//! Flow: connect → `DSctl {"op":"stream-open","target":…,"credits":N}` → wait for the
//! `stream-credit` grant → per frame write DSF1 and read `DSctl` replies until
//! `stream-ack {seq}`. Credit handling honours the daemon's grants: with an empty local
//! window the producer **waits for a `stream-credit` re-grant** (bounded by
//! [`BACKPRESSURE_TIMEOUT`](super::BACKPRESSURE_TIMEOUT)) instead of buffering frames
//! unboundedly; `stream-nack` + `E_LIMIT` is backpressure (wait + resend, exit 5 on
//! timeout), any other nack code is fatal with its stable exit code. The CLI never spins.

use super::{unreachable_stream, BACKPRESSURE_TIMEOUT};
use crate::output::{server_failure, Failure};
use ds_client::ClientError;
use ds_ipc::error::{ErrorCode, IpcError};
use ds_ipc::frames::codec::{write_frame, Frame, DSCTL_MAGIC};
use ds_ipc::frames::limits::MAX_DSCTL_LINE_BYTES;
use ds_ipc::framing;
use serde_json::{json, Value};
use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

/// One connection to the daemon's stream socket.
pub struct StreamConn {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    /// Local credit window (grants minus acked frames).
    credits: u32,
}

impl StreamConn {
    /// Connect to `stream.sock` (missing/unreachable → exit 6 `E_UNREACHABLE`).
    pub fn connect(path: &Path) -> Result<Self, Failure> {
        let writer = UnixStream::connect(path).map_err(|source| {
            let Failure::Client(ClientError::Unreachable { hint, .. }) = unreachable_stream(path)
            else {
                unreachable!("unreachable_stream is always an Unreachable failure")
            };
            Failure::Client(ClientError::Unreachable {
                path: path.to_path_buf(),
                hint,
                source,
            })
        })?;
        // Reads block at most BACKPRESSURE_TIMEOUT; a stall is a backpressure timeout
        // (exit 5), never a hang.
        let _ = writer.set_read_timeout(Some(BACKPRESSURE_TIMEOUT));
        let reader = BufReader::new(
            writer
                .try_clone()
                .map_err(|e| Failure::Usage(format!("cannot clone stream socket: {e}")))?,
        );
        Ok(Self {
            reader,
            writer,
            credits: 0,
        })
    }

    /// Send one `DSctl\n` NDJSON control line.
    pub fn send_dsctl(&mut self, v: &Value) -> Result<(), Failure> {
        let line = serde_json::to_vec(v)
            .map_err(|e| Failure::Usage(format!("cannot serialize DSctl line: {e}")))?;
        if line.len() + 1 > MAX_DSCTL_LINE_BYTES {
            return Err(Failure::Client(ClientError::Limit(format!(
                "DSctl line would be {} bytes (cap {MAX_DSCTL_LINE_BYTES})",
                line.len() + 1
            ))));
        }
        self.writer
            .write_all(DSCTL_MAGIC)
            .and_then(|_| self.writer.write_all(&line))
            .and_then(|_| self.writer.write_all(b"\n"))
            .and_then(|_| self.writer.flush())
            .map_err(|e| Failure::Client(ClientError::Protocol(format!("stream socket: {e}"))))?;
        Ok(())
    }

    /// Read one `DSctl\n` control line from the stream socket (magic included).
    pub fn recv_dsctl(&mut self) -> Result<Value, Failure> {
        let mut magic = [0u8; 4];
        self.reader
            .read_exact(&mut magic)
            .map_err(|e| self.io_failure(e))?;
        if &magic == b"DSF1" {
            // The daemon never sends frames to a producer.
            return Err(Failure::Client(ClientError::Protocol(
                "unexpected DSF1 frame from the daemon on the stream socket".to_string(),
            )));
        }
        if magic != *b"DSct" {
            return Err(Failure::Client(ClientError::Protocol(
                "stream socket message is neither DSF1 nor DSctl".to_string(),
            )));
        }
        let mut rest = [0u8; 2];
        self.reader
            .read_exact(&mut rest)
            .map_err(|e| self.io_failure(e))?;
        if &rest != b"l\n" {
            return Err(Failure::Client(ClientError::Protocol(
                "stream socket message is neither DSF1 nor DSctl".to_string(),
            )));
        }
        let line = framing::read_line(&mut self.reader, MAX_DSCTL_LINE_BYTES)
            .map_err(|e| self.io_failure_io(e))?;
        let v: Value = serde_json::from_str(line.trim())
            .map_err(|e| Failure::Client(ClientError::Protocol(format!("bad DSctl line: {e}"))))?;
        Ok(v)
    }

    /// `DSctl stream-open {target, credits}` and wait for the `stream-credit` grant.
    pub fn open(&mut self, target: &str, credits: u32) -> Result<(), Failure> {
        self.send_dsctl(&json!({"op": "stream-open", "target": target, "credits": credits}))?;
        let deadline = Instant::now() + BACKPRESSURE_TIMEOUT;
        loop {
            let msg = self.recv_dsctl()?;
            if msg.get("ok") == Some(&Value::Bool(false)) {
                let err = msg
                    .get("error")
                    .and_then(IpcError::from_value)
                    .unwrap_or_else(|| IpcError::new(ErrorCode::EInternal, "stream-open refused"));
                return Err(server_failure(err));
            }
            match msg.get("op").and_then(Value::as_str) {
                Some("stream-credit")
                    if msg.get("target").and_then(Value::as_str) == Some(target) =>
                {
                    self.credits = msg.get("credits").and_then(Value::as_u64).unwrap_or(0) as u32;
                    return Ok(());
                }
                // interleaved ack/nack lines before the grant: keep reading
                _ => {
                    if Instant::now() >= deadline {
                        return Err(backpressure_timeout(
                            "stream-open: no stream-credit grant within the backpressure timeout",
                        ));
                    }
                }
            }
        }
    }

    /// Wait for a `stream-credit` re-grant for `target` (backpressure: the daemon grants
    /// as its queue drains). Bounded; on timeout → `E_LIMIT` exit 5.
    fn wait_credit(&mut self, target: &str, deadline: Instant) -> Result<(), Failure> {
        loop {
            if Instant::now() >= deadline {
                return Err(backpressure_timeout(
                    "stream-credit not re-granted within the backpressure timeout",
                ));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let _ = self
                .writer
                .set_read_timeout(Some(remaining.max(Duration::from_millis(50))));
            let msg = self.recv_dsctl()?;
            match msg.get("op").and_then(Value::as_str) {
                Some("stream-credit")
                    if msg.get("target").and_then(Value::as_str) == Some(target) =>
                {
                    let c = msg.get("credits").and_then(Value::as_u64).unwrap_or(0) as u32;
                    self.credits = self.credits.saturating_add(c);
                    let _ = self.writer.set_read_timeout(Some(BACKPRESSURE_TIMEOUT));
                    return Ok(());
                }
                Some("stream-ack") => {
                    // a late ack: not a credit, keep waiting
                }
                _ if msg.get("ok") == Some(&Value::Bool(false)) => {
                    let err = msg
                        .get("error")
                        .and_then(IpcError::from_value)
                        .unwrap_or_else(|| IpcError::new(ErrorCode::EInternal, "stream error"));
                    let _ = self.writer.set_read_timeout(Some(BACKPRESSURE_TIMEOUT));
                    return Err(server_failure(err));
                }
                _ => {}
            }
        }
    }

    /// Send one DSF1 frame and wait for its `stream-ack`, honouring credits and
    /// backpressure nacks (wait for replenishment, bounded; exit 5 on timeout).
    pub fn send_frame(&mut self, target: &str, frame: &Frame) -> Result<(), Failure> {
        let deadline = Instant::now() + BACKPRESSURE_TIMEOUT;
        let seq = frame.meta.seq;
        // Never buffer unboundedly: with an empty credit window, wait for the daemon's
        // re-grant instead of firing the frame into a nack.
        while self.credits == 0 {
            self.wait_credit(target, deadline)?;
        }
        loop {
            write_frame(&mut self.writer, frame)
                .map_err(|e| Failure::Client(ClientError::Server(e)))?;
            self.writer.flush().map_err(|e| {
                Failure::Client(ClientError::Protocol(format!("stream socket: {e}")))
            })?;
            loop {
                let msg = self.recv_dsctl()?;
                match msg.get("op").and_then(Value::as_str) {
                    Some("stream-ack") if msg.get("seq").and_then(Value::as_u64) == Some(seq) => {
                        self.credits = self.credits.saturating_sub(1);
                        return Ok(());
                    }
                    Some("stream-credit")
                        if msg.get("target").and_then(Value::as_str) == Some(target) =>
                    {
                        let c = msg.get("credits").and_then(Value::as_u64).unwrap_or(0) as u32;
                        self.credits = self.credits.saturating_add(c);
                    }
                    Some("stream-nack") => {
                        let err = msg
                            .get("error")
                            .and_then(IpcError::from_value)
                            .unwrap_or_else(|| {
                                IpcError::new(ErrorCode::EInternal, "stream-nack without error")
                            });
                        if err.code == ErrorCode::ELimit {
                            // Backpressure: rate limit (credits were not spent) or an
                            // exhausted window. Wait for replenishment, then resend the
                            // same frame — bounded by the deadline (exit 5 on timeout).
                            if self.credits == 0 {
                                self.wait_credit(target, deadline)?;
                            } else {
                                std::thread::sleep(Duration::from_millis(25));
                                if Instant::now() >= deadline {
                                    return Err(backpressure_timeout(
                                        "frame rate backpressure did not clear within the timeout",
                                    ));
                                }
                            }
                            break; // resend the frame (outer loop)
                        }
                        return Err(server_failure(err));
                    }
                    _ if msg.get("ok") == Some(&Value::Bool(false)) => {
                        let err = msg
                            .get("error")
                            .and_then(IpcError::from_value)
                            .unwrap_or_else(|| IpcError::new(ErrorCode::EInternal, "stream error"));
                        return Err(server_failure(err));
                    }
                    _ => {} // unrelated async line: ignore
                }
            }
        }
    }

    /// Read and discard control lines until the socket goes quiet (used by `stream stop`;
    /// the stop reply is `stream-credit credits:0`, but EOF/error also means "gone").
    pub fn drain_until_quiet(&mut self, max: Duration) -> Result<(), Failure> {
        let deadline = Instant::now() + max;
        loop {
            if Instant::now() >= deadline {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let _ = self
                .writer
                .set_read_timeout(Some(remaining.max(Duration::from_millis(50))));
            match self.recv_dsctl() {
                Ok(_) => {}
                Err(_) => {
                    let _ = self.writer.set_read_timeout(Some(BACKPRESSURE_TIMEOUT));
                    return Ok(());
                }
            }
        }
    }

    /// Close the stream connection.
    pub fn close(self) {}

    fn io_failure(&self, e: std::io::Error) -> Failure {
        if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut {
            backpressure_timeout("stream socket read timed out (backpressure timeout)")
        } else if e.kind() == std::io::ErrorKind::UnexpectedEof {
            Failure::Client(ClientError::Protocol(
                "stream socket closed by the daemon".to_string(),
            ))
        } else {
            Failure::Client(ClientError::Protocol(format!("stream socket: {e}")))
        }
    }

    fn io_failure_io(&self, e: framing::FrameError) -> Failure {
        match e {
            framing::FrameError::Eof => Failure::Client(ClientError::Protocol(
                "stream socket closed by the daemon".to_string(),
            )),
            framing::FrameError::TooLong => Failure::Client(ClientError::Limit(
                "DSctl line exceeds the 1 MiB cap".to_string(),
            )),
            framing::FrameError::Io(io) => self.io_failure(io),
        }
    }
}

/// The backpressure-timeout failure: `E_LIMIT`, exit 5.
pub fn backpressure_timeout(msg: &str) -> Failure {
    Failure::Client(ClientError::Server(IpcError::new(
        ErrorCode::ELimit,
        format!("{msg} (E_LIMIT backpressure timeout)"),
    )))
}
