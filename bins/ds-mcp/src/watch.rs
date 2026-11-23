//! `deck_watch`: one dedicated control connection per subscription. The handshake and
//! the `subscribe` op are the same ds-client ops (`hello`, `auth` from the token file,
//! `subscribe`); the framing uses the `ds-ipc` primitives directly so the subscription
//! can be polled with a read timeout and cancelled client-side
//! (`notifications/cancelled` → `unsubscribe` → close). Events are forwarded to the
//! main loop, which emits them as MCP notifications (NDJSON, one per line).

use crate::bridge::In;
use crate::conn::{read_token, Config, CLIENT_NAME};
use ds_client::ClientError;
use ds_ipc::error::{ErrorCode, IpcError};
use ds_ipc::framing::{self, FrameError};
use ds_ipc::limits::MAX_LINE_BYTES;
use serde_json::{json, Value};
use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

/// How long one poll of the subscription socket blocks before the stop flag is checked.
const POLL_TIMEOUT: Duration = Duration::from_millis(200);

/// A running subscription: its id and the flag that cancels it.
pub struct WatchHandle {
    pub sub: String,
    pub stop: Arc<AtomicBool>,
}

/// Connect, handshake, `subscribe`, then spawn the reader thread. Errors of the setup
/// itself (daemon down, bad token, refused subscription) are returned to the caller so
/// the `tools/call` can answer with the mapped tool error.
pub fn start_watch(
    cfg: &Config,
    tx: Sender<In>,
    sub: String,
    path: String,
    from_rev: Option<u64>,
    max_events: Option<u64>,
) -> Result<WatchHandle, ClientError> {
    let mut writer =
        UnixStream::connect(&cfg.socket).map_err(|source| ClientError::Unreachable {
            path: cfg.socket.clone(),
            hint: "start it with: ds daemon start".to_string(),
            source,
        })?;
    writer
        .set_read_timeout(Some(POLL_TIMEOUT))
        .map_err(|e| ClientError::Protocol(e.to_string()))?;
    let mut reader = BufReader::new(
        writer
            .try_clone()
            .map_err(|e| ClientError::Protocol(e.to_string()))?,
    );
    let mut next_id: i64 = 0;
    request(
        &mut reader,
        &mut writer,
        &mut next_id,
        json!({"op": "hello", "protocol": ds_ipc::PROTOCOL_VERSION, "client": CLIENT_NAME}),
    )?;
    let token = read_token(cfg)?;
    request(
        &mut reader,
        &mut writer,
        &mut next_id,
        json!({"op": "auth", "token": token}),
    )?;
    let mut sub_req = json!({"op": "subscribe", "path": path});
    if let Some(fr) = from_rev {
        sub_req["from_rev"] = json!(fr);
    }
    request(&mut reader, &mut writer, &mut next_id, sub_req)?;

    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let tx_thread = tx;
    let sub_thread = sub.clone();
    std::thread::Builder::new()
        .name(format!("ds-mcp-watch-{sub_thread}"))
        .spawn(move || {
            watch_loop(
                reader,
                writer,
                tx_thread,
                sub_thread,
                path,
                stop_thread,
                max_events,
            );
        })
        .map_err(|e| ClientError::Protocol(format!("cannot spawn the watch thread: {e}")))?;
    Ok(WatchHandle { sub, stop })
}

