//! `cargo bench -p ds-bench` — the full streaming bench (32 fps x 60 s) as a plain
//! main (harness = false, stable Rust). `cargo test -p ds-bench` does not run this
//! target; it runs the short (2 s) integration test in `tests/stream_bench.rs`.

use ds_bench::stream::{run, BenchOptions};

fn main() {
    let opts = BenchOptions::default();
    match run(&opts) {
        Ok(report) => {
            print!("{}", report.table());
            println!(
                "{}",
                serde_json::to_string_pretty(&report.to_json()).unwrap()
            );
            if report.ok() {
                std::process::exit(0);
            }
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("ds-bench: harness error: {e}");
            std::process::exit(2);
        }
    }
}
