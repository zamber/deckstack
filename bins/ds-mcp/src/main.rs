//! `ds-mcp` — the deckstack MCP bridge: the daemon control API as MCP tools over
//! stdio (`docs/components/mcp-bridge.md`, DS-061). JSON-RPC 2.0, one message per
//! line; stdout carries protocol messages only (diagnostics go to stderr).
//!
//! ```text
//! ds-mcp [--socket PATH] [--data-dir PATH] [--token-file PATH] [-h|--help] [--version]
//! ```
//!
//! `--socket` wins over `--data-dir/daemon.sock`; both default to the ds-ipc default
//! (`$XDG_RUNTIME_DIR/deckstack/daemon.sock`). The token defaults to `<socket
//! dir>/token`. Handshake: `initialize` → `notifications/initialized` →
//! `tools/list` / `tools/call`.

mod bridge;
mod conn;
mod rpc;
mod stream;
mod tools;
mod watch;

#[cfg(test)]
mod it;

use bridge::{Bridge, In};
use conn::Config;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
ds-mcp — the deckstack MCP bridge (MCP over stdio, JSON-RPC 2.0; docs/components/mcp-bridge.md)

USAGE:
    ds-mcp [--socket PATH] [--data-dir PATH] [--token-file PATH]

OPTIONS:
    --socket PATH       control socket of the daemon (default: --data-dir/daemon.sock,
                        else $XDG_RUNTIME_DIR/deckstack/daemon.sock)
    --data-dir PATH     daemon data dir; the socket is <dir>/daemon.sock unless --socket
    --token-file PATH   auth token file (default: <socket dir>/token, 0600)
    -h, --help          show this help
    --version           show the version

PROTOCOL:
    One JSON-RPC 2.0 message per line on stdin/stdout. Methods: initialize, ping,
    tools/list, tools/call; notifications: initialized, cancelled. Tools: deck_status,
    deck_get, deck_set, deck_bind, deck_unbind, deck_emit, deck_watch (streams
    notifications/deck/event), deck_stream_push, deck_plugin_list, deck_schema.
    Daemon down -> tool error E_UNREACHABLE with the 'ds daemon start' hint.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&args) {
        Ok(Parse::Help) => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Ok(Parse::Version) => {
            println!("ds-mcp {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Parse::Run {
            socket,
            data_dir,
            token_file,
        }) => {
            let socket = Config::resolve_socket(socket, data_dir);
            let cfg = Config::new(socket, token_file);
            run(cfg)
        }
        Err(msg) => {
            eprintln!("ds-mcp: {msg}  [exit 1]");
            eprintln!("try: ds-mcp --help");
            ExitCode::from(1)
        }
    }
}

enum Parse {
    Help,
    Version,
    Run {
        socket: Option<PathBuf>,
        data_dir: Option<PathBuf>,
        token_file: Option<PathBuf>,
    },
}

fn parse_args(args: &[String]) -> Result<Parse, String> {
    let mut socket = None;
    let mut data_dir = None;
    let mut token_file = None;
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        let value = |i: &mut usize| -> Result<String, String> {
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag {
            "-h" | "--help" => return Ok(Parse::Help),
            "--version" => return Ok(Parse::Version),
            "--socket" => socket = Some(PathBuf::from(value(&mut i)?)),
            "--data-dir" => data_dir = Some(PathBuf::from(value(&mut i)?)),
            "--token-file" => token_file = Some(PathBuf::from(value(&mut i)?)),
            other => return Err(format!("unknown argument {other:?}")),
        }
        i += 1;
    }
    Ok(Parse::Run {
        socket,
        data_dir,
        token_file,
    })
}

/// The stdio serve loop: a reader thread feeds lines into one channel together with
/// the watch events, so notifications are written in arrival order and stdout stays
/// protocol-only.
fn run(cfg: Config) -> ExitCode {
    let (tx, rx) = std::sync::mpsc::channel::<In>();
    let stdin_tx = tx.clone();
    std::thread::Builder::new()
        .name("ds-mcp-stdin".to_string())
        .spawn(move || {
            let stdin = std::io::stdin();
            let mut reader = BufReader::new(stdin.lock());
            let mut line = String::new();
            while let Ok(n) = reader.read_line(&mut line) {
                if n == 0 {
                    break; // EOF
                }
                if !line.trim().is_empty() && stdin_tx.send(In::Line(line.clone())).is_err() {
                    return;
                }
                line.clear();
            }
            let _ = stdin_tx.send(In::Eof);
        })
        .expect("cannot spawn the stdin reader thread");

    let mut bridge = Bridge::new(cfg, tx);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    while let Ok(msg) = rx.recv() {
        if matches!(msg, In::Eof) {
            break;
        }
        for m in bridge.handle_in(msg) {
            write_msg(&mut out, &m);
        }
    }
    let _ = out.flush();
    ExitCode::SUCCESS
}

/// One protocol message = one line on stdout (never any logging there).
fn write_msg(out: &mut impl Write, msg: &serde_json::Value) {
    if let Ok(line) = serde_json::to_string(msg) {
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_ok() {
        match parse_args(&[
            "--socket".into(),
            "/tmp/s.sock".into(),
            "--data-dir".into(),
            "/tmp/d".into(),
        ])
        .unwrap()
        {
            Parse::Run {
                socket,
                data_dir,
                token_file,
            } => {
                assert_eq!(socket, Some(PathBuf::from("/tmp/s.sock")));
                assert_eq!(data_dir, Some(PathBuf::from("/tmp/d")));
                assert_eq!(token_file, None);
            }
            _ => panic!(),
        }
        assert!(matches!(
            parse_args(&["--help".into()]).unwrap(),
            Parse::Help
        ));
        assert!(matches!(
            parse_args(&["--version".into()]).unwrap(),
            Parse::Version
        ));
        assert!(parse_args(&["--socket".into()]).is_err());
        assert!(parse_args(&["--bogus".into()]).is_err());
        assert!(matches!(parse_args(&[]).unwrap(), Parse::Run { .. }));
    }

    #[test]
    fn write_msg_emits_one_line() {
        let mut buf: Vec<u8> = Vec::new();
        write_msg(
            &mut buf,
            &serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {}}),
        );
        let s = String::from_utf8(buf).unwrap();
        assert!(s.ends_with('\n'), "{s:?}");
        assert_eq!(s.matches('\n').count(), 1, "one message per line: {s:?}");
        let parsed: serde_json::Value = serde_json::from_str(s.trim()).unwrap();
        assert_eq!(parsed["id"], serde_json::json!(1));
        assert_eq!(parsed["jsonrpc"], serde_json::json!("2.0"));
    }
}
