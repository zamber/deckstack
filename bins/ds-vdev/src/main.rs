//! `ds-vdev` — serve a report-level virtual MasterHub on a Unix socket.
//!
//! ```sh
//! ds-vdev --socket /tmp/vdev.sock [--modules keys,dial,m0] [--throttle-kbps N]
//!         [--drop-after N] [--inject events.ndjson]
//! ```
//!
//! Hosts connect with `virtual:/tmp/vdev.sock` (ds-device's VirtualTransport); inspection and
//! control go through the sibling control socket, driven by `ds-vdevctl`.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use ds_protocol::ModuleKind;
use ds_vdev::{parse_events_ndjson, HubConfig, Throttle, VdevServer};

const USAGE: &str = "\
ds-vdev — serve a report-level virtual MasterHub on a Unix socket

USAGE:
    ds-vdev --socket <path> [options]

OPTIONS:
    --socket <path>        Unix socket to serve (required); control socket is <path>.ctl
    --modules <list>       Comma-separated module names or 'all' (default: all):
                           base,keys,faders,knobs,rollers,dial
    --throttle-kbps <n>    Simulate the device write cap (acks delayed accordingly)
    --drop-after <n>       Simulate hot-unplug after n accepted writes
    --inject <file>        Replay input events from an ndjson file to every host
    -h, --help             Show this help
    --version              Show the version
";

struct Args {
    socket: PathBuf,
    config: HubConfig,
    inject: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut socket = None;
    let mut modules: Option<Vec<ModuleKind>> = None;
    let mut throttle = Throttle::none();
    let mut inject = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => {
                socket = Some(PathBuf::from(args.next().ok_or("--socket needs a path")?));
            }
            "--modules" => {
                let list = args.next().ok_or("--modules needs a list")?;
                modules = Some(if list == "all" {
                    ModuleKind::ALL.to_vec()
                } else {
                    list.split(',')
                        .map(|name| {
                            ModuleKind::from_name(name.trim())
                                .ok_or_else(|| format!("unknown module '{name}'"))
                        })
                        .collect::<Result<Vec<_>, _>>()?
                });
            }
            "--throttle-kbps" => {
                let n = args
                    .next()
                    .ok_or("--throttle-kbps needs a number")?
                    .parse::<u32>()
                    .map_err(|_| "--throttle-kbps needs a number".to_string())?;
                throttle.kbps = Some(n);
            }
            "--drop-after" => {
                let n = args
                    .next()
                    .ok_or("--drop-after needs a number")?
                    .parse::<u32>()
                    .map_err(|_| "--drop-after needs a number".to_string())?;
                throttle.drop_after = Some(n);
            }
            "--inject" => inject = Some(PathBuf::from(args.next().ok_or("--inject needs a file")?)),
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "--version" => {
                println!("ds-vdev {}", ds_vdev::version());
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument '{other}' (try --help)")),
        }
    }
    let socket = socket.ok_or("missing required --socket <path> (try --help)")?;
    Ok(Args {
        socket,
        config: HubConfig {
            modules: modules.unwrap_or_else(|| ModuleKind::ALL.to_vec()),
            throttle,
        },
        inject,
    })
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("ds-vdev: {e}");
            return ExitCode::from(2);
        }
    };

    let events = match &args.inject {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => match parse_events_ndjson(&text) {
                Ok(ev) => ev,
                Err(e) => {
                    eprintln!("ds-vdev: {}: {e}", path.display());
                    return ExitCode::from(2);
                }
            },
            Err(e) => {
                eprintln!("ds-vdev: {}: {e}", path.display());
                return ExitCode::from(2);
            }
        },
        None => Vec::new(),
    };

    let server = match VdevServer::start(&args.socket, args.config.clone()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ds-vdev: cannot serve {}: {e}", args.socket.display());
            return ExitCode::FAILURE;
        }
    };

    eprintln!(
        "ds-vdev {} listening: device socket {}, control socket {}",
        ds_vdev::version(),
        server.socket_path().display(),
        server.control_path().display()
    );
    for line in server.hub().reports() {
        eprintln!("  module {line}");
    }

    // --inject: replay the script to hosts, spaced by the events' at_ms timestamps
    if !events.is_empty() {
        let hub = server.hub();
        std::thread::spawn(move || {
            let mut last = 0u64;
            for ev in &events {
                let wait = ev.at_ms.saturating_sub(last);
                if wait > 0 {
                    std::thread::sleep(Duration::from_millis(wait));
                }
                last = ev.at_ms;
                if let Err(e) = hub.inject(ev) {
                    eprintln!("ds-vdev: inject: {e}");
                }
            }
        });
    }

    // serve until killed (ctrl-c)
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
