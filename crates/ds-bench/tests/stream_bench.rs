//! Short end-to-end integration run of the streaming bench (DS-033): 2 s at 32 fps
//! (64 frames) against a throttled ds-vdev with the in-process daemon stack, so
//! `cargo test -p ds-bench` proves the whole path — DSF1 push, credits/acks, writer
//! threads, telemetry, control-plane queries, RSS read, budget evaluation.

use ds_bench::stream::{run, BenchOptions, DEFAULT_FPS};

#[test]
fn streaming_bench_2s_end_to_end_within_budgets() {
    let opts = BenchOptions {
        seconds: 2,
        ..BenchOptions::default()
    };
    let report = run(&opts).expect("bench run must not error");
    assert!(
        report.ok(),
        "budget violations: {:?}\n{}",
        report.violations,
        report.table()
    );
    let m = &report.m;
    assert_eq!(
        m.pushed,
        DEFAULT_FPS as u64 * 2,
        "every paced frame was acked"
    );
    assert_eq!(
        m.sent + m.dropped,
        m.pushed,
        "sent + dropped == pushed (drops counted)"
    );
    assert!(
        m.sent > 0,
        "the throttled vdev must accept frames at 32 fps: {}",
        report.table()
    );
    assert!(
        m.query_ms.len() >= 10,
        "query loop sampled during the window"
    );
    assert_eq!(m.rev_delta, 0, "S1: no rev churn from frame pushes");
    assert_eq!(m.controls_events, 0, "S1: no /controls events");
    assert!(
        m.last_frame_on_device,
        "latest pushed frame is the device's final frame"
    );
    assert!(
        m.peak_rss_bytes <= 256 * 1024 * 1024,
        "256 MiB cap respected"
    );
    let (p50, _p95, p99) = m.query_ms.p50_p95_p99();
    assert!(p99 <= 10.0, "p99 {p99:.3} ms <= 10 ms (p50 {p50:.3} ms)");
}
