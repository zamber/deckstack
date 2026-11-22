//! Message types and schema validation for the NDJSON control plane.
//!
//! `schemas/ipc-message.json` is normative and embedded at compile time (`include_str!`),
//! so the crate, the fixture gate (`tests/fixtures/validate.sh`) and the daemon all
//! validate against the exact same file. The external `$ref` to `binding.json` (used by
//! the `bind` op) is served from the embedded copy by a local retriever — no network.
//!
//! The schema discriminates requests from responses with `ok` (m3): requests never carry
//! `ok`, responses always do. [`validate_request`] / [`validate_message`] enforce that on
//! both sides of the socket.

use crate::error::{ErrorCode, IpcError};
use jsonschema::{Draft, Retrieve, Uri, Validator};
use serde_json::Value;
use std::sync::OnceLock;

/// `schemas/ipc-message.json`, embedded at compile time.
pub const IPC_SCHEMA_JSON: &str = include_str!("../../../schemas/ipc-message.json");
/// `schemas/binding.json`, embedded (external `$ref` target of `ipc-message.json`).
pub const BINDING_SCHEMA_JSON: &str = include_str!("../../../schemas/binding.json");

/// Serves the `https://deckstack.dev/schemas/*.json/vN` `$ref`s from the embedded files.
struct LocalRetriever;

impl Retrieve for LocalRetriever {
    fn retrieve(
        &self,
        uri: &Uri<String>,
    ) -> std::result::Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        match uri.as_str() {
            "https://deckstack.dev/schemas/binding.json/v1" => {
                Ok(serde_json::from_str(BINDING_SCHEMA_JSON)?)
            }
            other => Err(format!("unknown schema reference: {other}").into()),
        }
    }
}

/// The compiled `schemas/ipc-message.json` validator (draft 2020-12).
pub fn message_validator() -> &'static Validator {
    static VALIDATOR: OnceLock<Validator> = OnceLock::new();
    VALIDATOR.get_or_init(|| {
        let schema: Value = serde_json::from_str(IPC_SCHEMA_JSON)
            .expect("embedded schemas/ipc-message.json parses");
        jsonschema::options()
            .with_draft(Draft::Draft202012)
            .with_retriever(LocalRetriever)
            .build(&schema)
            .expect("schemas/ipc-message.json compiles")
    })
}

/// Validate any control message (request, response, or async server message) against
/// `schemas/ipc-message.json`. Returns the first violation as an `E_SCHEMA` error.
pub fn validate_message(msg: &Value) -> Result<(), IpcError> {
    if let Err(err) = message_validator().validate(msg) {
        return Err(IpcError::new(
            ErrorCode::ESchema,
            format!("message violates schemas/ipc-message.json: {err}"),
        ));
    }
    Ok(())
}

/// The request ops of `schemas/ipc-message.json` (the `const` values of the request
/// branch). Async server messages (`event`, `dropped`, `stream-*`, `welcome`) are not
/// requests even though they carry an `op`.
pub const REQUEST_OPS: &[&str] = &[
    "hello",
    "auth",
    "ping",
    "get",
    "set",
    "subscribe",
    "unsubscribe",
    "emit",
    "bind",
    "unbind",
    "plugin",
    "stream-open",
    "stream-stats",
    "stream-stop",
    "daemon",
    "token",
    "schema",
];

/// Validate that `msg` is a well-formed **request** (client → daemon): schema-valid, a
/// known request `op`, and without `ok` (m3 discriminator). Async server messages are not
/// requests.
pub fn validate_request(msg: &Value) -> Result<(), IpcError> {
    validate_message(msg)?;
    if msg.get("ok").is_some() {
        return Err(IpcError::new(
            ErrorCode::ESchema,
            "requests never carry 'ok' (responses always do)",
        ));
    }
    let op = msg.get("op").and_then(Value::as_str);
    match op {
        Some(op) if REQUEST_OPS.contains(&op) => Ok(()),
        _ => Err(IpcError::new(
            ErrorCode::ESchema,
            "not a request: 'op' must be one of the request ops",
        )),
    }
}

/// Validate that `msg` is a well-formed **response or async server message**
/// (daemon → client).
pub fn validate_server_message(msg: &Value) -> Result<(), IpcError> {
    validate_message(msg)
}

