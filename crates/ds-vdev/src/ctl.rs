//! The client side of the control socket (`<socket>.ctl`): what `ds-vdevctl` uses to inspect
//! and steer a running ds-vdev. One request line in, then `OK` + body lines + a `.` line out
//! (or a single `ERR <message>` line).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

/// The control socket of a device socket (same as [`crate::server::control_path`]).
pub fn control_path(socket: &Path) -> std::path::PathBuf {
    crate::server::control_path(socket)
}

/// Send one control request line to the control socket of `socket` and return the body lines.
/// An `ERR` reply surfaces as an `io::Error` with the device's message.
pub fn request(socket: &Path, line: &str) -> std::io::Result<Vec<String>> {
    request_at(&control_path(socket), line)
}

/// Same as [`request`], but against an explicit control socket path.
pub fn request_at(ctl: &Path, line: &str) -> std::io::Result<Vec<String>> {
    let mut stream = UnixStream::connect(ctl)?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader.read_line(&mut status)?;
    let status = status.trim_end();
    if let Some(msg) = status.strip_prefix("ERR") {
        return Err(std::io::Error::other(msg.trim().to_string()));
    }
    if status != "OK" {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("bad control reply status: {status:?}"),
        ));
    }
    let mut lines = Vec::new();
    loop {
        let mut l = String::new();
        if reader.read_line(&mut l)? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "control socket closed before '.'",
            ));
        }
        let l = l.trim_end();
        if l == "." {
            return Ok(lines);
        }
        lines.push(l.to_string());
    }
}
