//! The MCP tool table and the tool executor. Every tool is ONE daemon control-socket
//! op via `ds-client` (connect → hello → token auth → op), mirroring the `ds` verbs
//! (`docs/contracts/cli.md` §1); the bridge adds no policy, no caching, no shell, no
//! eval. Errors come back as MCP tool errors (`isError: true`) whose text carries the
//! stable `E_*` code and the CLI exit code; bad tool names/arguments are JSON-RPC
//! -32602. `deck_watch` is the exception: it answers immediately (bounded response) and
//! streams the subscription as MCP notifications (`crate::watch`).

use crate::bridge::In;
use crate::conn::{connect, Config, ToolFailure, CLIENT_NAME};
use crate::watch::{start_watch, WatchHandle};
use ds_client::ClientError;
use ds_ipc::error::{ErrorCode, IpcError};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;

/// Per-tool-call timeout (docs/components/mcp-bridge.md §Invariants).
pub const TOOL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Subscription id sequence (`w1`, `w2`, …).
static SUB_SEQ: AtomicU64 = AtomicU64::new(0);

/// What one `tools/call` produced.
pub enum ToolOutcome {
    /// A JSON result → `structuredContent` + text content, `isError: false`.
    Ok(Value),
    /// A daemon/client failure → tool error, `isError: true`, `E_*` + exit code in text.
    Fail(ToolFailure),
    /// Bad tool name or bad arguments → JSON-RPC -32602 (not a tool error).
    InvalidParams(String),
    /// `deck_watch`: bounded response now, events later as notifications.
    Watch {
        sub: String,
        path: String,
        handle: WatchHandle,
    },
}

/// Tool argument problems are -32602, daemon errors are tool errors.
pub enum ToolError {
    Params(String),
    Daemon(ToolFailure),
}

impl From<ClientError> for ToolError {
    fn from(e: ClientError) -> Self {
        ToolError::Daemon(ToolFailure::from(e))
    }
}

type ToolResult = Result<Value, ToolError>;

fn params<T>(r: Result<T, String>) -> Result<T, ToolError> {
    r.map_err(ToolError::Params)
}

/// Required string argument.
fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("argument '{key}' is required and must be a string"))
}

/// Optional string argument.
fn opt_str(args: &Value, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("argument '{key}' must be a string")),
    }
}

/// Optional unsigned integer argument.
fn opt_u64(args: &Value, key: &str) -> Result<Option<u64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("argument '{key}' must be a non-negative integer")),
    }
}

/// A state path argument: must start with '/' (the IPC `path` pattern).
fn check_path(path: &str) -> Result<(), String> {
    if path.starts_with('/') && !path.contains('\n') {
        Ok(())
    } else {
        Err(format!(
            "argument 'path' must be an absolute state path starting with '/', got {path:?}"
        ))
    }
}

/// S6 stream target vocabulary: `keys` | `dial` | `key:0..14`.
fn validate_target(target: &str) -> Result<(), String> {
    let valid = target == "keys"
        || target == "dial"
        || (target.starts_with("key:")
            && target[4..]
                .parse::<u32>()
                .is_ok_and(|n| n <= 14 && target[4..] == n.to_string()));
    if valid {
        Ok(())
    } else {
        Err(format!(
            "argument 'target' must be keys | key:0-14 | dial, got {target:?}"
        ))
    }
}

