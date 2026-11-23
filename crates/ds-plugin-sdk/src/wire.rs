//! Wire messages of the plugin protocol (`docs/contracts/plugins.md` §1).
//!
//! Transport: one JSON object per line (newline-JSON) over stdin/stdout. Plugin→host
//! messages are typed structs serialized in the documented field order; host→plugin messages
//! are parsed leniently from [`serde_json::Value`] so a malformed or unknown message can be
//! logged and skipped — a protocol violation never panics the plugin (or the host).

use serde::Serialize;
use serde_json::{Map, Value};

use crate::error::PluginError;
use crate::manifest::Manifest;

/// Wire protocol version spoken by this SDK (`protocol: 1` in `hello`).
pub const PROTOCOL_VERSION: u64 = 1;

/// Max bytes of one inbound line (mirrors `ds-ipc` `MAX_LINE_BYTES` = 1 MiB).
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// plugin -> host (serialized; field order is part of the contract tests)
// ---------------------------------------------------------------------------

/// `{"op":"register","id":"demo","manifest":{…}}`
#[derive(Serialize)]
pub struct RegisterMsg<'a> {
    pub op: &'static str,
    pub id: &'a str,
    pub manifest: &'a Manifest,
}

/// `{"op":"effects","request_id":7,"effects":[…]}` — `request_id` only when replying to an
/// `invoke`/correlated `event`.
#[derive(Serialize)]
pub struct EffectsMsg {
    pub op: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<u64>,
    pub effects: Vec<crate::effect::Effect>,
}

/// `{"op":"error","request_id":7,"error":{"code":"E_…","message":"…"}}` — sent instead of
/// `effects` when an invoke/event handler failed.
#[derive(Serialize)]
pub struct ErrorMsg {
    pub op: &'static str,
    pub request_id: u64,
    pub error: PluginError,
}

/// `{"op":"request","request_id":N,"kind":"get","path":…}` /
/// `{"kind":"set","path":…,"value":…}` / `{"kind":"net","method":…,"url":…,"body"?:…,"headers"?:…}`.
///
/// `kind` is followed by the kind-specific fields in this struct's declaration order, so the
/// `net` request serializes exactly like the contract example
/// `{"op":"request","request_id":9,"kind":"net","method":"GET","url":"http://lights.lan/api/…"}`.
#[derive(Serialize)]
pub struct RequestMsg {
    pub op: &'static str,
    pub request_id: u64,
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<Map<String, Value>>,
}

// ---------------------------------------------------------------------------
// host -> plugin (parsed leniently)
// ---------------------------------------------------------------------------

/// One host→plugin message. Parsing never fails hard: [`parse_host_msg`] returns
/// `Ok(None)` for lines that are not a JSON object with a known `op`, which the serve loop
/// logs to stderr and skips.
#[derive(Debug, Clone, PartialEq)]
pub enum HostMsg {
    Hello {
        protocol: u64,
        caps: Vec<String>,
        plugin_token: Option<String>,
        roots: Option<Value>,
        limits: Option<Value>,
    },
    Event {
        event: String,
        control: Option<String>,
        state: Option<Value>,
        request_id: Option<u64>,
    },
    Invoke {
        id: String,
        args: Value,
        request_id: u64,
    },
    Reply {
        request_id: u64,
        ok: bool,
        status: Option<u64>,
        body: Option<Value>,
        value: Option<Value>,
        rev: Option<u64>,
        error: Option<PluginError>,
    },
    /// Graceful shutdown request (see README: the contract pins EOF on stdin; an explicit
    /// `shutdown`/`stop` op is accepted as well).
    Shutdown,
}

fn get_str(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn get_u64(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_u64)
}

fn get_bool(v: &Value, key: &str) -> Option<bool> {
    v.get(key).and_then(Value::as_bool)
}

