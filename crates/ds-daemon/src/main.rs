//! The `ds-daemon` binary: a thin shell over the library — subcommands `run` (default),
//! `--guard` (stub, DS-024) and `--version` (docs/components/daemon.md).

use ds_daemon::daemon::{self, Cli};
use ds_daemon::lifecycle;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = real_main(&args);
    std::process::exit(code);
}

fn real_main(args: &[String]) -> i32 {
    match daemon::parse_args(args) {
        Ok(Cli::Version) => {
            println!("{} {}", ds_daemon::CRATE_NAME, ds_daemon::version());
            0
        }
        Ok(Cli::Help) => {
            print!("{}", daemon::usage());
            0
        }
        Ok(Cli::Guard) => {
            // Guard/handback (supervise, restart, hand the device back) is DS-024.
            lifecycle::log(
                "info",
                "ds-daemon",
                "--guard: guard/handback lands with DS-024; nothing to do in phase 1",
            );
            0
        }
        Ok(Cli::Run(opts)) => match daemon::run(opts) {
            Ok(code) => code,
            Err(e) => {
                lifecycle::log("error", "ds-daemon", &format!("fatal: {e}"));
                8 // E_INTERNAL (docs/contracts/ipc.md §4)
            }
        },
        Err(e) => {
            eprintln!("ds-daemon: {e}");
            eprintln!();
            eprint!("{}", daemon::usage());
            1 // usage error
        }
    }
}