/// A validated request: correlation `id` (optional on the wire for handshake errors),
/// the `op` string, and the raw JSON object for the handler.
#[derive(Debug, Clone)]
pub struct Request {
    raw: Value,
}

impl Request {
    /// Wrap an already schema-validated request value.
    pub fn new(raw: Value) -> Self {
        Self { raw }
    }

    /// Parse + validate one NDJSON line as a request.
    pub fn parse(line: &str) -> Result<Self, IpcError> {
        let trimmed = line.strip_suffix('\r').unwrap_or(line);
        let value: Value = serde_json::from_str(trimmed).map_err(|e| {
            IpcError::new(
                ErrorCode::ESchema,
                format!("line is not one JSON object: {e}"),
            )
        })?;
        validate_request(&value)?;
        Ok(Self { raw: value })
    }

    /// The raw JSON object.
    pub fn raw(&self) -> &Value {
        &self.raw
    }

    /// The correlation id, if the request carries an integer one.
    pub fn id(&self) -> Option<i64> {
        self.raw.get("id").and_then(Value::as_i64)
    }

    /// The op string.
    pub fn op(&self) -> &str {
        self.raw.get("op").and_then(Value::as_str).unwrap_or("")
    }

    /// Any top-level field of the request.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.raw.get(key)
    }
}

/// A response (or error) to send back: `{"id":…,"ok":true,"rev"?,"result":…}` or
/// `{"id":…,"ok":false,"error":{…}}`. The server fills in the request's `id`.
#[derive(Debug, Clone)]
pub struct Response {
    /// Correlation id; overwritten by the server with the request's id before sending.
    pub id: Option<i64>,
    pub ok: bool,
    pub rev: Option<u64>,
    pub result: Option<Value>,
    pub error: Option<IpcError>,
}

impl Response {
    /// `{"id":…,"ok":true,"result":…}`.
    pub fn ok(id: Option<i64>, result: Value) -> Self {
        Self {
            id,
            ok: true,
            rev: None,
            result: Some(result),
            error: None,
        }
    }

    /// `{"id":…,"ok":true,"rev":N,"result":…}` (state-changing ops).
    pub fn ok_rev(id: Option<i64>, rev: u64, result: Value) -> Self {
        Self {
            id,
            ok: true,
            rev: Some(rev),
            result: Some(result),
            error: None,
        }
    }

    /// `{"id":…,"ok":false,"error":{…}}`. A `None` id produces an id-less envelope, used
    /// for framing-level errors where no request id could be recovered.
    pub fn error(id: Option<i64>, err: IpcError) -> Self {
        Self {
            id,
            ok: false,
            rev: None,
            result: None,
            error: Some(err),
        }
    }

    /// Convenience: `Response::error(id, ErrorCode::X, "msg")`.
    pub fn error_code(id: Option<i64>, code: ErrorCode, message: impl Into<String>) -> Self {
        Self::error(id, IpcError::new(code, message))
    }

    /// The wire JSON object, validated against the schema's response branch.
    pub fn to_value(&self) -> Value {
        let mut m = serde_json::Map::new();
        if let Some(id) = self.id {
            m.insert("id".into(), Value::from(id));
        }
        m.insert("ok".into(), Value::from(self.ok));
        if let Some(rev) = self.rev {
            m.insert("rev".into(), Value::from(rev));
        }
        if let Some(r) = &self.result {
            m.insert("result".into(), r.clone());
        }
        if let Some(e) = &self.error {
            m.insert("error".into(), e.to_value());
        }
        Value::Object(m)
    }

