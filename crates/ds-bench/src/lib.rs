//! `ds-bench` library: the streaming bench harness (DS-033).
//!
//! - [`stats`] — latency series + nearest-rank percentiles (unit-tested).
//! - [`budget`] — the budget evaluation against `docs/30-testing.md` §5 (unit-tested).
//! - [`stack`] — the in-process vdev + control server + frame-service stack.
//! - [`stream`] — the bench runner (producer + query loop + measurements).
//! - [`report`] — the table / `--json` output.
//!
//! ```rust,no_run
//! use ds_bench::stream::{run, BenchOptions};
//! let report = run(&BenchOptions { seconds: 5, ..Default::default() }).unwrap();
//! assert!(report.ok());
//! ```

pub mod budget;
pub mod report;
pub mod stack;
pub mod stats;
pub mod stream;

/// Crate version, read from `Cargo.toml` at compile time.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_metadata() {
        assert!(!version().is_empty());
        assert_eq!(stream::DEFAULT_FPS, 32);
        assert_eq!(stream::DEFAULT_SECONDS, 60);
        // ~2.1 MB/s simulated device cap in kbit/s
        assert_eq!(stream::DEFAULT_THROTTLE_KBPS, 16800);
        assert_eq!(budget::MEMORY_CAP_BYTES, 256 * 1024 * 1024);
    }
}
