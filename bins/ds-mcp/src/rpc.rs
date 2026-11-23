//! Hand-rolled JSON-RPC 2.0 framing for the MCP stdio transport: one message per line
//! on stdin/stdout, nothing else ever on stdout (`docs/components/mcp-bridge.md`).

use serde_json::{json, Value};

/// JSON-RPC 2.0 error codes used by the bridge.
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;

/// The MCP protocol revision this bridge speaks (pinned — see
/// `docs/components/mcp-bridge.md` §Version assumptions; the server always answers with
/// this revision and a client that does not support it must disconnect).
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// A JSON-RPC 2.0 response with a `result`.
pub fn response(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// A JSON-RPC 2.0 response with an `error`.
pub fn error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

/// A JSON-RPC 2.0 notification (server → client, no `id`, never answered).
pub fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

/// Parse one stdin line into a JSON-RPC message. `Err` carries the id to answer with
/// (null when unrecoverable), the code, and the message.
pub fn parse_line(line: &str) -> Result<Value, (Value, i64, String)> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err((
            Value::Null,
            INVALID_REQUEST,
            "empty line is not a JSON-RPC message".to_string(),
        ));
    }
    let msg: Value = serde_json::from_str(trimmed)
        .map_err(|e| (Value::Null, PARSE_ERROR, format!("parse error: {e}")))?;
    if !msg.is_object() {
        return Err((
            Value::Null,
            INVALID_REQUEST,
            "JSON-RPC message must be a JSON object".to_string(),
        ));
    }
    if msg.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        return Err((
            id,
            INVALID_REQUEST,
            "missing \"jsonrpc\":\"2.0\"".to_string(),
        ));
    }
    Ok(msg)
}

/// A notification has no `id` and is never answered.
pub fn is_notification(msg: &Value) -> bool {
    msg.get("id").is_none()
}

/// The request method (empty string when absent).
pub fn method_of(msg: &Value) -> &str {
    msg.get("method").and_then(Value::as_str).unwrap_or("")
}

/// The correlation id (null when absent).
pub fn id_of(msg: &Value) -> Value {
    msg.get("id").cloned().unwrap_or(Value::Null)
}

/// The params object (null when absent).
pub fn params_of(msg: &Value) -> Value {
    msg.get("params").cloned().unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_requests_and_notifications() {
        let m = parse_line(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).unwrap();
        assert_eq!(id_of(&m), json!(1));
        assert!(!is_notification(&m));
        assert_eq!(method_of(&m), "ping");
        let n = parse_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap();
        assert!(is_notification(&n));
        assert_eq!(id_of(&n), Value::Null);
    }

    #[test]
    fn malformed_lines_map_to_jsonrpc_errors() {
        let (id, code, _) = parse_line("not json").unwrap_err();
        assert_eq!(id, Value::Null);
        assert_eq!(code, PARSE_ERROR);
        let (_, code, _) = parse_line("[1,2]").unwrap_err();
        assert_eq!(code, INVALID_REQUEST);
        let (id, code, _) = parse_line(r#"{"id":7,"method":"ping"}"#).unwrap_err();
        assert_eq!(id, json!(7));
        assert_eq!(code, INVALID_REQUEST);
        let (_, code, _) = parse_line("   ").unwrap_err();
        assert_eq!(code, INVALID_REQUEST);
    }

    #[test]
    fn response_and_notification_shapes() {
        let r = response(&json!(3), json!({"ok": true}));
        assert_eq!(r["jsonrpc"], "2.0");
        assert_eq!(r["id"], json!(3));
        assert_eq!(r["result"]["ok"], json!(true));
        let e = error(Value::Null, INVALID_PARAMS, "bad");
        assert_eq!(e["error"]["code"], json!(INVALID_PARAMS));
        assert!(e.get("result").is_none());
        let n = notification("notifications/deck/event", json!({"a": 1}));
        assert_eq!(n["method"], "notifications/deck/event");
        assert!(n.get("id").is_none());
    }
}