    /// Validate this response against the schema (server-side, before sending).
    pub fn validate(&self) -> Result<(), IpcError> {
        validate_server_message(&self.to_value())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn requests_from_the_corpus_validate() {
        for msg in [
            json!({"id": 0, "op": "hello", "protocol": 1, "client": "ds/0.1.0"}),
            json!({"id": 1, "op": "auth", "token": "0123456789abcdef0123456789abcdef", "scope": "admin"}),
            json!({"id": 2, "op": "ping"}),
            json!({"id": 3, "op": "get", "path": "/meta"}),
            json!({"id": 5, "op": "set", "ops": [{"op": "set", "path": "/controls/keys/0/image", "value": {"kind": "color", "value": "#3cc86e"}}], "if_rev": 41}),
            json!({"id": 7, "op": "subscribe", "path": "/controls/keys/*", "from_rev": 41}),
            json!({"id": 9, "op": "emit", "topic": "app/demo/hello", "payload": {"n": 1}}),
            json!({"id": 14, "op": "stream-open", "target": "keys", "credits": 8}),
            json!({"id": 17, "op": "daemon", "action": "status"}),
            json!({"id": 19, "op": "schema", "name": "stream-frame"}),
        ] {
            validate_request(&msg).unwrap();
            let req = Request::parse(&serde_json::to_string(&msg).unwrap()).unwrap();
            assert!(req.id().is_some());
        }
    }

    #[test]
    fn responses_and_events_validate_but_are_not_requests() {
        let resp = json!({"id": 5, "ok": true, "rev": 42});
        validate_message(&resp).unwrap();
        assert!(validate_request(&resp).is_err());
        let err = json!({"id": 6, "ok": false, "error": {"code": "E_SCOPE", "message": "no"}});
        validate_message(&err).unwrap();
        assert!(validate_request(&err).is_err());
        let event =
            json!({"op": "event", "path": "/controls/keys/0/image", "rev": 42, "value": {}});
        validate_message(&event).unwrap();
        assert!(validate_request(&event).is_err());
        // id-less error envelope (framing-level failure)
        let no_id = json!({"ok": false, "error": {"code": "E_LIMIT", "message": "line too long"}});
        validate_message(&no_id).unwrap();
    }

    #[test]
    fn bad_messages_are_rejected() {
        // unknown op -> E_SCHEMA (the CLI maps this to usage, exit 1)
        let unknown = json!({"id": 1, "op": "migrate", "path": "/x"});
        let err = validate_request(&unknown).unwrap_err();
        assert_eq!(err.code, ErrorCode::ESchema);
        // request carrying ok
        assert!(validate_request(&json!({"op": "ping", "ok": true})).is_err());
        // hello with a non-1 protocol is schema-valid (the SERVER answers E_VERSION,
        // docs/contracts/ipc.md §2) but non-integer/negative protocols are rejected
        assert!(validate_request(&json!({"op": "hello", "protocol": 2})).is_ok());
        assert!(validate_request(&json!({"op": "hello", "protocol": -1})).is_err());
        assert!(validate_request(&json!({"op": "hello", "protocol": "1"})).is_err());
        // unbind carries the binding id in 'binding' (the envelope's 'id' is the
        // integer correlation id) or a 'topic'
        assert!(validate_request(&json!({"id": 11, "op": "unbind", "binding": "b0001"})).is_ok());
        assert!(validate_request(&json!({"id": 11, "op": "unbind", "topic": "deck/x"})).is_ok());
        assert!(validate_request(&json!({"op": "unbind", "id": "b0001"})).is_err());
        // bad path syntax
        assert!(validate_request(&json!({"op": "get", "path": "controls"})).is_err());
        // auth token too short (minLength 16)
        assert!(validate_request(&json!({"op": "auth", "token": "short"})).is_err());
        // not an object
        assert!(validate_request(&json!([1, 2])).is_err());
        assert!(Request::parse("not json").is_err());
        assert!(Request::parse("{\"id\":1,\"op\":\"ping\"}\r").is_ok());
    }

    #[test]
    fn response_envelope_shapes() {
        let r = Response::ok(Some(3), json!({"pong": true}));
        assert_eq!(
            r.to_value(),
            json!({"id": 3, "ok": true, "result": {"pong": true}})
        );
        r.validate().unwrap();
        let r = Response::ok_rev(Some(5), 42, Value::Null);
        assert_eq!(r.to_value()["rev"], 42);
        r.validate().unwrap();
        let r = Response::error_code(None, ErrorCode::ELimit, "line too long");
        assert_eq!(
            r.to_value(),
            json!({"ok": false, "error": {"code": "E_LIMIT", "message": "line too long"}})
        );
        r.validate().unwrap();
        let r = Response::error(
            Some(5),
            IpcError::new(ErrorCode::EConflict, "rev is 42").with_current_rev(42),
        );
        assert_eq!(r.to_value()["error"]["current_rev"], 42);
        r.validate().unwrap();
    }
}
