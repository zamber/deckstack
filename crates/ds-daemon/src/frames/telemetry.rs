//! The frame-service telemetry sampler (DS-031): writes non-durable
//! `/telemetry/streams/<target>` samples into the state store at **≤ 1 Hz**
//! (`docs/contracts/streaming.md` §4/§6, `docs/components/frame-service.md`).
//!
//! S1: a sample goes through `Store::set_telemetry` — never persisted, never bumps `rev`,
//! and `/telemetry/*` reads/events carry no `rev`. Frame bytes never enter the JSON state
//! document; only counters and the last frame's sha256/seq do. The sampler holds no lock
//! across the store write beyond the store's own (the slots lock is released first), and
//! the store write never touches the device.
//!
//! The per-target object is the `stream-stats` shape of streaming.md §6 (and
//! `schemas/state.json` `telemetry.streams`):
//! `{"sent", "dropped", "fps", "bytes", "queue", "in_flight", "last_sha256"?,
//! "last_seq"?, "writer_ms_p99", "sampled_at"}` — `queue` is the waiting depth (0/1),
//! `in_flight` whether a frame is currently being sent whole.

use super::service::{ServiceInner, Slot};
use crate::lifecycle;
use serde_json::{json, Value};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Sampling period: exactly 1 Hz (the contract cap is ≤ 1 Hz, S1).
pub const SAMPLE_PERIOD: Duration = Duration::from_secs(1);

