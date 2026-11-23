//! `ds-bench` — the streaming bench binary (DS-033).
//!
//! ```sh
//! ds-bench [--seconds N] [--fps N] [--throttle-kbps N|off] [--target keys|key:0-14|dial] [--json]
//! ```
//!
//! Runs the streaming bench (default 32 fps x 60 s to `key:0` on a ~2.1 MB/s throttled
//! ds-vdev) and prints a compact table (or one `--json` object). Exit 0 = all budgets
//! met, 1 = a budget was violated, 2 = usage/harness error. See docs/30-testing.md §9.

use ds_bench::stream::{cli_target, run, BenchOptions, DEFAULT_FPS, DEFAULT_SECONDS};
use std::process::ExitCode;

const USAGE: &str = "\
ds-bench — streaming bench (DS-033): sustained fps frame push on a throttled ds-vdev
           while querying the control plane (docs/30-testing.md §5/§9)

USAGE:
    ds-bench [options]

OPTIONS:
    --seconds N          streaming window in seconds (default 60; CI: --seconds 5)
    --fps N              producer push rate in frames/s (default 32)
    --throttle-kbps N    vdev write cap in kbit/s (default 16800 = ~2.1 MB/s); 'off' = unlimited
    --target T           stream target: keys | key:0-14 | dial (default key:0)
    --json               print one JSON object instead of the table
    -h, --help           show this help
    --version            show the version

EXIT CODES:
    0  all budgets met (query p99 <= 10 ms, sustained fps, sent+dropped == pushed,
       rev delta 0, /controls events 0, peak RSS <= 256 MiB)
    1  a budget was violated (violations are listed)
    2  usage or harness error
";

struct Args {
    opts: BenchOptions,
    json: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut opts = BenchOptions {
        seconds: DEFAULT_SECONDS,
        fps: DEFAULT_FPS,
        ..BenchOptions::default()
    };
    let mut json = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seconds" => {
                opts.seconds = args
                    .next()
                    .ok_or("--seconds needs a number")?
                    .parse::<u64>()
                    .map_err(|_| "--seconds needs a number".to_string())?;
            }
            "--fps" => {
                opts.fps = args
                    .next()
                    .ok_or("--fps needs a number")?
                    .parse::<u32>()
                    .map_err(|_| "--fps needs a number".to_string())?;
            }
            "--throttle-kbps" => {
                let v = args.next().ok_or("--throttle-kbps needs a value")?;
                opts.throttle_kbps = if v == "off" {
                    None
                } else {
                    Some(
                        v.parse::<u32>()
                            .map_err(|_| "--throttle-kbps needs a number or 'off'")?,
                    )
                };
            }
            "--target" => {
                let v = args.next().ok_or("--target needs a value")?;
                opts.target = cli_target(&v)?;
            }
            "--json" => json = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "--version" => {
                println!("ds-bench {}", ds_bench::version());
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument '{other}' (try --help)")),
        }
    }
    if opts.fps == 0 || opts.seconds == 0 {
        return Err("--fps and --seconds must be > 0".to_string());
    }
    Ok(Args { opts, json })
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("ds-bench: {e}\n");
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(&args.opts) {
        Ok(report) => {
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report.to_json()).unwrap()
                );
            } else {
                print!("{}", report.table());
            }
            if report.ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("ds-bench: harness error: {e}");
            ExitCode::from(2)
        }
    }
}
