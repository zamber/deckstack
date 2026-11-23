//! `ds-bench` — the streaming bench binary (DS-033).
//!
//! ```sh
//! ds-bench [--seconds N] [--fps N] [--throttle-kbps N|off] [--target keys|key:0-14|dial]
//!          [--json] [--ci] [--out PATH]
//! ```
//!
//! Runs the streaming bench (default 32 fps x 60 s to `key:0` on a ~2.1 MB/s throttled
//! ds-vdev) and prints a compact table (or one `--json` object). Exit 0 = all budgets
//! met, 1 = a budget was violated, 2 = usage/harness error. See docs/30-testing.md §9.
//! `--ci` (DS-082) is the deterministic, bounded CI gate: 10 s window unless
//! `--seconds` is given, JSON on stdout, and the same JSON written to
//! `ds-bench-ci.json` (or `--out PATH`) for upload as a CI artifact.

use ds_bench::ci::{write_report, CiRun};
use ds_bench::stream::{cli_target, run, BenchOptions, DEFAULT_FPS, DEFAULT_SECONDS};
use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "\
ds-bench — streaming bench (DS-033): sustained fps frame push on a throttled ds-vdev
           while querying the control plane (docs/30-testing.md §5/§9)

USAGE:
    ds-bench [options]

OPTIONS:
    --seconds N          streaming window in seconds (default 60; --ci: 10 unless given)
    --fps N              producer push rate in frames/s (default 32)
    --throttle-kbps N    vdev write cap in kbit/s (default 16800 = ~2.1 MB/s); 'off' = unlimited
    --target T           stream target: keys | key:0-14 | dial (default key:0)
    --json               print one JSON object instead of the table
    --ci                 CI gate preset (DS-082): deterministic, bounded run — implies
                         --json, defaults --seconds to 10, and writes the JSON report to
                         ds-bench-ci.json (see --out); budgets still decide exit 0/1
    --out PATH           with --ci: write the JSON report to PATH instead of
                         ds-bench-ci.json (the CI artifact)
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
    /// The `--ci` preset (None without `--ci`).
    ci: Option<CiRun>,
}

fn parse_args() -> Result<Args, String> {
    let mut opts = BenchOptions {
        seconds: DEFAULT_SECONDS,
        fps: DEFAULT_FPS,
        ..BenchOptions::default()
    };
    let mut json = false;
    let mut ci = false;
    let mut seconds_given = false;
    let mut out: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seconds" => {
                opts.seconds = args
                    .next()
                    .ok_or("--seconds needs a number")?
                    .parse::<u64>()
                    .map_err(|_| "--seconds needs a number".to_string())?;
                seconds_given = true;
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
            "--ci" => ci = true,
            "--out" => out = Some(args.next().ok_or("--out needs a path")?),
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
    // The CI preset (DS-082): an explicit --seconds wins over the 10 s CI default.
    let ci_run = ci.then(|| {
        CiRun::new(
            seconds_given.then_some(opts.seconds),
            out.as_deref().map(Path::new),
        )
    });
    if ci {
        json = true;
    }
    Ok(Args {
        opts,
        json,
        ci: ci_run,
    })
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
            let mut json_report = report.to_json();
            if let Some(ci) = &args.ci {
                json_report["ci"] = serde_json::json!({
                    "seconds": ci.seconds,
                    "report_file": ci.report_path.display().to_string(),
                });
                match write_report(&json_report, &ci.report_path) {
                    Ok(path) => eprintln!("ds-bench: CI report written to {}", path.display()),
                    Err(e) => {
                        eprintln!("ds-bench: {e}");
                        return ExitCode::from(2);
                    }
                }
            }
            if args.json {
                println!("{}", serde_json::to_string_pretty(&json_report).unwrap());
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
