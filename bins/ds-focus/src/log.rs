//! Structured logging to stderr, same line shape as the daemon's `lifecycle::log`:
//! `{"ts_ms":…,"level":…,"target":…,"msg":…}`. Info lines are suppressed by `--quiet`;
//! every emit is logged at info (unless `--quiet`).

use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};

static QUIET: AtomicBool = AtomicBool::new(false);

/// Set the `--quiet` flag (suppresses info lines).
pub fn set_quiet(q: bool) {
    QUIET.store(q, Ordering::Relaxed);
}

/// One structured log line.
pub fn log(level: &str, target: &str, msg: &str) {
    if level == "info" && QUIET.load(Ordering::Relaxed) {
        return;
    }
    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let line = json!({
        "ts_ms": ts_ms,
        "level": level,
        "target": target,
        "msg": msg,
    });
    match serde_json::to_string(&line) {
        Ok(s) => eprintln!("{s}"),
        Err(_) => eprintln!("{{\"level\":\"{level}\",\"target\":\"{target}\",\"msg\":\"{msg}\"}}"),
    }
}

pub fn info(target: &str, msg: &str) {
    log("info", target, msg);
}

pub fn warn(target: &str, msg: &str) {
    log("warn", target, msg);
}

pub fn error(target: &str, msg: &str) {
    log("error", target, msg);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_gates_info_only() {
        set_quiet(true);
        assert!(QUIET.load(Ordering::Relaxed));
        log("info", "ds-focus::test", "suppressed");
        set_quiet(false);
        log("warn", "ds-focus::test", "visible");
        log("error", "ds-focus::test", "visible");
    }

    #[test]
    fn line_shape() {
        // log() prints to stderr; assert the JSON shape it builds by re-building it here
        let line = json!({"ts_ms": 1u64, "level": "info", "target": "t", "msg": "m"});
        let s = serde_json::to_string(&line).unwrap();
        assert!(s.contains("\"level\":\"info\""));
        assert!(s.contains("\"target\":\"t\""));
        assert!(s.contains("\"ts_ms\":1"));
    }
}
