//! `ds stream` — the producer-side verbs of the frame plane
//! (`docs/contracts/cli.md` §1, `docs/contracts/ipc.md` §1.2, `docs/contracts/streaming.md`).
//!
//! - [`verb_push`] — `ds stream push <target> --file f | --dir d --fps N [--loop]`:
//!   control-socket `stream-open` (best effort; the DS-013 stream server also accepts a
//!   `DSctl stream-open` line on `stream.sock`, which is what actually opens the session
//!   on the DS-031-wired daemon), then one DSF1 binary frame per source file on
//!   `stream.sock`, honouring `stream-credit` grants and nacking back with bounded waits.
//! - [`verb_stats`] — `ds stream stats <target> [--json]`: reads
//!   `/telemetry/streams/<target>` over the control socket (the `stream-stats` op when
//!   the daemon serves it, else `get` on the telemetry path) — never bumps `rev`, never
//!   carries one (S1).
//! - [`verb_stop`] — `ds stream stop <target>`: `stream-stop` (control op, DSctl fallback
//!   on the stream socket); exit 0 when the producer is already gone.
//!
//! Targets are the unified vocabulary of S6: `keys` | `key:0..14` | `dial` — no
//! `panel`/`screen` synonyms ([`parse_target`]). Push is never a plain NDJSON op: the
//! frames ride the binary DSF1 framing on `stream.sock` (ipc.md §1.2, B1/S14).

#[cfg(test)]
mod it;
pub mod sock;
pub mod source;
#[cfg(test)]
mod tests;