/// One request/response round-trip on a raw control connection (same rules as
/// `ds_client::Client::request`: schema-validated, line-capped, correlation-checked).
fn request(
    reader: &mut BufReader<UnixStream>,
    writer: &mut UnixStream,
    next_id: &mut i64,
    mut req: Value,
) -> Result<Value, ClientError> {
    let id = *next_id;
    *next_id += 1;
    req["id"] = json!(id);
    ds_ipc::message::validate_request(&req).map_err(|e| ClientError::Schema(e.message))?;
    let mut line = serde_json::to_vec(&req).map_err(|e| ClientError::Protocol(e.to_string()))?;
    if line.len() + 1 > MAX_LINE_BYTES {
        return Err(ClientError::Limit(format!(
            "request line would be {} bytes (cap {MAX_LINE_BYTES})",
            line.len() + 1
        )));
    }
    line.push(b'\n');
    writer
        .write_all(&line)
        .and_then(|_| writer.flush())
        .map_err(|e| ClientError::Protocol(format!("write: {e}")))?;
    loop {
        let line = framing::read_line(reader, MAX_LINE_BYTES).map_err(|e| match e {
            FrameError::Eof => ClientError::Protocol("connection closed by daemon".to_string()),
            other => ClientError::Protocol(other.to_ipc_error().message),
        })?;
        let msg: Value = serde_json::from_str(line.trim())
            .map_err(|e| ClientError::Protocol(format!("bad response line: {e}")))?;
        ds_ipc::validate_message(&msg).map_err(|e| ClientError::Protocol(e.message))?;
        if msg.get("ok").is_some() {
            if msg.get("id").and_then(Value::as_i64) != Some(id) {
                return Err(ClientError::Protocol(format!(
                    "response id {:?} does not match request id {id}",
                    msg.get("id")
                )));
            }
            if msg["ok"] == Value::Bool(false) {
                let err = msg
                    .get("error")
                    .and_then(IpcError::from_value)
                    .unwrap_or_else(|| {
                        IpcError::new(ErrorCode::EInternal, "error without envelope")
                    });
                return Err(ClientError::Server(err));
            }
            return Ok(msg);
        }
        // async event during the handshake: no subscription exists yet, drop it
    }
}

/// The subscription reader loop: forward every event as `In::Event`, stop on the cancel
/// flag (`unsubscribe` first, best effort), on `max_events`, or when the daemon closes.
fn watch_loop(
    mut reader: BufReader<UnixStream>,
    mut writer: UnixStream,
    tx: Sender<In>,
    sub: String,
    path: String,
    stop: Arc<AtomicBool>,
    max_events: Option<u64>,
) {
    let mut delivered: u64 = 0;
    let mut reason = "daemon closed the connection".to_string();
    loop {
        if stop.load(Ordering::SeqCst) {
            reason = "cancelled by the client".to_string();
            // best-effort unsubscribe so the daemon drops the subscriber immediately
            let req =
                json!({"id": 900_000_i64 + delivered as i64, "op": "unsubscribe", "path": path});
            if ds_ipc::message::validate_request(&req).is_ok() {
                if let Ok(mut line) = serde_json::to_vec(&req) {
                    line.push(b'\n');
                    let _ = writer.write_all(&line).and_then(|_| writer.flush());
                }
            }
            break;
        }
        let msg = match framing::read_line(&mut reader, MAX_LINE_BYTES) {
            Ok(line) => line,
            Err(FrameError::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(FrameError::Eof) => break,
            Err(other) => {
                reason = format!("stream error: {}", other.to_ipc_error().message);
                break;
            }
        };
        let trimmed = msg.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parsed: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if ds_ipc::validate_message(&parsed).is_err() {
            continue;
        }
        if parsed.get("ok").is_some() {
            // an error envelope on the subscription connection (e.g. E_LIMIT close)
            if parsed["ok"] == Value::Bool(false) {
                reason = parsed
                    .get("error")
                    .and_then(IpcError::from_value)
                    .map(|e| format!("daemon error {}: {}", e.code.as_str(), e.message))
                    .unwrap_or_else(|| "daemon error".to_string());
                break;
            }
            continue;
        }
        let _ = tx.send(In::Event {
            sub: sub.clone(),
            event: parsed,
        });
        delivered += 1;
        if max_events.is_some_and(|m| delivered >= m) {
            reason = format!("max_events reached ({delivered})");
            break;
        }
    }
    let _ = tx.send(In::Closed { sub, reason });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_handle_carries_sub_and_stop() {
        let h = WatchHandle {
            sub: "w1".to_string(),
            stop: Arc::new(AtomicBool::new(false)),
        };
        assert_eq!(h.sub, "w1");
        assert!(!h.stop.load(Ordering::SeqCst));
        h.stop.store(true, Ordering::SeqCst);
        assert!(h.stop.load(Ordering::SeqCst));
    }
}
