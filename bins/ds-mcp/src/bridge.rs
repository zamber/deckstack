//! The MCP server core: one JSON-RPC message per line in, responses/notifications out.
//! `Bridge::handle_line` is transport-independent (unit-tested directly); `main.rs`
//! wires it to stdin/stdout and merges the watch-event channel into the output stream.

use crate::conn::{Config, ToolFailure};
use crate::rpc;
use crate::tools::{self, ToolOutcome};
use crate::watch::WatchHandle;
use ds_ipc::error::ErrorCode;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;

/// Messages arriving at the main loop from the stdin reader and the watch threads.
#[derive(Debug)]
pub enum In {
    /// One raw stdin line.
    Line(String),
    /// stdin closed → exit.
    Eof,
    /// A subscription event → `notifications/deck/event`.
    Event { sub: String, event: Value },
    /// The subscription ended → `notifications/deck/watch_closed`.
    Closed { sub: String, reason: String },
}

/// The MCP notification method carrying a deck event.
pub const EVENT_NOTIFICATION: &str = "notifications/deck/event";
/// The MCP notification method announcing the end of a deck_watch stream.
pub const CLOSED_NOTIFICATION: &str = "notifications/deck/watch_closed";

/// The MCP bridge: config + active watch subscriptions (request id → handle).
pub struct Bridge {
    cfg: Config,
    tx: Sender<In>,
    watches: HashMap<i64, WatchHandle>,
}

impl Bridge {
    pub fn new(cfg: Config, tx: Sender<In>) -> Self {
        Self {
            cfg,
            tx,
            watches: HashMap::new(),
        }
    }

    /// Handle one raw stdin line; returns the protocol messages to write to stdout.
    /// `async_tools` runs each `tools/call` on a worker thread with the 10 s
    /// per-call timeout (production path); `false` runs it inline (tests).
    pub fn handle_line(&mut self, line: &str, async_tools: bool) -> Vec<Value> {
        let msg = match rpc::parse_line(line) {
            Ok(m) => m,
            Err((id, code, message)) => return vec![rpc::error(id, code, message)],
        };
        if rpc::is_notification(&msg) {
            self.handle_notification(&msg);
            return Vec::new();
        }
        let id = rpc::id_of(&msg);
        let method = rpc::method_of(&msg).to_string();
        let params = rpc::params_of(&msg);
        vec![self.dispatch(&id, &method, params, async_tools)]
    }

    /// Handle one incoming channel message that is not a stdin line (watch events).
    pub fn handle_in(&mut self, msg: In) -> Vec<Value> {
        match msg {
            In::Line(l) => self.handle_line(&l, true),
            In::Eof => Vec::new(),
            In::Event { sub, event } => vec![rpc::notification(
                EVENT_NOTIFICATION,
                json!({"subscription": sub, "event": event}),
            )],
            In::Closed { sub, reason } => {
                // drop the handle if still registered (cancel path removes it already)
                self.watches.retain(|_, h| h.sub != sub);
                vec![rpc::notification(
                    CLOSED_NOTIFICATION,
                    json!({"subscription": sub, "reason": reason}),
                )]
            }
        }
    }

    fn handle_notification(&mut self, msg: &Value) {
        match rpc::method_of(msg) {
            "notifications/initialized" => {}
            "notifications/cancelled" => {
                let params = rpc::params_of(msg);
                let rid = params
                    .get("requestId")
                    .or_else(|| params.get("id"))
                    .and_then(Value::as_i64);
                if let Some(rid) = rid {
                    if let Some(h) = self.watches.remove(&rid) {
                        h.stop.store(true, Ordering::SeqCst);
                    }
                }
            }
            _ => {} // unknown notifications are ignored (JSON-RPC 2.0)
        }
    }

    fn dispatch(&mut self, id: &Value, method: &str, params: Value, async_tools: bool) -> Value {
        match method {
            "initialize" => rpc::response(id, initialize_result(&params)),
            "ping" => rpc::response(id, json!({})),
            "tools/list" => rpc::response(id, json!({"tools": tools::tool_specs()})),
            "tools/call" => self.tools_call(id, params, async_tools),
            other => rpc::error(
                id.clone(),
                rpc::METHOD_NOT_FOUND,
                format!("method not found: {other} (this bridge serves initialize, ping, tools/list, tools/call)"),
            ),
        }
    }