use crate::cli::Global;
use crate::connect::{connect, socket_path};
use crate::output::{print_object, render_human, server_failure, Failure};
use ds_client::ClientError;
use ds_ipc::error::{ErrorCode, IpcError};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The frame-stream socket: the sibling of the control socket (`<dir>/stream.sock`,
/// `docs/contracts/ipc.md` §1.1).
pub fn stream_sock_path(g: &Global) -> PathBuf {
    socket_path(g)
        .parent()
        .map(|d| d.join("stream.sock"))
        .unwrap_or_else(|| PathBuf::from("stream.sock"))
}

/// S6 target vocabulary: `keys` | `dial` | `key:<0-14>`. `panel`/`screen` are refused
/// (they are not targets — `ds stream push keys …` means the panel).
pub fn parse_target(s: &str) -> Result<String, String> {
    let valid = s == "keys"
        || s == "dial"
        || (s.starts_with("key:")
            && s[4..]
                .parse::<u32>()
                .is_ok_and(|n| n <= 14 && s[4..] == n.to_string()));
    if valid {
        Ok(s.to_string())
    } else {
        Err(format!(
            "invalid stream target {s:?} — valid targets are keys | key:0-14 | dial \
             (S6; 'panel'/'screen' are not targets: use 'keys')"
        ))
    }
}

/// How long a push waits for a `stream-credit` re-grant / ack before giving up with
/// `E_LIMIT` (exit 5) — backpressure must never spin or hang the CLI.
pub const BACKPRESSURE_TIMEOUT: Duration = Duration::from_secs(15);

/// `ds stream push <target> --file f | --dir d --fps N [--loop] [--credits N]`.
pub fn verb_push(
    g: &Global,
    target: &str,
    file: Option<PathBuf>,
    dir: Option<PathBuf>,
    fps: Option<u32>,
    loop_: bool,
    credits: Option<u32>,
) -> Result<i32, Failure> {
    let target = parse_target(target).map_err(Failure::Usage)?;
    let plan = source::plan(file, dir, fps, loop_)?;
    let n = plan.frames.len() as u64;

    // Control socket first: daemon reachability (exit 6 when down) + token auth, and the
    // contract's control-plane `stream-open` (ipc.md §1.2). A phase-1 daemon that does not
    // serve the control op yet answers E_INTERNAL/E_SCHEMA — the DSctl stream-open on
    // stream.sock below is what opens the session there, so those two codes are not fatal.
    let mut c = connect(g)?;
    let mut open_credits = credits
        .unwrap_or_else(|| u32::try_from(n).unwrap_or(u32::MAX).min(1024))
        .clamp(1, 1024);
    match c.request(json!({"op": "stream-open", "target": target, "credits": open_credits})) {
        Ok(resp) => {
            if let Some(cr) = resp["result"]["credits"].as_u64() {
                open_credits = u32::try_from(cr).unwrap_or(open_credits).clamp(1, 1024);
            }
        }
        Err(ClientError::Server(e))
            if e.code == ErrorCode::EInternal || e.code == ErrorCode::ESchema => {}
        Err(e) => return Err(e.into()),
    }

    let stream_sock = stream_sock_path(g);
    if !stream_sock.exists() {
        return Err(unreachable_stream(&stream_sock));
    }
    let mut sc = sock::StreamConn::connect(&stream_sock)?;
    sc.open(&target, open_credits)?;

    let interval = plan.frame_interval();
    let mut sent: u64 = 0;
    for (i, fr) in plan.frames.iter().enumerate() {
        let mut meta = plan.meta(i as u64, fr);
        meta.target = target.clone();
        // schema-validate the meta client-side before it goes on the wire
        meta.to_value()
            .map_err(|e| Failure::Client(ClientError::Server(e)))?;
        let frame = ds_ipc::frames::Frame::new(meta, &fr.payload);
        sc.send_frame(&target, &frame)?;
        sent += 1;
        if i + 1 < plan.frames.len() {
            std::thread::sleep(interval);
        }
    }
    sc.close();

    let fps_value = plan.fps.map(|f| json!(f)).unwrap_or(Value::Null);
    match g.mode {
        crate::cli::OutputMode::Json => print_object(
            &json!({
                "ok": true,
                "target": target,
                "sent": sent,
                "frames": n,
                "fps": fps_value,
                "loop": plan.loop_,
            }),
            g.compact,
        ),
        _ => {
            let mut line = format!("ok target={target} sent={sent} frames={n}");
            if let Some(f) = plan.fps {
                line.push_str(&format!(" fps={f}"));
            }
            if plan.loop_ {
                line.push_str(" loop=true");
            }
            println!("{line}");
        }
    }
    Ok(0)
}

/// `ds stream stats <target> [--json]` — the `/telemetry/streams/<target>` object over
/// the control socket; telemetry reads carry no `rev` and this verb never prints one (S1).
pub fn verb_stats(g: &Global, target: &str) -> Result<i32, Failure> {
    let target = parse_target(target).map_err(Failure::Usage)?;
    let mut c = connect(g)?;
    let obj = match c.request(json!({"op": "stream-stats", "target": target})) {
        Ok(resp) => {
            let r = resp["result"].clone();
            r.get("value").cloned().unwrap_or(r)
        }
        // A daemon that does not serve the control op yet: read the telemetry plane
        // directly — same object, same no-rev semantics (streaming.md §6, S1). The
        // parent path is used because the IPC path pattern has no ':' (key:N targets).
        Err(ClientError::Server(e))
            if e.code == ErrorCode::EInternal || e.code == ErrorCode::ESchema =>
        {
            let resp = c.get("/telemetry/streams", None)?;
            let all = &resp["result"]["value"];
            match all.get(&target) {
                Some(v) => v.clone(),
                None => {
                    return Err(server_failure(
                        IpcError::new(
                            ErrorCode::EPath,
                            format!(
                                "no stream stats for target '{target}' yet — push a frame first: \
                                 ds stream push {target} --file <frame>"
                            ),
                        )
                        .with_path("/telemetry/streams"),
                    ));
                }
            }
        }
        Err(e) => return Err(e.into()),
    };
    match g.mode {
        crate::cli::OutputMode::Json => print_object(&obj, g.compact),
        _ => println!("{}", render_human(&obj)),
    }
    Ok(0)
}

/// `ds stream stop <target>` — stop the producer. Exit 0 when the producer is already
/// gone (no stream socket, or no session on it).
pub fn verb_stop(g: &Global, target: &str) -> Result<i32, Failure> {
    let target = parse_target(target).map_err(Failure::Usage)?;
    let mut c = connect(g)?; // daemon.sock missing → exit 6 E_UNREACHABLE
    let stopped = match c.request(json!({"op": "stream-stop", "target": target})) {
        Ok(_) => true,
        Err(ClientError::Server(e))
            if e.code == ErrorCode::EInternal || e.code == ErrorCode::ESchema =>
        {
            // Phase-1 daemon without the control op: DSctl stream-stop on stream.sock.
            let stream_sock = stream_sock_path(g);
            if !stream_sock.exists() {
                // No stream socket → no producer → already stopped (exit 0).
                true
            } else {
                let mut sc = sock::StreamConn::connect(&stream_sock)?;
                sc.send_dsctl(&json!({"op": "stream-stop", "target": target}))?;
                // The server answers stream-credit credits:0 (or an error line); either
                // way the session is gone. Bounded wait — never spin.
                let _ = sc.drain_until_quiet(Duration::from_secs(5));
                true
            }
        }
        Err(e) => return Err(e.into()),
    };
    match g.mode {
        crate::cli::OutputMode::Json => print_object(
            &json!({"ok": true, "target": target, "stopped": stopped}),
            g.compact,
        ),
        _ => println!("ok target={target} stopped={}", stopped),
    }
    Ok(0)
}

/// The missing-stream-socket failure: `E_UNREACHABLE` (exit 6) with the exact
/// `ds daemon start` hint (cli.md §5).
pub fn unreachable_stream(path: &Path) -> Failure {
    Failure::Client(ClientError::Unreachable {
        path: path.to_path_buf(),
        hint: "the daemon's frame stream socket is missing — start it with: ds daemon start"
            .to_string(),
        source: std::io::Error::new(std::io::ErrorKind::NotFound, "stream.sock missing"),
    })
}