/// Parse one inbound line into a host message. `Ok(None)` = not a recognizable message
/// (the caller logs and skips); `Err` is never returned for content reasons.
pub fn parse_host_msg(line: &str) -> Result<Option<HostMsg>, serde_json::Error> {
    let v: Value = serde_json::from_str(line)?;
    let Some(obj) = v.as_object() else {
        return Ok(None);
    };
    let Some(op) = obj.get("op").and_then(Value::as_str) else {
        return Ok(None);
    };
    let msg = match op {
        "hello" => {
            let protocol = get_u64(&v, "protocol").unwrap_or(0);
            let caps = v
                .get("caps")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            HostMsg::Hello {
                protocol,
                caps,
                plugin_token: get_str(&v, "plugin_token"),
                roots: v.get("roots").cloned(),
                limits: v.get("limits").cloned(),
            }
        }
        "event" => HostMsg::Event {
            event: get_str(&v, "event").unwrap_or_default(),
            control: get_str(&v, "control"),
            state: v.get("state").cloned(),
            request_id: get_u64(&v, "request_id"),
        },
        "invoke" => HostMsg::Invoke {
            id: get_str(&v, "id").unwrap_or_default(),
            args: v.get("args").cloned().unwrap_or(Value::Null),
            request_id: get_u64(&v, "request_id").unwrap_or(0),
        },
        "reply" => HostMsg::Reply {
            request_id: get_u64(&v, "request_id").unwrap_or(0),
            ok: get_bool(&v, "ok").unwrap_or(false),
            status: get_u64(&v, "status"),
            body: v.get("body").cloned(),
            value: v.get("value").cloned(),
            rev: get_u64(&v, "rev"),
            error: v.get("error").and_then(PluginError::from_value),
        },
        "shutdown" | "stop" => HostMsg::Shutdown,
        _ => return Ok(None),
    };
    Ok(Some(msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_documented_hello_and_invoke() {
        let hello = parse_host_msg(
            r#"{"op":"hello","protocol":1,"plugin_token":"tok","caps":["state.read","net:lights.lan"],"roots":{"fs":["/home/u/media"]},"limits":{"cpu_s":10,"nproc":64,"nofile":64,"as_mb":1024}}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            hello,
            HostMsg::Hello {
                protocol: 1,
                caps: vec!["state.read".to_string(), "net:lights.lan".to_string()],
                plugin_token: Some("tok".to_string()),
                roots: Some(json!({"fs":["/home/u/media"]})),
                limits: Some(json!({"cpu_s":10,"nproc":64,"nofile":64,"as_mb":1024})),
            }
        );
        let inv =
            parse_host_msg(r#"{"op":"invoke","id":"demo.bump","args":{"by":1},"request_id":7}"#)
                .unwrap()
                .unwrap();
        assert_eq!(
            inv,
            HostMsg::Invoke {
                id: "demo.bump".into(),
                args: json!({"by":1}),
                request_id: 7
            }
        );
    }

    #[test]
    fn parses_event_reply_and_shutdown() {
        let ev = parse_host_msg(
            r#"{"op":"event","event":"keyDown","control":"keys.0","state":{"rev":3}}"#,
        )
        .unwrap()
        .unwrap();
        assert!(matches!(ev, HostMsg::Event { .. }));
        let rep =
            parse_host_msg(r#"{"op":"reply","request_id":9,"ok":true,"status":200,"body":"…"}"#)
                .unwrap()
                .unwrap();
        assert_eq!(
            rep,
            HostMsg::Reply {
                request_id: 9,
                ok: true,
                status: Some(200),
                body: Some(Value::String("…".into())),
                value: None,
                rev: None,
                error: None,
            }
        );
        assert_eq!(
            parse_host_msg(r#"{"op":"shutdown"}"#).unwrap(),
            Some(HostMsg::Shutdown)
        );
        assert_eq!(
            parse_host_msg(r#"{"op":"stop"}"#).unwrap(),
            Some(HostMsg::Shutdown)
        );
    }

    #[test]
    fn malformed_lines_are_none_not_panics() {
        assert!(parse_host_msg("not json").is_err());
        assert_eq!(parse_host_msg("[1,2]").unwrap(), None);
        assert_eq!(parse_host_msg(r#"{"no_op":1}"#).unwrap(), None);
        assert_eq!(parse_host_msg(r#"{"op":"wat"}"#).unwrap(), None);
        assert_eq!(parse_host_msg(r#"{"op":123}"#).unwrap(), None);
    }

    #[test]
    fn plugin_messages_serialize_in_documented_order() {
        let req = RequestMsg {
            op: "request",
            request_id: 9,
            kind: "net",
            path: None,
            value: None,
            method: Some("GET".into()),
            url: Some("http://lights.lan/api/…".into()),
            body: None,
            headers: None,
        };
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            r#"{"op":"request","request_id":9,"kind":"net","method":"GET","url":"http://lights.lan/api/…"}"#
        );
        let get = RequestMsg {
            op: "request",
            request_id: 8,
            kind: "get",
            path: Some("/controls/keys.0".into()),
            value: None,
            method: None,
            url: None,
            body: None,
            headers: None,
        };
        assert_eq!(
            serde_json::to_string(&get).unwrap(),
            r#"{"op":"request","request_id":8,"kind":"get","path":"/controls/keys.0"}"#
        );
        let err = ErrorMsg {
            op: "error",
            request_id: 7,
            error: PluginError::schema("unknown action: demo.nope"),
        };
        assert_eq!(
            serde_json::to_string(&err).unwrap(),
            r#"{"op":"error","request_id":7,"error":{"code":"E_SCHEMA","message":"unknown action: demo.nope"}}"#
        );
    }
}