    fn tools_call(&mut self, id: &Value, params: Value, async_tools: bool) -> Value {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return rpc::error(
                id.clone(),
                rpc::INVALID_PARAMS,
                "tools/call params needs 'name' (string) and optional 'arguments' (object)",
            );
        };
        let args = params.get("arguments").cloned().unwrap_or(Value::Null);
        let outcome = if async_tools {
            self.exec_tool_async(name, &args)
        } else {
            tools::exec_tool(&self.cfg, &self.tx, name, &args)
        };
        match outcome {
            ToolOutcome::InvalidParams(m) => rpc::error(id.clone(), rpc::INVALID_PARAMS, m),
            ToolOutcome::Fail(f) => rpc::response(id, tool_error_result(&f)),
            ToolOutcome::Ok(v) => rpc::response(id, tool_ok_result(&v)),
            ToolOutcome::Watch { sub, path, handle } => {
                if let Some(rid) = id.as_i64() {
                    self.watches.insert(rid, handle);
                }
                rpc::response(id, tool_watch_result(&sub, &path))
            }
        }
    }

    /// Run one tool call on a worker thread with the 10 s per-call timeout. On timeout
    /// the worker is abandoned (it sends into a closed channel and exits on its own);
    /// the caller answers with a tool error.
    fn exec_tool_async(&self, name: &str, args: &Value) -> ToolOutcome {
        let cfg = self.cfg.clone();
        let tx = self.tx.clone();
        let name = name.to_string();
        let args = args.clone();
        let (otx, orx) = std::sync::mpsc::channel();
        let fallback_name = name.clone();
        let fallback_args = args.clone();
        let timeout_name = name.clone();
        let spawned = std::thread::Builder::new()
            .name("ds-mcp-tool".to_string())
            .spawn(move || {
                let _ = otx.send(tools::exec_tool(&cfg, &tx, &name, &args));
            });
        let orx = match spawned {
            Ok(_) => orx,
            Err(_) => {
                // cannot even spawn: run inline as a fallback
                return tools::exec_tool(&self.cfg, &self.tx, &fallback_name, &fallback_args);
            }
        };
        match orx.recv_timeout(tools::TOOL_TIMEOUT) {
            Ok(o) => o,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => ToolOutcome::Fail(ToolFailure::new(
                ErrorCode::EInternal,
                format!(
                    "tool call '{timeout_name}' exceeded the {}-second per-call timeout",
                    tools::TOOL_TIMEOUT.as_secs()
                ),
            )),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => ToolOutcome::Fail(
                ToolFailure::new(ErrorCode::EInternal, "tool worker thread died"),
            ),
        }
    }
}

/// The `initialize` result (MCP handshake).
fn initialize_result(params: &Value) -> Value {
    let _requested = params.get("protocolVersion").and_then(Value::as_str);
    json!({
        "protocolVersion": rpc::MCP_PROTOCOL_VERSION,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": "ds-mcp", "version": env!("CARGO_PKG_VERSION")},
        "instructions": "deckstack MCP bridge: every tool is one ds-daemon control-socket op (token-authed, 10 s timeout). deck_watch answers with a subscription id and streams events as notifications/deck/event. Tool errors carry the stable E_* code and the CLI exit code; daemon down answers E_UNREACHABLE — run `ds daemon start`. Full spec: docs/components/mcp-bridge.md."
    })
}

/// A successful tools/call result: text content + structuredContent (objects only).
pub fn tool_ok_result(v: &Value) -> Value {
    let text = serde_json::to_string_pretty(v).unwrap_or_default();
    let mut result = json!({
        "content": [{"type": "text", "text": text}],
        "isError": false,
    });
    if v.is_object() {
        result["structuredContent"] = v.clone();
    }
    result
}

/// A failed tools/call result (MCP tool error): `E_*` code + exit code in the text.
pub fn tool_error_result(f: &ToolFailure) -> Value {
    json!({
        "content": [{"type": "text", "text": f.text()}],
        "isError": true,
    })
}

