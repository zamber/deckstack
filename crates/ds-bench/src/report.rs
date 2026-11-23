//! The bench report: a compact table plus a `--json` object (DS-033).

use crate::budget::{Measurements, Violation};
use crate::stream::BenchOptions;
use serde_json::{json, Value};

/// One finished bench run.
#[derive(Debug, Clone)]
pub struct Report {
    /// Run configuration.
    pub seconds: u64,
    pub fps: u32,
    pub throttle_kbps: Option<u32>,
    pub target: String,
    pub query_path: String,
    /// Measurements.
    pub m: Measurements,
    /// Budget violations (empty = PASS).
    pub violations: Vec<Violation>,
}

impl Report {
    pub fn new(opts: &BenchOptions, m: &Measurements, violations: Vec<Violation>) -> Self {
        Self {
            seconds: opts.seconds,
            fps: opts.fps,
            throttle_kbps: opts.throttle_kbps,
            target: opts.target.wire_name(),
            query_path: opts.query_path.clone(),
            m: m.clone(),
            violations,
        }
    }

    /// True when every budget was met.
    pub fn ok(&self) -> bool {
        self.violations.is_empty()
    }

    /// The compact human table.
    pub fn table(&self) -> String {
        let (p50, p95, p99) = self.m.query_ms.p50_p95_p99();
        let throttle = match self.throttle_kbps {
            Some(k) => format!("{k} kbps"),
            None => "off".to_string(),
        };
        let mut s = String::new();
        s.push_str(&format!(
            "ds-bench streaming bench — target {} | {} fps x {} s | throttle {} | query {}\n",
            self.target, self.fps, self.seconds, throttle, self.query_path
        ));
        s.push_str("metric                    value                          budget                  status\n");
        let mut row = |metric: &str, value: String, budget: &str, ok: bool| {
            s.push_str(&format!(
                "{metric:<25} {value:<30} {budget:<23} {}\n",
                if ok { "ok" } else { "FAIL" }
            ));
        };
        let min_fps = self.fps as f64 * 0.95;
        row(
            "wall_s",
            format!("{:.2}", self.m.wall_s),
            &format!("{} s window", self.seconds),
            true,
        );
        row(
            "pushed (acked)",
            format!("{}", self.m.pushed),
            &format!("== {} fps x {} s", self.fps, self.seconds),
            self.m.pushed == self.fps as u64 * self.seconds,
        );
        row(
            "push_fps",
            format!("{:.2}", self.m.push_fps),
            &format!(">= {:.2}", min_fps),
            self.m.push_fps >= min_fps,
        );
        row(
            "accepted_fps (sent)",
            format!("{:.2}", self.m.accepted_fps),
            &format!(">= {:.2}", min_fps),
            self.m.accepted_fps >= min_fps,
        );
        row(
            "sent / dropped",
            format!("{} / {}", self.m.sent, self.m.dropped),
            "sent+dropped == pushed",
            self.m.sent + self.m.dropped == self.m.pushed,
        );
        row("device bytes", format!("{} B", self.m.bytes), "-", true);
        row(
            "query n",
            format!("{}", self.m.query_ms.len()),
            &format!("get {}", self.query_path),
            !self.m.query_ms.is_empty(),
        );
        row(
            "query p50 / p95 / p99 ms",
            format!("{p50:.3} / {p95:.3} / {p99:.3}"),
            "p50 <= 2.0, p99 <= 10.0",
            p50 <= 2.0 && p99 <= 10.0,
        );
        row(
            "query max ms",
            format!("{:.3}", self.m.query_ms.max()),
            "-",
            true,
        );
        row(
            "rev delta (S1)",
            format!("{}", self.m.rev_delta),
            "== 0",
            self.m.rev_delta == 0,
        );
        row(
            "/controls events (S1)",
            format!("{}", self.m.controls_events),
            "== 0",
            self.m.controls_events == 0,
        );
        row(
            "peak RSS (VmHWM)",
            format!(
                "{:.1} MiB",
                self.m.peak_rss_bytes as f64 / (1024.0 * 1024.0)
            ),
            "<= 256 MiB",
            self.m.peak_rss_bytes <= 256 * 1024 * 1024,
        );
        row(
            "last frame on device",
            if self.m.last_frame_on_device {
                "yes"
            } else {
                "no"
            }
            .to_string(),
            "yes (latest-frame-wins)",
            self.m.last_frame_on_device,
        );
        if self.ok() {
            s.push_str("RESULT: PASS (all budgets met)\n");
        } else {
            s.push_str("RESULT: FAIL (budget violations)\n");
            for v in &self.violations {
                s.push_str(&format!("  - {v}\n"));
            }
        }
        s
    }

    /// The `--json` object.
    pub fn to_json(&self) -> Value {
        let (p50, p95, p99) = self.m.query_ms.p50_p95_p99();
        json!({
            "bench": "stream-32fps-query-p99",
            "target": self.target,
            "seconds": self.seconds,
            "fps": self.fps,
            "throttle_kbps": self.throttle_kbps,
            "query_path": self.query_path,
            "wall_s": self.m.wall_s,
            "pushed": self.m.pushed,
            "sent": self.m.sent,
            "dropped": self.m.dropped,
            "bytes": self.m.bytes,
            "push_fps": self.m.push_fps,
            "accepted_fps": self.m.accepted_fps,
            "query": {
                "n": self.m.query_ms.len(),
                "p50_ms": p50,
                "p95_ms": p95,
                "p99_ms": p99,
                "max_ms": self.m.query_ms.max(),
            },
            "rev_delta": self.m.rev_delta,
            "controls_events": self.m.controls_events,
            "peak_rss_bytes": self.m.peak_rss_bytes,
            "daemon_pid": std::process::id(),
            "last_frame_on_device": self.m.last_frame_on_device,
            "budgets": {
                "query_p50_ms": 2.0,
                "query_p99_ms": 10.0,
                "sustained_fps_min": self.fps as f64 * 0.95,
                "memory_cap_bytes": 256 * 1024 * 1024,
            },
            "violations": self.violations.iter().map(|v| format!("{v}")).collect::<Vec<_>>(),
            "ok": self.ok(),
        })
    }
}
