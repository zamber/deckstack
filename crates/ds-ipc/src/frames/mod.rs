//! The frame plane: DSF1 binary frames on the stream socket (`<dir>/stream.sock`),
//! with `DSctl\n` NDJSON stream control, per-connection credit windows, ack/nack per
//! frame, per-target and global limits, and backpressure (`docs/contracts/streaming.md`,
//! `docs/contracts/ipc.md` §2, `schemas/stream-frame.json`).
//!
//! - [`codec`] — the DSF1 wire codec: `"DSF1" | u32 LE meta_len | meta JSON | u64 LE
//!   payload_len | payload`, meta validated against the embedded
//!   `schemas/stream-frame.json`, payload CRC32-checked.
//! - [`limits`] — the contract limits (16 MB wire cap, queue depth 2, 256 MiB queue
//!   memory, 30 fps, 1024 credits) and the tunable [`limits::StreamConfig`].
//! - [`server`] — the stream socket server: accept loop, `DSctl` `stream-open` /
//!   `stream-stop`, per-connection per-target credit window, `stream-ack` /
//!   `stream-nack` / `stream-credit` replies, [`server::StreamEvent`] backpressure
//!   signal, per-target in-memory stats.
//!
//! **S1 — frames never enter JSON state and never bump `rev`:** this module takes no
//! state handle and `ds-ipc` does not depend on `ds-state` (asserted by test). The only
//! outputs of a frame push are the socket ack and in-memory telemetry counters.
//!
//! ```rust
//! use ds_ipc::frames::{Frame, FrameFormat, FrameMeta};
//! use ds_ipc::frames::limits::MAX_PAYLOAD_BYTES;
//!
//! let meta = FrameMeta {
//!     target: "keys".into(),
//!     seq: 1,
//!     ts_ms: 1_732_000_000_000,
//!     format: FrameFormat::Png,
//!     w: Some(480), h: Some(272), frames: Some(1), fps: None, loop_: None,
//!     crc32: 0,
//!     max_payload_bytes: MAX_PAYLOAD_BYTES as u32,
//! };
//! let frame = Frame::new(meta, b"\x89PNG...");
//! let wire = frame.encode().unwrap();
//! assert_eq!(&wire[..4], b"DSF1");
//! ```

pub mod codec;
pub mod limits;
pub mod server;

#[cfg(test)]
mod tests;

pub use codec::{
    crc32, read_frame, read_frame_header, read_payload, validate_meta, write_frame, DecodeError,
    Frame, FrameFormat, FrameHeader, FrameMeta, DSCTL_MAGIC, DSF1_MAGIC, MAX_META_BYTES,
    STREAM_FRAME_SCHEMA_JSON,
};
pub use limits::StreamConfig;
pub use server::{FrameSink, StreamBus, StreamEvent, StreamServer, TargetStats};
