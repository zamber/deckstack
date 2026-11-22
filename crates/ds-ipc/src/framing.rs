//! NDJSON framing for the control plane (`docs/contracts/ipc.md` §2): one JSON object per
//! `\n`-terminated line, one request line → one response line. Lines longer than
//! `max_line` are rejected with `E_LIMIT` **before** parsing (the cap exists so a hostile
//! peer cannot make us buffer unbounded input); a line that is not exactly one JSON
//! value gets `E_SCHEMA`.

use crate::error::{ErrorCode, IpcError};
use std::io::{BufRead, Read, Write};

/// Why reading one line failed.
#[derive(Debug)]
pub enum FrameError {
    /// The line exceeded the configured byte cap → `E_LIMIT`, and the connection must be
    /// closed (the framing can no longer be trusted).
    TooLong,
    /// Clean EOF (the peer closed the connection).
    Eof,
    /// Underlying I/O error, incl. read timeouts (idle close).
    Io(std::io::Error),
}

impl FrameError {
    /// The error envelope for this failure (id-less — no request could be recovered for
    /// `TooLong`).
    pub fn to_ipc_error(&self) -> IpcError {
        match self {
            Self::TooLong => IpcError::new(
                ErrorCode::ELimit,
                "request line exceeds the 1 MiB NDJSON limit",
            ),
            Self::Eof => IpcError::new(ErrorCode::EUnreachable, "connection closed by peer"),
            Self::Io(e) => IpcError::new(ErrorCode::EInternal, format!("i/o error: {e}")),
        }
    }

    /// Whether the connection must be closed after this error.
    pub fn is_fatal(&self) -> bool {
        !matches!(self, Self::Eof)
    }
}

/// Read one NDJSON line (without the trailing `\n`; a trailing `\r` is stripped) from
/// `reader`, enforcing `max_line` bytes of raw wire data (newline included). Returns the
/// line or a [`FrameError`].
///
/// The read goes through a `Take` capped at `max_line + 1` bytes, so a peer that never
/// sends a newline cannot make us buffer more than the cap — the limit is enforced
/// before any parsing.
pub fn read_line(reader: &mut impl BufRead, max_line: usize) -> Result<String, FrameError> {
    let mut buf: Vec<u8> = Vec::new();
    let n = reader
        .by_ref()
        .take(max_line as u64 + 1)
        .read_until(b'\n', &mut buf)
        .map_err(FrameError::Io)?;
    if n == 0 {
        return Err(FrameError::Eof);
    }
    if buf.len() > max_line {
        return Err(FrameError::TooLong);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    String::from_utf8(buf).map_err(|_| {
        FrameError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "line is not valid UTF-8",
        ))
    })
}

/// Serialize `value` as one NDJSON line and write it (with `\n`) to `writer`, enforcing
/// `max_line` on the serialized form. Returns the number of bytes written (incl. `\n`).
pub fn write_line(
    writer: &mut impl Write,
    value: &serde_json::Value,
    max_line: usize,
) -> Result<usize, FrameError> {
    let mut line = serde_json::to_vec(value).map_err(|e| {
        FrameError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            e.to_string(),
        ))
    })?;
    if line.len() + 1 > max_line {
        return Err(FrameError::TooLong);
    }
    line.push(b'\n');
    writer.write_all(&line).map_err(FrameError::Io)?;
    writer.flush().map_err(FrameError::Io)?;
    Ok(line.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trip_lines() {
        let mut buf: Vec<u8> = Vec::new();
        let v = json!({"id": 1, "op": "ping"});
        let n = write_line(&mut buf, &v, 1024).unwrap();
        assert_eq!(n, buf.len());
        assert!(buf.ends_with(b"\n"));
        let mut cursor = std::io::BufReader::new(&buf[..]);
        let line = read_line(&mut cursor, 1024).unwrap();
        assert_eq!(line, serde_json::to_string(&v).unwrap());
        // CRLF tolerated
        let mut crlf = b"{\"op\":\"ping\"}\r\n".to_vec();
        let mut cursor = std::io::BufReader::new(&crlf[..]);
        assert_eq!(read_line(&mut cursor, 1024).unwrap(), "{\"op\":\"ping\"}");
        crlf.clear();
        // EOF on empty
        let mut cursor = std::io::BufReader::new(&[][..]);
        assert!(matches!(read_line(&mut cursor, 1024), Err(FrameError::Eof)));
        // EOF after a final unterminated line still returns the line
        let mut cursor = std::io::BufReader::new(&b"{\"op\":\"ping\"}"[..]);
        assert_eq!(read_line(&mut cursor, 1024).unwrap(), "{\"op\":\"ping\"}");
    }

    #[test]
    fn oversized_line_rejected_before_parsing() {
        let prefix = b"{\"op\":\"x\",\"pad\":\"";
        let mut line: Vec<u8> = prefix.repeat(100);
        line.push(b'"');
        line.extend(std::iter::repeat_n(b'a', 2048));
        line.push(b'\n');
        let mut cursor = std::io::BufReader::new(&line[..]);
        match read_line(&mut cursor, 1024) {
            Err(FrameError::TooLong) => {}
            other => panic!("expected TooLong, got {other:?}"),
        }
        let err = FrameError::TooLong.to_ipc_error();
        assert_eq!(err.code, ErrorCode::ELimit);
        assert!(FrameError::TooLong.is_fatal());
        // exactly at the cap is fine
        let line = vec![b'a'; 1023];
        let mut with_nl = line.clone();
        with_nl.push(b'\n');
        let mut cursor = std::io::BufReader::new(&with_nl[..]);
        assert_eq!(read_line(&mut cursor, 1024).unwrap().len(), 1023);
    }

    #[test]
    fn oversized_write_rejected() {
        let mut buf: Vec<u8> = Vec::new();
        let v = json!({"pad": "x".repeat(2048)});
        assert!(matches!(
            write_line(&mut buf, &v, 1024),
            Err(FrameError::TooLong)
        ));
        assert!(buf.is_empty());
    }
}