/// The ten tools, in `tools/list` order. An agent must be able to discover every
/// capability from `tools/list` alone (R1/R10).
pub fn tool_specs() -> Vec<Value> {
    vec![
        json!({
            "name": "deck_status",
            "description": "Daemon status (`ds status`): running, rev, pid, server, device, protocol, plus the control socket in use. One daemon/status op (read scope).",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        }),
        json!({
            "name": "deck_get",
            "description": "Read a state path (`ds get <path> [--fields a,b] [--rev N]`), e.g. /controls/keys/0/image. With rev, fails with E_CONFLICT unless the state is exactly at that rev.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "state path, must start with '/'"},
                    "fields": {"type": "array", "items": {"type": "string"}, "description": "optional subset of fields"},
                    "rev": {"type": "integer", "minimum": 0, "description": "optional expected revision"}
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "deck_set",
            "description": "Write a state path (`ds set <path> <value> [--if-rev N]`); value is any JSON valid for schemas/state.json, e.g. {\"kind\":\"color\",\"value\":\"#3cc86e\"}.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "state path, must start with '/'"},
                    "value": {"description": "any JSON value matching the path's schema"},
                    "if_rev": {"type": "integer", "minimum": 0, "description": "compare-and-set: fail with E_CONFLICT unless the state is at this rev"}
                },
                "required": ["path", "value"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "deck_bind",
            "description": "Append a binding (`ds bind <topic> --do '<step>' ... [--when '<cond>']`); returns the daemon-assigned id (bNNNN). Each 'do' entry is one step object (set/emit/plugin/exec/sleep per schemas/binding.json).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "topic": {"type": "string", "pattern": "^(deck|app)/", "description": "event topic to bind"},
                    "do": {"type": "array", "minItems": 1, "items": {"type": "object"}, "description": "step objects, e.g. {\"do\":\"set\",\"path\":\"/controls/keys/1/image\",\"value\":{...}}"},
                    "when": {"type": "object", "description": "optional condition, e.g. {\"payload\":\"delta\",\"op\":\"gt\",\"value\":0}"}
                },
                "required": ["topic", "do"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "deck_unbind",
            "description": "Remove a binding by id or by topic (`ds unbind <id> | --topic <topic>`); returns the removed ids.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "binding": {"type": "string", "pattern": "^b[0-9]{4,}$", "description": "binding id, e.g. b0001"},
                    "topic": {"type": "string", "pattern": "^(deck|app)/", "description": "remove all bindings with this topic"}
                },
                "additionalProperties": false,
                "description": "exactly one of 'binding' or 'topic'"
            }
        }),
        json!({
            "name": "deck_emit",
            "description": "Publish a topic on the event bus (`ds emit <topic> [json-payload]`), e.g. app/demo/red.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "topic": {"type": "string", "pattern": "^(deck|app)/"},
                    "payload": {"description": "any JSON payload (default {})"}
                },
                "required": ["topic"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "deck_watch",
            "description": "Subscribe to a state path or topic (`ds watch <path>`). Answers immediately with a subscription id; every event then arrives as a notifications/deck/event MCP notification, and notifications/deck/watch_closed ends the stream (cancel via notifications/cancelled, or after max_events).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "state path or topic pattern, e.g. /controls/keys/* or /deck/control/keys/*"},
                    "from_rev": {"type": "integer", "minimum": 0},
                    "max_events": {"type": "integer", "minimum": 1, "maximum": 100000, "description": "close the subscription after N events"}
                },
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "deck_stream_push",
            "description": "Push one image file as a DSF1 frame to a stream target (`ds stream push <target> --file <f>`): control-plane stream-open, then one binary frame on stream.sock. File must be a png/jpeg/gif (magic bytes), ≤ 16 MiB.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {"type": "string", "pattern": "^(keys|dial|key:(?:[0-9]|1[0-4]))$", "description": "stream target: keys | dial | key:0-14"},
                    "file": {"type": "string", "description": "path to a png/jpeg/gif file"},
                    "credits": {"type": "integer", "minimum": 1, "maximum": 1024}
                },
                "required": ["target", "file"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "deck_plugin_list",
            "description": "List installed plugins (`ds plugin list`): one plugin/list control op (read scope).",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        }),
        json!({
            "name": "deck_schema",
            "description": "Return an embedded JSON Schema (`ds schema <name>`): state | ipc | stream-frame | plugin-manifest | binding | action-metadata | preview.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string", "enum": ["state", "ipc", "stream-frame", "plugin-manifest", "binding", "action-metadata", "preview"]}
                },
                "required": ["name"],
                "additionalProperties": false
            }
        }),
    ]
}

/// The tool names (used by the tests to pin the tools/list order).
#[cfg(test)]
pub fn tool_names() -> Vec<&'static str> {
    // keep in sync with tool_specs (asserted by a unit test)
    vec![
        "deck_status",
        "deck_get",
        "deck_set",
        "deck_bind",
        "deck_unbind",
        "deck_emit",
        "deck_watch",
        "deck_stream_push",
        "deck_plugin_list",
        "deck_schema",
    ]
}

/// Execute one `tools/call`. `args` is the `arguments` object (may be null → `{}`).
pub fn exec_tool(cfg: &Config, tx: &Sender<In>, name: &str, args: &Value) -> ToolOutcome {
    if !args.is_object() {
        return ToolOutcome::InvalidParams(
            "tools/call 'arguments' must be a JSON object (or null)".to_string(),
        );
    }
    let args = args.clone();
    let result: ToolResult = match name {
        "deck_status" => tool_status(cfg, &args),
        "deck_get" => tool_get(cfg, &args),
        "deck_set" => tool_set(cfg, &args),
        "deck_bind" => tool_bind(cfg, &args),
        "deck_unbind" => tool_unbind(cfg, &args),
        "deck_emit" => tool_emit(cfg, &args),
        "deck_stream_push" => tool_stream_push(cfg, &args),
        "deck_plugin_list" => tool_plugin_list(cfg, &args),
        "deck_schema" => tool_schema(cfg, &args),
        "deck_watch" => return tool_watch(cfg, tx, &args),
        other => {
            return ToolOutcome::InvalidParams(format!(
                "unknown tool '{other}' — see tools/list for the available deck_* tools"
            ))
        }
    };
    match result {
        Ok(v) => ToolOutcome::Ok(v),
        Err(ToolError::Params(m)) => ToolOutcome::InvalidParams(m),
        Err(ToolError::Daemon(f)) => ToolOutcome::Fail(f),
    }
}

/// `daemon/status` + the local socket/pid-file state (`ds status`).
fn tool_status(cfg: &Config, _args: &Value) -> ToolResult {
    let mut c = connect(cfg)?;
    let resp = c.request(json!({"op": "daemon", "action": "status"}))?;
    let mut result = resp["result"].clone();
    if !result.is_object() {
        result = json!({"result": result});
    }
    result["socket"] = json!(cfg.socket.display().to_string());
    result["pid_file"] = json!(cfg.data_dir().join("daemon.pid").display().to_string());
    result["client"] = json!(CLIENT_NAME);
    Ok(result)
}

/// `get <path> [fields] [rev]`.
fn tool_get(cfg: &Config, args: &Value) -> ToolResult {
    let path = params(req_str(args, "path").and_then(|p| check_path(p).map(|_| p)))?;
    let fields = params(opt_str_array(args, "fields"))?;
    let want_rev = params(opt_u64(args, "rev"))?;
    let mut c = connect(cfg)?;
    let resp = c.get(path, fields.as_deref())?;
    let value = resp["result"]["value"].clone();
    let got_rev = resp.get("rev").and_then(Value::as_u64);
    if let Some(want) = want_rev {
        if got_rev != Some(want) {
            let mut e = IpcError::new(
                ErrorCode::EConflict,
                format!(
                    "rev mismatch: current rev is {}, requested {want}",
                    got_rev
                        .map(|r| r.to_string())
                        .unwrap_or_else(|| "none".into())
                ),
            );
            if let Some(cur) = got_rev {
                e = e.with_current_rev(cur);
            }
            return Err(ClientError::Server(e).into());
        }
    }
    let mut out = json!({"path": path, "value": value});
    if let Some(r) = got_rev {
        out["rev"] = json!(r);
    }
    Ok(out)
}

/// Optional `fields` array of strings.
fn opt_str_array(args: &Value, key: &str) -> Result<Option<Vec<String>>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(a)) => {
            let mut out = Vec::with_capacity(a.len());
            for v in a {
                match v.as_str() {
                    Some(s) => out.push(s.to_string()),
                    None => return Err(format!("argument '{key}' must be an array of strings")),
                }
            }
            Ok(Some(out))
        }
        Some(_) => Err(format!("argument '{key}' must be an array of strings")),
    }
}

/// `set <path> <value> [if_rev]` — one set op in one transaction.
fn tool_set(cfg: &Config, args: &Value) -> ToolResult {
    let path = params(req_str(args, "path").and_then(|p| check_path(p).map(|_| p)))?;
    let value = args
        .get("value")
        .cloned()
        .ok_or_else(|| "argument 'value' is required (any JSON)".to_string())
        .map_err(ToolError::Params)?;
    let if_rev = params(opt_u64(args, "if_rev"))?;
    let ops = json!([{"op": "set", "path": path, "value": value}]);
    let mut c = connect(cfg)?;
    let resp = c.set(ops, if_rev)?;
    let rev = resp.get("rev").and_then(Value::as_u64).unwrap_or(0);
    Ok(json!({"ok": true, "rev": rev}))
}

/// `bind <topic> --do <step>... [--when <cond>]` — steps are passed through as JSON
/// objects and validated against the normative schemas (client-side via
/// `validate_request`, daemon-side again).
fn tool_bind(cfg: &Config, args: &Value) -> ToolResult {
    let topic = params(req_str(args, "topic"))?;
    let dos = match args.get("do") {
        Some(Value::Array(a)) if !a.is_empty() && a.iter().all(Value::is_object) => a.clone(),
        _ => {
            return Err(ToolError::Params(
                "argument 'do' is required and must be a non-empty array of step objects"
                    .to_string(),
            ))
        }
    };
    let mut binding = json!({"id": "b0000", "topic": topic, "steps": dos});
    if let Some(when) = args.get("when") {
        if !when.is_null() {
            if !when.is_object() {
                return Err(ToolError::Params(
                    "argument 'when' must be a JSON object".to_string(),
                ));
            }
            binding["when"] = when.clone();
        }
    }
    let req = json!({"op": "bind", "binding": binding});
    ds_ipc::message::validate_request(&req)
        .map_err(|e| ToolError::Daemon(ToolFailure::new(ErrorCode::ESchema, e.message)))?;
    let mut c = connect(cfg)?;
    let resp = c.request(req)?;
    let rev = resp.get("rev").and_then(Value::as_u64).unwrap_or(0);
    let id = resp["result"]["id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    Ok(json!({"ok": true, "rev": rev, "id": id}))
}

/// `unbind <binding|topic>` — exactly one of the two.
fn tool_unbind(cfg: &Config, args: &Value) -> ToolResult {
    let binding = params(opt_str(args, "binding"))?;
    let topic = params(opt_str(args, "topic"))?;
    let req = match (binding, topic) {
        (Some(b), None) => json!({"op": "unbind", "binding": b}),
        (None, Some(t)) => json!({"op": "unbind", "topic": t}),
        _ => {
            return Err(ToolError::Params(
                "deck_unbind takes exactly one of 'binding' (bNNNN) or 'topic'".to_string(),
            ))
        }
    };
    ds_ipc::message::validate_request(&req)
        .map_err(|e| ToolError::Daemon(ToolFailure::new(ErrorCode::ESchema, e.message)))?;
    let mut c = connect(cfg)?;
    let resp = c.request(req)?;
    let rev = resp.get("rev").and_then(Value::as_u64).unwrap_or(0);
    let removed: Vec<String> = resp["result"]["removed"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({"ok": true, "rev": rev, "removed": removed}))
}

/// `emit <topic> [payload]`.
fn tool_emit(cfg: &Config, args: &Value) -> ToolResult {
    let topic = params(req_str(args, "topic"))?;
    let payload = args.get("payload").cloned().unwrap_or(Value::Null);
    let req = json!({"op": "emit", "topic": topic, "payload": payload});
    let mut c = connect(cfg)?;
    let resp = c.request(req)?;
    Ok(resp["result"].clone())
}

/// `plugin list` — one plugin/list op (on the phase-1 daemon this answers E_INTERNAL,
/// which is mapped honestly into the tool error).
fn tool_plugin_list(cfg: &Config, _args: &Value) -> ToolResult {
    let mut c = connect(cfg)?;
    let resp = c.request(json!({"op": "plugin", "action": "list"}))?;
    Ok(resp["result"].clone())
}

/// `schema <name>` — the daemon serves the embedded schemas.
fn tool_schema(cfg: &Config, args: &Value) -> ToolResult {
    let name = params(req_str(args, "name"))?;
    let mut c = connect(cfg)?;
    let resp = c.request(json!({"op": "schema", "name": name}))?;
    Ok(resp["result"].clone())
}

/// `deck_watch`: bounded response now, events as notifications.
fn tool_watch(cfg: &Config, tx: &Sender<In>, args: &Value) -> ToolOutcome {
    let mk = |r: Result<Value, ToolError>| match r {
        Ok(v) => ToolOutcome::Ok(v),
        Err(ToolError::Params(m)) => ToolOutcome::InvalidParams(m),
        Err(ToolError::Daemon(f)) => ToolOutcome::Fail(f),
    };
    let path = match params(req_str(args, "path").and_then(|p| check_path(p).map(|_| p))) {
        Ok(p) => p,
        Err(e) => return mk(Err(e)),
    };
    let from_rev = match params(opt_u64(args, "from_rev")) {
        Ok(v) => v,
        Err(e) => return mk(Err(e)),
    };
    let max_events = match params(opt_u64(args, "max_events")) {
        Ok(v) => v,
        Err(e) => return mk(Err(e)),
    };
    if let Some(m) = max_events {
        if m == 0 || m > 100_000 {
            return mk(Err(ToolError::Params(
                "argument 'max_events' must be between 1 and 100000".to_string(),
            )));
        }
    }
    let sub = format!("w{}", SUB_SEQ.fetch_add(1, Ordering::SeqCst) + 1);
    match start_watch(
        cfg,
        tx.clone(),
        sub.clone(),
        path.to_string(),
        from_rev,
        max_events,
    ) {
        Ok(handle) => ToolOutcome::Watch {
            sub,
            path: path.to_string(),
            handle,
        },
        Err(e) => ToolOutcome::Fail(ToolFailure::from(e)),
    }
}

/// `stream push <target> --file <f>`: one DSF1 frame on the frame plane.
fn tool_stream_push(cfg: &Config, args: &Value) -> ToolResult {
    let target = params(req_str(args, "target").and_then(|t| validate_target(t).map(|_| t)))?;
    let file = params(req_str(args, "file"))?;
    let credits = params(opt_u64(args, "credits"))?;
    crate::stream::push_one_frame(cfg, target, file, credits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_list_has_the_ten_tools_in_order() {
        let specs = tool_specs();
        let names: Vec<&str> = specs.iter().map(|s| s["name"].as_str().unwrap()).collect();
        assert_eq!(names, tool_names());
        assert_eq!(names.len(), 10);
        for s in &specs {
            assert_eq!(s["inputSchema"]["type"], json!("object"), "{}", s["name"]);
            assert!(s["description"].as_str().unwrap().len() > 10);
        }
        // spot-check required arguments
        let by_name = |n: &str| {
            specs
                .iter()
                .find(|s| s["name"] == json!(n))
                .unwrap()
                .clone()
        };
        assert_eq!(
            by_name("deck_get")["inputSchema"]["required"],
            json!(["path"])
        );
        assert_eq!(
            by_name("deck_set")["inputSchema"]["required"],
            json!(["path", "value"])
        );
        assert_eq!(
            by_name("deck_watch")["inputSchema"]["required"],
            json!(["path"])
        );
        assert_eq!(
            by_name("deck_stream_push")["inputSchema"]["required"],
            json!(["target", "file"])
        );
        assert_eq!(
            by_name("deck_schema")["inputSchema"]["required"],
            json!(["name"])
        );
    }

    #[test]
    fn target_validation_matches_s6() {
        assert!(validate_target("keys").is_ok());
        assert!(validate_target("dial").is_ok());
        assert!(validate_target("key:0").is_ok());
        assert!(validate_target("key:14").is_ok());
        assert!(validate_target("key:15").is_err());
        assert!(validate_target("key:x").is_err());
        assert!(validate_target("panel").is_err());
        assert!(validate_target("screen").is_err());
        assert!(validate_target("").is_err());
    }

    #[test]
    fn path_validation() {
        assert!(check_path("/controls/keys/0").is_ok());
        assert!(check_path("/").is_ok());
        assert!(check_path("controls").is_err());
        assert!(check_path("").is_err());
    }

    #[test]
    fn arg_helpers() {
        let args = json!({"path": "/x", "rev": 3, "fields": ["a", "b"]});
        assert_eq!(req_str(&args, "path").unwrap(), "/x");
        assert!(req_str(&args, "missing").is_err());
        assert_eq!(opt_u64(&args, "rev").unwrap(), Some(3));
        assert!(opt_u64(&args, "path").is_err());
        assert_eq!(
            opt_str_array(&args, "fields").unwrap().unwrap(),
            vec!["a", "b"]
        );
        assert!(opt_str_array(&json!({"fields": [1]}), "fields").is_err());
        assert_eq!(
            opt_str(&json!({"topic": "app/x"}), "topic")
                .unwrap()
                .unwrap(),
            "app/x"
        );
        assert_eq!(opt_str(&json!({}), "topic").unwrap(), None);
    }

    #[test]
    fn unknown_tool_and_bad_arguments_are_invalid_params() {
        let cfg = Config::new("/nonexistent/daemon.sock".into(), None);
        let (tx, _rx) = std::sync::mpsc::channel();
        match exec_tool(&cfg, &tx, "deck_nope", &json!({})) {
            ToolOutcome::InvalidParams(m) => assert!(m.contains("unknown tool"), "{m}"),
            _ => panic!("unknown tool must be InvalidParams"),
        }
        match exec_tool(&cfg, &tx, "deck_get", &json!({})) {
            ToolOutcome::InvalidParams(m) => assert!(m.contains("'path'"), "{m}"),
            _ => panic!("missing path must be InvalidParams"),
        }
        match exec_tool(&cfg, &tx, "deck_get", &json!([1, 2])) {
            ToolOutcome::InvalidParams(m) => assert!(m.contains("arguments"), "{m}"),
            _ => panic!("non-object arguments must be InvalidParams"),
        }
        match exec_tool(&cfg, &tx, "deck_unbind", &json!({})) {
            ToolOutcome::InvalidParams(m) => assert!(m.contains("exactly one"), "{m}"),
            _ => panic!("unbind without binding/topic must be InvalidParams"),
        }
        match exec_tool(
            &cfg,
            &tx,
            "deck_stream_push",
            &json!({"target": "panel", "file": "/x"}),
        ) {
            ToolOutcome::InvalidParams(m) => assert!(m.contains("target"), "{m}"),
            _ => panic!("bad target must be InvalidParams"),
        }
    }

    #[test]
    fn unreachable_daemon_maps_to_actionable_tool_failure() {
        let cfg = Config::new(
            std::path::PathBuf::from("/tmp/ds-mcp-test-missing-zz/daemon.sock"),
            None,
        );
        let (tx, _rx) = std::sync::mpsc::channel();
        match exec_tool(&cfg, &tx, "deck_status", &json!({})) {
            ToolOutcome::Fail(f) => {
                assert_eq!(f.code, "E_UNREACHABLE");
                assert_eq!(f.exit, 6);
                assert!(f.text().contains("ds daemon start"), "{}", f.text());
            }
            _ => panic!("daemon down must be a tool failure"),
        }
    }
}
