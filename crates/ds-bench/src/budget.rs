//! Budget evaluation for the streaming bench (DS-033).
//!
//! Budgets come from `docs/30-testing.md` §5 and `docs/contracts/streaming.md`:
//! query-under-load p50 ≤ 2 ms / p99 ≤ 10 ms while frames stream at the target rate;
//! the push and the device-accepted rate sustain ≥ the target fps; every accepted
//! frame is later sent or dropped+counted (`sent + dropped == pushed`); frames never
//! churn state (S1: `rev` delta == 0, zero `/controls` subscription events); peak RSS
//! of the daemon process stays under the 256 MiB cap.

use crate::stats::Series;

/// The frame-queue / daemon memory cap (`docs/contracts/streaming.md` §5): 256 MiB.
pub const MEMORY_CAP_BYTES: u64 = 256 * 1024 * 1024;

/// Budgets for one bench run. The fps-dependent rates are checked against `fps`
/// scaled by `sustained_fraction` (timing jitter headroom, not a relaxed budget —
/// the producer paces at exactly `fps`).
#[derive(Debug, Clone)]
pub struct Budgets {
    /// Query p50 budget, ms (docs/30-testing.md §5 query-under-load).
    pub query_p50_ms: f64,
    /// Query p99 budget, ms (the phase-3 exit budget).
    pub query_p99_ms: f64,
    /// Fraction of the target fps that push and accepted rates must sustain.
    pub sustained_fraction: f64,
    /// Peak RSS cap of the daemon process, bytes.
    pub memory_cap_bytes: u64,
}

impl Default for Budgets {
    fn default() -> Self {
        Self {
            query_p50_ms: 2.0,
            query_p99_ms: 10.0,
            sustained_fraction: 0.95,
            memory_cap_bytes: MEMORY_CAP_BYTES,
        }
    }
}

/// Everything the bench measured in one run.
#[derive(Debug, Clone)]
pub struct Measurements {
    /// Wall-clock seconds of the streaming window.
    pub wall_s: f64,
    /// Frames the producer pushed (and got `stream-ack` for).
    pub pushed: u64,
    /// Frames the device writer fully sent (telemetry `sent`).
    pub sent: u64,
    /// Frames dropped and counted (telemetry `dropped`).
    pub dropped: u64,
    /// Device-JPEG bytes sent.
    pub bytes: u64,
    /// Producer push rate, frames/s over the window.
    pub push_fps: f64,
    /// Device-accepted (sent) rate, frames/s over the window.
    pub accepted_fps: f64,
    /// Query latencies, ms (one per `get` during the window).
    pub query_ms: Series,
    /// `rev` delta across the streaming window (S1: must be 0).
    pub rev_delta: u64,
    /// `/controls` subscription events delivered during the window (S1: must be 0).
    pub controls_events: u64,
    /// Peak RSS (VmHWM) of the daemon process, bytes.
    pub peak_rss_bytes: u64,
    /// The last pushed frame landed on the device (sha256 match), latest-frame-wins.
    pub last_frame_on_device: bool,
}

/// One budget violation: the metric name and a human-readable detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub metric: String,
    pub detail: String,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.metric, self.detail)
    }
}