/// The bounded deck_watch tools/call result.
pub fn tool_watch_result(sub: &str, path: &str) -> Value {
    tool_ok_result(&json!({
        "subscription": sub,
        "path": path,
        "events": EVENT_NOTIFICATION,
        "closed": CLOSED_NOTIFICATION,
        "note": "events arrive as MCP notifications until notifications/cancelled (with this call's id) or max_events"
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bridge() -> (Bridge, std::sync::mpsc::Receiver<In>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let cfg = Config::new("/nonexistent/daemon.sock".into(), None);
        (Bridge::new(cfg, tx), rx)
    }

    #[test]
    fn initialize_handshake_shape() {
        let (mut b, _rx) = bridge();
        let out = b.handle_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
            false,
        );
        assert_eq!(out.len(), 1);
        let r = &out[0];
        assert_eq!(r["id"], json!(1));
        assert_eq!(
            r["result"]["protocolVersion"],
            json!(rpc::MCP_PROTOCOL_VERSION)
        );
        assert_eq!(r["result"]["serverInfo"]["name"], json!("ds-mcp"));
        assert!(r["result"]["capabilities"]["tools"].is_object());
        assert!(r["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("ds daemon start"));
    }

    #[test]
    fn notifications_get_no_response_and_unknown_methods_error() {
        let (mut b, _rx) = bridge();
        let out = b.handle_line(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            false,
        );
        assert!(out.is_empty());
        let out = b.handle_line(
            r#"{"jsonrpc":"2.0","id":9,"method":"resources/list"}"#,
            false,
        );
        assert_eq!(out[0]["error"]["code"], json!(rpc::METHOD_NOT_FOUND));
        assert_eq!(out[0]["id"], json!(9));
    }

    #[test]
    fn malformed_lines_answer_jsonrpc_errors() {
        let (mut b, _rx) = bridge();
        let out = b.handle_line("this is not json", false);
        assert_eq!(out[0]["error"]["code"], json!(rpc::PARSE_ERROR));
        assert_eq!(out[0]["id"], Value::Null);
        let out = b.handle_line(r#"{"jsonrpc":"1.0","id":2,"method":"ping"}"#, false);
        assert_eq!(out[0]["error"]["code"], json!(rpc::INVALID_REQUEST));
        assert_eq!(out[0]["id"], json!(2));
    }

    #[test]
    fn tools_list_returns_the_ten_tools() {
        let (mut b, _rx) = bridge();
        let out = b.handle_line(r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#, false);
        let tools = out[0]["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 10);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, tools::tool_names());
    }

    #[test]
    fn tools_call_errors_are_jsonrpc_not_crashes() {
        let (mut b, _rx) = bridge();
        // unknown tool -> -32602
        let out = b.handle_line(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"deck_nope","arguments":{}}}"#,
            false,
        );
        assert_eq!(out[0]["error"]["code"], json!(rpc::INVALID_PARAMS));
        assert!(out[0]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown tool"));
        // missing name -> -32602
        let out = b.handle_line(
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{}}"#,
            false,
        );
        assert_eq!(out[0]["error"]["code"], json!(rpc::INVALID_PARAMS));
        // daemon down -> tool error with E_UNREACHABLE + hint (not a crash, not -32602)
        let out = b.handle_line(
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"deck_status","arguments":{}}}"#,
            false,
        );
        let r = &out[0]["result"];
        assert_eq!(r["isError"], json!(true));
        let text = r["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("E_UNREACHABLE"), "{text}");
        assert!(text.contains("ds daemon start"), "{text}");
        assert!(text.contains("[exit 6]"), "{text}");
    }

    #[test]
    fn tool_ok_result_has_structured_content_for_objects() {
        let r = tool_ok_result(&json!({"ok": true, "rev": 2}));
        assert_eq!(r["isError"], json!(false));
        assert_eq!(r["structuredContent"]["rev"], json!(2));
        assert!(r["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("\"rev\": 2"));
        let r = tool_ok_result(&json!(42));
        assert!(r.get("structuredContent").is_none());
    }

    #[test]
    fn watch_result_is_bounded_and_documents_notifications() {
        let r = tool_watch_result("w1", "/controls/keys/*");
        assert_eq!(r["structuredContent"]["subscription"], json!("w1"));
        assert_eq!(r["structuredContent"]["events"], json!(EVENT_NOTIFICATION));
        assert_eq!(r["structuredContent"]["closed"], json!(CLOSED_NOTIFICATION));
    }

    #[test]
    fn cancel_notification_stops_a_registered_watch() {
        let (mut b, _rx) = bridge();
        let handle = crate::watch::WatchHandle {
            sub: "w7".to_string(),
            stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let stop = handle.stop.clone();
        b.watches.insert(77, handle);
        let out = b.handle_line(
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":77,"reason":"user"}}"#,
            false,
        );
        assert!(out.is_empty());
        assert!(stop.load(Ordering::SeqCst));
        assert!(b.watches.is_empty());
        // cancelling an unknown id is a no-op, not an error
        let out = b.handle_line(
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":999}}"#,
            false,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn watch_events_become_notifications() {
        let (mut b, rx) = bridge();
        let out = b.handle_in(In::Event {
            sub: "w1".to_string(),
            event: json!({"op": "event", "path": "/controls/keys/0/image", "rev": 2}),
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["method"], json!(EVENT_NOTIFICATION));
        assert_eq!(out[0]["params"]["subscription"], json!("w1"));
        assert_eq!(
            out[0]["params"]["event"]["path"],
            json!("/controls/keys/0/image")
        );
        assert!(rx.try_recv().is_err()); // nothing was sent back into the channel
        let out = b.handle_in(In::Closed {
            sub: "w1".to_string(),
            reason: "cancelled by the client".to_string(),
        });
        assert_eq!(out[0]["method"], json!(CLOSED_NOTIFICATION));
        assert_eq!(out[0]["params"]["reason"], json!("cancelled by the client"));
    }
}
