//! `ds` — the deckstack CLI: a stateless one-shot client of `ds-daemon`
//! (`docs/contracts/cli.md`, `docs/components/cli.md`, ADR-002, DS-016).
//!
//! Phase-1 verbs: `get`, `set`, `watch`, `schema`, `describe`, `daemon
//! start|stop|status`, `status`, plus the binding verbs `bind`/`unbind` (DS-041).
//! Global `--json`/`--ndjson`, `--socket`/`--token-file`/`--data-dir`, `--compact`,
//! `--help` at every level, `--version`.
//! Exit codes 0–8 per the contract, mapped from the ds-client error codes.
//! There is deliberately no `migrate` verb: `ds migrate …` exits 1.

mod bind;
mod cli;
mod connect;
mod help;
mod output;
mod unbind;
mod verbs;

use cli::{Command, OutputMode};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // for parse errors the output mode is not known yet — scan the raw args for --json
    let json_err = args
        .iter()
        .any(|a| a == "--json" || a.starts_with("--json="));
    match cli::parse(&args) {
        Ok((Command::Help(topic), _)) => {
            print!("{}", help::text(topic));
            ExitCode::SUCCESS
        }
        Ok((Command::Version, _)) => {
            println!("ds {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok((cmd, g)) => match verbs::run(cmd, &g) {
            Ok(code) => ExitCode::from(code as u8),
            Err(f) => {
                output::print_failure(&f, g.mode == OutputMode::Json);
                ExitCode::from(f.exit_code() as u8)
            }
        },
        Err(e) => {
            let f = output::Failure::Usage(e.0.clone());
            output::print_failure(&f, json_err);
            if e.0.starts_with("missing verb") {
                eprint!("{}", help::text("top"));
            }
            ExitCode::from(1)
        }
    }
}