/// Evaluate the measurements against the budgets. Empty output = all budgets met.
pub fn evaluate(m: &Measurements, b: &Budgets, fps: u32) -> Vec<Violation> {
    let mut v = Vec::new();
    let (p50, _p95, p99) = m.query_ms.p50_p95_p99();
    if p50 > b.query_p50_ms {
        v.push(Violation {
            metric: "query p50".into(),
            detail: format!("{p50:.3} ms > {:.1} ms budget", b.query_p50_ms),
        });
    }
    if p99 > b.query_p99_ms {
        v.push(Violation {
            metric: "query p99".into(),
            detail: format!("{p99:.3} ms > {:.1} ms budget", b.query_p99_ms),
        });
    }
    let min_fps = fps as f64 * b.sustained_fraction;
    if m.push_fps < min_fps {
        v.push(Violation {
            metric: "push rate".into(),
            detail: format!("{:.2} fps < {:.2} fps (target {fps})", m.push_fps, min_fps),
        });
    }
    if m.accepted_fps < min_fps {
        v.push(Violation {
            metric: "accepted rate".into(),
            detail: format!(
                "{:.2} fps < {:.2} fps (target {fps}); sent={} dropped={}",
                m.accepted_fps, min_fps, m.sent, m.dropped
            ),
        });
    }
    if m.sent + m.dropped != m.pushed {
        v.push(Violation {
            metric: "frame conservation".into(),
            detail: format!(
                "sent {} + dropped {} != pushed {}",
                m.sent, m.dropped, m.pushed
            ),
        });
    }
    if m.rev_delta != 0 {
        v.push(Violation {
            metric: "rev delta (S1)".into(),
            detail: format!("rev moved by {} during frame streaming", m.rev_delta),
        });
    }
    if m.controls_events != 0 {
        v.push(Violation {
            metric: "/controls events (S1)".into(),
            detail: format!(
                "{} subscription events during frame streaming",
                m.controls_events
            ),
        });
    }
    if m.peak_rss_bytes > b.memory_cap_bytes {
        v.push(Violation {
            metric: "peak RSS".into(),
            detail: format!(
                "{} MiB > {} MiB cap",
                m.peak_rss_bytes / (1024 * 1024),
                b.memory_cap_bytes / (1024 * 1024)
            ),
        });
    }
    if !m.last_frame_on_device {
        v.push(Violation {
            metric: "last frame on device".into(),
            detail: "the device's final reassembled frame is not the last pushed frame".into(),
        });
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passing_measurements() -> Measurements {
        let mut query_ms = Series::new();
        for _ in 0..200 {
            query_ms.push(0.4);
        }
        // one 8 ms sample: p99 of 200 samples stays under the 10 ms budget
        query_ms.push(8.0);
        Measurements {
            wall_s: 60.0,
            pushed: 1920,
            sent: 1920,
            dropped: 0,
            bytes: 1920 * 1500,
            push_fps: 32.0,
            accepted_fps: 32.0,
            query_ms,
            rev_delta: 0,
            controls_events: 0,
            peak_rss_bytes: 120 * 1024 * 1024,
            last_frame_on_device: true,
        }
    }

    #[test]
    fn budgets_default_to_the_contract_values() {
        let b = Budgets::default();
        assert_eq!(b.query_p50_ms, 2.0);
        assert_eq!(b.query_p99_ms, 10.0);
        assert_eq!(b.memory_cap_bytes, 256 * 1024 * 1024);
        assert!(b.sustained_fraction > 0.0 && b.sustained_fraction <= 1.0);
    }

    #[test]
    fn a_synthetic_passing_run_has_no_violations() {
        let m = passing_measurements();
        let v = evaluate(&m, &Budgets::default(), 32);
        assert!(v.is_empty(), "unexpected violations: {v:?}");
    }

    #[test]
    fn each_budget_violation_is_detected_on_a_synthetic_series() {
        let b = Budgets::default();
        // p99 over budget
        let mut m = passing_measurements();
        for _ in 0..10 {
            m.query_ms.push(50.0); // 10/210 samples at 50 ms -> p99 = 50 ms
        }
        let v = evaluate(&m, &b, 32);
        assert!(v.iter().any(|x| x.metric == "query p99"), "{v:?}");
        // p50 over budget: a synthetic series dominated by 5 ms samples
        let mut m = passing_measurements();
        m.query_ms = Series::new();
        for _ in 0..300 {
            m.query_ms.push(5.0);
        }
        let v = evaluate(&m, &b, 32);
        assert!(v.iter().any(|x| x.metric == "query p50"), "{v:?}");
        // push + accepted rate under the sustained fraction
        let mut m = passing_measurements();
        m.push_fps = 20.0;
        m.accepted_fps = 20.0;
        let v = evaluate(&m, &b, 32);
        assert!(v.iter().any(|x| x.metric == "push rate"), "{v:?}");
        assert!(v.iter().any(|x| x.metric == "accepted rate"), "{v:?}");
        // conservation: a frame neither sent nor dropped
        let mut m = passing_measurements();
        m.sent = 1919;
        let v = evaluate(&m, &b, 32);
        assert!(v.iter().any(|x| x.metric == "frame conservation"), "{v:?}");
        // S1: rev churn and a /controls event
        let mut m = passing_measurements();
        m.rev_delta = 3;
        m.controls_events = 2;
        let v = evaluate(&m, &b, 32);
        assert!(v.iter().any(|x| x.metric == "rev delta (S1)"), "{v:?}");
        assert!(
            v.iter().any(|x| x.metric == "/controls events (S1)"),
            "{v:?}"
        );
        // memory over the 256 MiB cap
        let mut m = passing_measurements();
        m.peak_rss_bytes = 300 * 1024 * 1024;
        let v = evaluate(&m, &b, 32);
        assert!(v.iter().any(|x| x.metric == "peak RSS"), "{v:?}");
        // last frame never landed
        let mut m = passing_measurements();
        m.last_frame_on_device = false;
        let v = evaluate(&m, &b, 32);
        assert!(
            v.iter().any(|x| x.metric == "last frame on device"),
            "{v:?}"
        );
    }

    #[test]
    fn drops_are_not_a_violation_when_conserved() {
        // a throttled run that drops frames is fine as long as sent+dropped == pushed
        let mut m = passing_measurements();
        m.sent = 100;
        m.dropped = 1820;
        m.accepted_fps = 100.0 / 60.0;
        let v = evaluate(&m, &b_with(1.0), 32);
        // accepted rate is under budget at fps=32 -> violation expected, but NOT conservation
        assert!(v.iter().any(|x| x.metric == "accepted rate"), "{v:?}");
        assert!(!v.iter().any(|x| x.metric == "frame conservation"), "{v:?}");
    }

    fn b_with(sustained_fraction: f64) -> Budgets {
        Budgets {
            sustained_fraction,
            ..Budgets::default()
        }
    }
}
