//! Wire framing for the virtual transport socket (host ↔ ds-vdev).
//!
//! Every message on the socket is `u32 LE body_len | body`, where the body is
//! `u8 kind | u16 LE pid | payload`. `pid` is the USB product id of the module the message is
//! about (vendor id is always [`crate::module::VENDOR_ID`]); `payload` is report bytes for the
//! report kinds and a small command payload for the control kinds. The exact protocol is
//! pinned in `docs/components/virtual-device.md` §"Socket protocol" and ds-vdev (DS-017)
//! implements the other end.

use std::io::{Read, Write};

use crate::error::{Error, Result};

/// Bytes in front of the body: `u32 LE` length.
pub const HEADER_LEN: usize = 4;
/// `u8 kind | u16 LE pid` in front of the payload.
pub const BODY_HEADER_LEN: usize = 3;
/// Largest accepted body (kind + pid + a full 320-byte output report, with headroom).
pub const MAX_FRAME_BODY: usize = 4096;

/// Message kinds. Host→device: `0x01..=0x04`. Device→host: `0x81..=0x84`.
pub mod kind {
    /// Host→device: output report (`hid_write`), payload = report bytes, report id `0x02` first.
    pub const WRITE: u8 = 0x01;
    /// Host→device: set feature report (`hid_send_feature_report`), id `0x03` first.
    pub const FEATURE_SET: u8 = 0x02;
    /// Host→device: get feature report (`hid_get_feature_report`), payload = `[feature_id]`.
    pub const FEATURE_GET: u8 = 0x03;
    /// Host→device: close the connection, no payload.
    pub const CLOSE: u8 = 0x04;
    /// Device→host: input report (`hid_read`), payload = raw report bytes, id `0x01` first.
    pub const INPUT: u8 = 0x81;
    /// Device→host: write accepted, payload = `u32 LE` byte count (the `hid_write` return).
    pub const ACK: u8 = 0x82;
    /// Device→host: feature report answer, payload = answer bytes, report id first.
    pub const ANSWER: u8 = 0x83;
    /// Device→host: refusal/error, payload = UTF-8 message.
    pub const ERROR: u8 = 0x84;

    /// Every kind this codec knows.
    pub const ALL: [u8; 8] = [
        WRITE,
        FEATURE_SET,
        FEATURE_GET,
        CLOSE,
        INPUT,
        ACK,
        ANSWER,
        ERROR,
    ];
}

/// One framed message on the virtual socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Message kind, one of [`kind`].
    pub kind: u8,
    /// USB product id of the module this message is about.
    pub pid: u16,
    /// Kind-specific payload.
    pub payload: Vec<u8>,
}

impl Frame {
    /// Build a frame, rejecting unknown kinds and bodies over [`MAX_FRAME_BODY`].
    pub fn new(kind: u8, pid: u16, payload: Vec<u8>) -> Result<Self> {
        if !kind::ALL.contains(&kind) {
            return Err(Error::BadFrame(format!("unknown kind {kind:#04x}")));
        }
        let body_len = BODY_HEADER_LEN + payload.len();
        if body_len > MAX_FRAME_BODY {
            return Err(Error::BadFrame(format!(
                "body of {body_len} bytes exceeds {MAX_FRAME_BODY}"
            )));
        }
        Ok(Frame { kind, pid, payload })
    }

    /// Encode to `u32 LE body_len | kind | pid LE | payload`.
    pub fn encode(&self) -> Vec<u8> {
        let body_len = (BODY_HEADER_LEN + self.payload.len()) as u32;
        let mut out = Vec::with_capacity(HEADER_LEN + body_len as usize);
        out.extend_from_slice(&body_len.to_le_bytes());
        out.push(self.kind);
        out.extend_from_slice(&self.pid.to_le_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    /// Decode a body (kind + pid + payload, without the length prefix).
    pub fn decode(body: &[u8]) -> Result<Self> {
        if body.len() < BODY_HEADER_LEN {
            return Err(Error::BadFrame(format!(
                "body of {} bytes is shorter than the header",
                body.len()
            )));
        }
        let kind = body[0];
        if !kind::ALL.contains(&kind) {
            return Err(Error::BadFrame(format!("unknown kind {kind:#04x}")));
        }
        let pid = u16::from_le_bytes([body[1], body[2]]);
        Ok(Frame {
            kind,
            pid,
            payload: body[BODY_HEADER_LEN..].to_vec(),
        })
    }
}

/// Write one framed message.
pub fn write_frame(w: &mut impl Write, frame: &Frame) -> Result<()> {
    w.write_all(&frame.encode())?;
    w.flush()?;
    Ok(())
}

/// Read one framed message (blocking).
pub fn read_frame(r: &mut impl Read) -> Result<Frame> {
    let mut header = [0u8; HEADER_LEN];
    r.read_exact(&mut header)?;
    let body_len = u32::from_le_bytes(header) as usize;
    if !(BODY_HEADER_LEN..=MAX_FRAME_BODY).contains(&body_len) {
        return Err(Error::BadFrame(format!(
            "body length {body_len} out of range {BODY_HEADER_LEN}..={MAX_FRAME_BODY}"
        )));
    }
    let mut body = vec![0u8; body_len];
    r.read_exact(&mut body)?;
    Frame::decode(&body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn round_trip_every_kind() {
        for k in kind::ALL {
            let f = Frame::new(k, 0x01FE, vec![0x02, 0x35, 0x00, 0x01, 0x99]).unwrap();
            let enc = f.encode();
            assert_eq!(&enc[..4], &(enc.len() as u32 - 4).to_le_bytes());
            let mut cur = Cursor::new(enc);
            let back = read_frame(&mut cur).unwrap();
            assert_eq!(back, f);
            assert_eq!(Frame::decode(&back.encode()[4..]).unwrap(), f);
        }
    }

    #[test]
    fn empty_payload_round_trips() {
        let f = Frame::new(kind::CLOSE, 0x0206, vec![]).unwrap();
        let mut cur = Cursor::new(f.encode());
        assert_eq!(read_frame(&mut cur).unwrap(), f);
    }

    #[test]
    fn full_size_report_round_trips() {
        let payload = vec![0xAB; 320];
        let f = Frame::new(kind::WRITE, 0x01FE, payload.clone()).unwrap();
        let mut cur = Cursor::new(f.encode());
        assert_eq!(read_frame(&mut cur).unwrap().payload, payload);
    }

    #[test]
    fn rejects_bad_frames() {
        assert!(Frame::new(0x7F, 1, vec![]).is_err());
        assert!(Frame::new(kind::WRITE, 1, vec![0; MAX_FRAME_BODY]).is_err());
        assert!(Frame::decode(&[kind::WRITE, 0xFE]).is_err());
        assert!(Frame::decode(&[0x7F, 0xFE, 0x01]).is_err());
        // length prefix out of range
        let mut cur = Cursor::new((MAX_FRAME_BODY as u32 + 1).to_le_bytes().to_vec());
        assert!(read_frame(&mut cur).is_err());
        let mut cur = Cursor::new(2u32.to_le_bytes().to_vec());
        assert!(read_frame(&mut cur).is_err());
    }

    #[test]
    fn write_frame_then_read_frame() {
        let mut buf = Vec::new();
        let f = Frame::new(kind::FEATURE_GET, 0x0206, vec![0x48]).unwrap();
        write_frame(&mut buf, &f).unwrap();
        let mut cur = Cursor::new(&buf);
        assert_eq!(read_frame(&mut cur).unwrap(), f);
    }
}