/// Spawn the sampler thread: sample every `SAMPLE_PERIOD`, and once more at shutdown.
pub fn spawn_sampler(inner: std::sync::Arc<ServiceInner>) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("ds-frames-telemetry".into())
        .spawn(move || loop {
            let deadline = Instant::now() + SAMPLE_PERIOD;
            while Instant::now() < deadline {
                if inner.shutdown.get() {
                    sample_once(&inner);
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            sample_once(&inner);
        })
        .expect("spawn frame telemetry sampler thread")
}

/// Write one sample of every active target into `/telemetry/streams/<target>`
/// (non-durable, no rev — `Store::set_telemetry`).
pub fn sample_once(inner: &ServiceInner) {
    let now = Instant::now();
    let sampled_at = lifecycle::rfc3339_utc(lifecycle::now_secs());
    let mut updates: Vec<(super::FrameTarget, Value)> = Vec::new();
    {
        let mut slots = inner.slots.lock().unwrap();
        for (target, slot) in slots.iter_mut() {
            let active = slot.stats.sent > 0
                || slot.stats.dropped > 0
                || slot.waiting.is_some()
                || slot.in_flight.is_some();
            if !active {
                continue; // do not create the path for a target that never streamed
            }
            let value = stats_value(&target.wire_name(), slot, now, &sampled_at);
            // reset the fps window
            if now.duration_since(slot.stats.window_start).as_secs_f64() > 0.0 {
                slot.stats.window_sent = slot.stats.sent;
                slot.stats.window_start = now;
            }
            updates.push((*target, value));
        }
    }
    for (target, value) in updates {
        let path = super::service::streams_path(target);
        if let Err(e) = inner.store.set_telemetry(&path, value) {
            lifecycle::log(
                "warn",
                "ds-daemon::frames::telemetry",
                &format!("telemetry sample for {path} failed: {e}"),
            );
        }
    }
}

/// The `/telemetry/streams/<target>` object for one slot (also used by the service's
/// `stats` view — same shape, no rev).
pub fn stats_value(name: &str, slot: &Slot, now: Instant, sampled_at: &str) -> Value {
    let stats = &slot.stats;
    let elapsed = now
        .duration_since(stats.window_start)
        .as_secs_f64()
        .max(f64::EPSILON);
    let fps = (stats.sent.saturating_sub(stats.window_sent)) as f64 / elapsed;
    let p99 = percentile_ms(&stats.writer_ms, 0.99);
    let mut v = json!({
        "sent": stats.sent,
        "dropped": stats.dropped,
        "fps": (fps * 1000.0).round() / 1000.0,
        "bytes": stats.bytes,
        "queue": u64::from(slot.waiting.is_some()),
        "in_flight": slot.in_flight.is_some(),
        "writer_ms_p99": p99,
        "sampled_at": sampled_at,
    });
    if let Some(sha) = &stats.last_sha256 {
        v["last_sha256"] = json!(sha);
    }
    if let Some(seq) = stats.last_seq {
        v["last_seq"] = json!(seq);
    }
    let _ = name; // the name is the path key chosen by the caller
    v
}

/// The p-th percentile (0..1) of the writer send durations, in milliseconds.
fn percentile_ms(samples: &std::collections::VecDeque<Duration>, p: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut ms: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = ms.len();
    let idx = ((p * n as f64).ceil() as usize).clamp(1, n) - 1;
    (ms[idx] * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frames::service::{FrameService, Slot};
    use crate::frames::FrameTarget;
    use crate::lifecycle::Shutdown;
    use crate::router::Bus;
    use ds_state::Store;
    use std::sync::Arc;

    #[test]
    fn percentile_is_monotonic_and_handles_empty() {
        let mut d = std::collections::VecDeque::new();
        assert_eq!(percentile_ms(&d, 0.99), 0.0);
        for ms in [1.0, 2.0, 3.0, 4.0, 100.0] {
            d.push_back(Duration::from_secs_f64(ms / 1000.0));
        }
        assert_eq!(percentile_ms(&d, 0.99), 100.0);
        assert_eq!(percentile_ms(&d, 0.5), 3.0);
        assert_eq!(percentile_ms(&d, 0.0), 1.0);
    }

    #[test]
    fn stats_value_matches_the_contract_shape() {
        let store = Arc::new(Store::fresh());
        let shutdown = Shutdown::manual();
        let svc = FrameService::spawn(store, None, Bus::new(), shutdown.clone());
        let slot = Slot::new();
        let now = Instant::now();
        let v = stats_value("key:0", &slot, now, "2026-11-22T00:00:00Z");
        for key in [
            "sent",
            "dropped",
            "fps",
            "bytes",
            "queue",
            "in_flight",
            "writer_ms_p99",
            "sampled_at",
        ] {
            assert!(v.get(key).is_some(), "missing {key}: {v}");
        }
        assert_eq!(v["sent"], json!(0));
        assert_eq!(v["queue"], json!(0));
        assert_eq!(v["in_flight"], json!(false));
        assert!(v.get("last_sha256").is_none(), "no last frame yet: {v}");
        assert!(v.get("last_seq").is_none());
        // an unknown target has no stats
        assert!(svc.stats(FrameTarget::Key(5)).is_none());
        shutdown.trigger();
        svc.join();
    }

    #[test]
    fn sampler_writes_only_active_targets_and_never_bumps_rev() {
        let store = Arc::new(Store::fresh());
        let shutdown = Shutdown::manual();
        let svc = FrameService::spawn(store.clone(), None, Bus::new(), shutdown.clone());
        // no activity: no /telemetry/streams path is created
        svc.sample_now();
        assert!(store.get("/telemetry/streams/keys").is_err());
        // push one frame with no transport: it is taken and dropped (counted) -> active
        svc.push_frame(FrameTarget::Panel, vec![0u8; 16], 0, true)
            .unwrap();
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if svc.queue_state(FrameTarget::Panel).3 >= 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        svc.sample_now();
        let reply = store.get("/telemetry/streams/keys").unwrap();
        assert_eq!(reply.rev, None);
        assert_eq!(reply.value["dropped"], json!(1));
        assert_eq!(reply.value["sent"], json!(0));
        assert_eq!(
            store.rev(),
            0,
            "S1: sampling and frame drops never bump rev"
        );
        shutdown.trigger();
        svc.join();
    }
}
