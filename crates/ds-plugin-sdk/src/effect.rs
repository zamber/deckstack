//! Effect builders (`docs/contracts/plugins.md` §8: `set`/`emit`/`log`).
//!
//! An action/event handler returns `Vec<Effect>`; the serve loop sends them as the
//! `effects` array of an `{"op":"effects",…}` message. The host validates every effect
//! against the state schema and the plugin's capabilities before touching state
//! (`docs/components/plugin-host.md` failure modes).

use serde::Serialize;
use serde_json::Value;

/// One effect. Field order matches the contract example
/// `{"effect":"set","path":"/controls/keys.0/label","value":"1"}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Effect {
    pub effect: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
}

/// `{"effect":"set","path":…,"value":…}` — set a state path (capability `state.write`).
pub fn set(path: impl Into<String>, value: impl Into<Value>) -> Effect {
    Effect {
        effect: "set",
        path: Some(path.into()),
        value: Some(value.into()),
        topic: None,
        payload: None,
        level: None,
        message: None,
        ms: None,
    }
}

/// `{"effect":"emit","topic":…,"payload":…}` — emit a bus event (capability `emit`).
pub fn emit(topic: impl Into<String>, payload: impl Into<Value>) -> Effect {
    Effect {
        effect: "emit",
        path: None,
        value: None,
        topic: Some(topic.into()),
        payload: Some(payload.into()),
        level: None,
        message: None,
        ms: None,
    }
}

/// `{"effect":"log","level":…,"message":…}` — a log line in the daemon log.
pub fn log(level: impl Into<String>, message: impl Into<String>) -> Effect {
    Effect {
        effect: "log",
        path: None,
        value: None,
        topic: None,
        payload: None,
        level: Some(level.into()),
        message: Some(message.into()),
        ms: None,
    }
}

/// `{"effect":"sleep","ms":…}` — pause the action chain for `ms` (the `sleep` step of
/// `docs/contracts/actions.md` §3, as a plugin effect).
pub fn sleep(ms: u64) -> Effect {
    Effect {
        effect: "sleep",
        path: None,
        value: None,
        topic: None,
        payload: None,
        level: None,
        message: None,
        ms: Some(ms),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builders_serialize_like_the_contract_examples() {
        assert_eq!(
            serde_json::to_string(&set("/controls/keys.0/label", "1")).unwrap(),
            r#"{"effect":"set","path":"/controls/keys.0/label","value":"1"}"#
        );
        assert_eq!(
            serde_json::to_string(&set(
                "/controls/keys.0/image",
                json!({"kind":"color","value":"#ff8800"})
            ))
            .unwrap(),
            r##"{"effect":"set","path":"/controls/keys.0/image","value":{"kind":"color","value":"#ff8800"}}"##
        );
        assert_eq!(
            serde_json::to_string(&emit("app/demo/bumped", json!({"n":1}))).unwrap(),
            r#"{"effect":"emit","topic":"app/demo/bumped","payload":{"n":1}}"#
        );
        assert_eq!(
            serde_json::to_string(&log("info", "hello")).unwrap(),
            r#"{"effect":"log","level":"info","message":"hello"}"#
        );
        assert_eq!(
            serde_json::to_string(&sleep(250)).unwrap(),
            r#"{"effect":"sleep","ms":250}"#
        );
    }
}
