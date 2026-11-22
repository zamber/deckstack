//! The frame-plane limits (`docs/contracts/streaming.md` §5, `docs/contracts/ipc.md` §5)
//! and the tunable [`StreamConfig`]. All values are the contract values by default.

use std::time::Duration;

/// Wire frame cap: **16 MB** payload (`MAX_BODY` precedent
/// `upstream/.../plugins.rs:266`; `schemas/stream-frame.json` `max_payload_bytes`).
pub const MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;

/// Cap for the frame meta JSON blob (the wire constant lives in [`super::codec`]).
pub const MAX_META_BYTES: usize = super::codec::MAX_META_BYTES as usize;

/// Per-target queue depth: **2** (1 in flight + 1 waiting, latest-frame-wins)
/// (`docs/contracts/streaming.md` §3/§5).
pub const QUEUE_DEPTH_PER_TARGET: usize = 2;

/// Total frame-queue memory cap: **256 MiB** (`docs/contracts/streaming.md` §5,
/// `docs/contracts/ipc.md` §5).
pub const MAX_QUEUE_MEMORY_BYTES: usize = 256 * 1024 * 1024;

/// Maximum animation length: **300 frames** (schema-enforced in
/// `schemas/stream-frame.json`).
pub const MAX_ANIMATION_FRAMES: u32 = 300;

/// Maximum playback rate: **30 fps** (schema-enforced), i.e. a minimum of **0.02 s**
/// between frames of one target (`docs/contracts/streaming.md` §2).
pub const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(20);

/// Maximum credits a single `stream-open` may grant: **1024** (the schema's
/// `stream-open.credits` maximum).
pub const MAX_CREDITS_PER_OPEN: u32 = 1024;

/// Stream-socket connection cap: **64** (`docs/contracts/ipc.md` §5 global connection
/// limit applies to the daemon's sockets).
pub const MAX_STREAM_CONNECTIONS: usize = crate::limits::MAX_CONNECTIONS;

/// Stream-socket control-line cap (the `DSctl\n` NDJSON lines): **1 MiB**, same as the
/// control plane (`docs/contracts/ipc.md` §2).
pub const MAX_DSCTL_LINE_BYTES: usize = crate::limits::MAX_LINE_BYTES;

/// Idle stream-connection timeout: **300 s** (`docs/contracts/ipc.md` §5).
pub const STREAM_IDLE_TIMEOUT: Duration = crate::limits::IDLE_TIMEOUT;

/// Stream server configuration. Defaults are the contract values above.
#[derive(Debug, Clone)]
pub struct StreamConfig {
    /// Payload cap per frame (wire).
    pub max_payload_bytes: usize,
    /// Meta JSON cap per frame.
    pub max_meta_bytes: usize,
    /// Per-target queue depth (1 in flight + 1 waiting).
    pub queue_depth_per_target: usize,
    /// Global frame-queue memory cap.
    pub max_queue_memory_bytes: usize,
    /// Minimum interval between two frames of the same target (≤ 30 fps).
    /// `Duration::ZERO` disables the rate limit (tests).
    pub min_frame_interval: Duration,
    /// Maximum credits one `stream-open` may grant.
    pub max_credits_per_open: u32,
    /// Maximum simultaneous stream connections.
    pub max_connections: usize,
    /// `DSctl\n` control-line cap.
    pub max_dsctl_line_bytes: usize,
    /// Idle connection timeout; `None` disables it.
    pub idle_timeout: Option<Duration>,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            max_payload_bytes: MAX_PAYLOAD_BYTES,
            max_meta_bytes: MAX_META_BYTES,
            queue_depth_per_target: QUEUE_DEPTH_PER_TARGET,
            max_queue_memory_bytes: MAX_QUEUE_MEMORY_BYTES,
            min_frame_interval: MIN_FRAME_INTERVAL,
            max_credits_per_open: MAX_CREDITS_PER_OPEN,
            max_connections: MAX_STREAM_CONNECTIONS,
            max_dsctl_line_bytes: MAX_DSCTL_LINE_BYTES,
            idle_timeout: Some(STREAM_IDLE_TIMEOUT),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_contract() {
        let c = StreamConfig::default();
        assert_eq!(c.max_payload_bytes, 16 * 1024 * 1024);
        assert_eq!(c.max_meta_bytes, 64 * 1024);
        assert_eq!(c.queue_depth_per_target, 2);
        assert_eq!(c.max_queue_memory_bytes, 256 * 1024 * 1024);
        assert_eq!(c.min_frame_interval, Duration::from_millis(20));
        assert_eq!(c.max_credits_per_open, 1024);
        assert_eq!(c.max_connections, 64);
        assert_eq!(c.max_dsctl_line_bytes, 1024 * 1024);
        assert_eq!(c.idle_timeout, Some(Duration::from_secs(300)));
    }
}
