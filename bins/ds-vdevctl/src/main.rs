//! `ds-vdevctl` — inspect and control a running ds-vdev virtual MasterHub.
//!
//! ```sh
//! ds-vdevctl --socket /tmp/vdev.sock frames [target]   # sha256 of each reassembled frame
//! ds-vdevctl --socket /tmp/vdev.sock reports           # module reports of the deck layout
//! ds-vdevctl --socket /tmp/vdev.sock writes [n] [full] # report log (kind, refused; full adds payload)
//! ds-vdevctl --socket /tmp/vdev.sock clock            # hub clock + wall clock (test time base)
//! ds-vdevctl --socket /tmp/vdev.sock inject FILE.ndjson
//! ds-vdevctl --socket /tmp/vdev.sock throttle <kbps|off> [drop-after|off]
//! ds-vdevctl --socket /tmp/vdev.sock ping
//! ```
//!
//! Targets for `frames`: `panel`, `key:N` (N < 15), `dial`.

use std::path::PathBuf;
use std::process::ExitCode;

use ds_vdev::ctl;

const USAGE: &str = "\
ds-vdevctl — inspect and control a running ds-vdev (virtual MasterHub)

USAGE:
    ds-vdevctl --socket <path> <command> [args]

COMMANDS:
    frames [target]              sha256 + bytes of each reassembled frame (target: panel|key:N|dial)
    reports                      module reports of the deck layout
    writes [n] [full]            last n accepted/refused reports (default 50; full adds the payload of short reports)
    clock                        hub clock (at_ms) + wall clock (epoch_ms)
    inject <file.ndjson>         replay scripted input events to connected hosts
    throttle <kbps|off> [drop]   set the device write cap / hot-unplug knob (drop: n|off)
    ping                         check the control socket is alive
    -h, --help                   show this help
    --version                    show the version
";

fn main() -> ExitCode {
    let mut socket: Option<PathBuf> = None;
    let mut args: Vec<String> = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--socket" => {
                socket = Some(PathBuf::from(it.next().expect("--socket needs a path")));
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "--version" => {
                println!("ds-vdevctl {}", ds_vdev::version());
                return ExitCode::SUCCESS;
            }
            _ => args.push(arg),
        }
    }
    let Some(socket) = socket else {
        eprintln!("ds-vdevctl: missing required --socket <path> (try --help)");
        return ExitCode::from(2);
    };
    let Some((cmd, rest)) = args.split_first() else {
        eprintln!("ds-vdevctl: missing command (try --help)");
        return ExitCode::from(2);
    };

    let request = match cmd.as_str() {
        "ping" => "ping".to_string(),
        "frames" => match rest.first() {
            Some(t) => format!("frames {t}"),
            None => "frames".to_string(),
        },
        "reports" => "reports".to_string(),
        "writes" => {
            if rest.is_empty() {
                "writes".to_string()
            } else {
                format!("writes {}", rest.join(" "))
            }
        }
        "clock" => "clock".to_string(),
        "throttle" => {
            if rest.is_empty() {
                eprintln!("ds-vdevctl: throttle needs <kbps|off> [drop-after|off]");
                return ExitCode::from(2);
            }
            format!("throttle {}", rest.join(" "))
        }
        "inject" => {
            let Some(path) = rest.first() else {
                eprintln!("ds-vdevctl: inject needs an ndjson file");
                return ExitCode::from(2);
            };
            let text = match std::fs::read_to_string(path) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("ds-vdevctl: {path}: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let events = match ds_vdev::parse_events_ndjson(&text) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("ds-vdevctl: {path}: {e}");
                    return ExitCode::from(2);
                }
            };
            if events.is_empty() {
                eprintln!("ds-vdevctl: {path}: no events");
                return ExitCode::FAILURE;
            }
            let mut sent = 0;
            for ev in &events {
                let line = format!(
                    "inject {}",
                    serde_json::to_string(ev).expect("event serializes")
                );
                match ctl::request(&socket, &line) {
                    Ok(lines) => {
                        for l in lines {
                            eprintln!("ds-vdevctl: {l}");
                        }
                        sent += 1;
                    }
                    Err(e) => {
                        eprintln!("ds-vdevctl: inject: {e}");
                        return ExitCode::FAILURE;
                    }
                }
            }
            println!("injected {sent} events");
            return ExitCode::SUCCESS;
        }
        other => {
            eprintln!("ds-vdevctl: unknown command '{other}' (try --help)");
            return ExitCode::from(2);
        }
    };

    match ctl::request(&socket, &request) {
        Ok(lines) => {
            for l in lines {
                println!("{l}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("ds-vdevctl: {e}");
            ExitCode::FAILURE
        }
    }
}
