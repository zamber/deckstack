//! The `--ci` preset for ds-bench (DS-082): a deterministic, bounded CI gate run.
//!
//! `--ci` pins the streaming window to [`CI_SECONDS`] (10 s) unless `--seconds` is
//! given explicitly, implies `--json` on stdout, and writes the same JSON report to
//! [`CI_REPORT_FILE`] (overridable with `--out PATH`) so CI can upload it as an
//! artifact. Everything else — 32 fps pacing, the 16800 kbps vdev throttle, the
//! `key:0` target, the budgets of `docs/30-testing.md` §5 — stays at the DS-033
//! defaults, so a CI run is directly comparable to a local
//! `cargo run -p ds-bench -- --ci` (recorded in docs/30-testing.md §9).

use std::path::{Path, PathBuf};

/// CI streaming window, seconds (the short CI configuration; the full budget run
/// is 60 s, `DEFAULT_SECONDS`).
pub const CI_SECONDS: u64 = 10;
/// Default artifact file name written by `--ci` (in the current directory).
pub const CI_REPORT_FILE: &str = "ds-bench-ci.json";

/// The resolved CI run configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiRun {
    /// Streaming window in seconds (`CI_SECONDS` unless `--seconds` was given).
    pub seconds: u64,
    /// Where the JSON report is written (`CI_REPORT_FILE` unless `--out` was given).
    pub report_path: PathBuf,
}

impl CiRun {
    /// Resolve the preset: an explicit `--seconds` always wins over [`CI_SECONDS`],
    /// an explicit `--out` always wins over [`CI_REPORT_FILE`].
    pub fn new(seconds: Option<u64>, out: Option<&Path>) -> Self {
        Self {
            seconds: seconds.unwrap_or(CI_SECONDS),
            report_path: out
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from(CI_REPORT_FILE)),
        }
    }
}

/// Write the JSON report of a CI run to `path` (pretty-printed, trailing newline).
/// Returns the path written.
pub fn write_report(json: &serde_json::Value, path: &Path) -> Result<PathBuf, String> {
    let mut text =
        serde_json::to_string_pretty(json).map_err(|e| format!("serialize report: {e}"))?;
    text.push('\n');
    std::fs::write(path, text).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ci_preset_defaults_to_10s_and_the_default_artifact_name() {
        let c = CiRun::new(None, None);
        assert_eq!(c.seconds, 10);
        assert_eq!(c.report_path, PathBuf::from(CI_REPORT_FILE));
        assert_eq!(CI_SECONDS, 10);
    }

    #[test]
    fn explicit_seconds_and_out_win_over_the_preset() {
        let c = CiRun::new(Some(30), Some(Path::new("/tmp/x.json")));
        assert_eq!(c.seconds, 30);
        assert_eq!(c.report_path, PathBuf::from("/tmp/x.json"));
    }

    #[test]
    fn write_report_round_trips_the_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.json");
        let v = serde_json::json!({"ok": true, "p99_ms": 1.25});
        let written = write_report(&v, &path).unwrap();
        assert_eq!(written, path);
        let back: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn write_report_fails_on_an_unwritable_path() {
        let v = serde_json::json!({"ok": true});
        assert!(write_report(&v, Path::new("/nonexistent-dir-xyz/report.json")).is_err());
    }
}
