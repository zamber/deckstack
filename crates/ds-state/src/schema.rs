//! Schema validation of the state document against `schemas/state.json`.
//!
//! The schemas are embedded at compile time (`include_str!`), so the crate, the fixture
//! gate (`tests/fixtures/validate.sh`) and the daemon all validate against the exact same
//! normative files — no network, no drift. External `$ref`s
//! (`https://deckstack.dev/schemas/binding.json/v1`, `.../plugin-manifest.json/v1`) are
//! served by a local retriever from the embedded copies in `schemas/`.

use crate::error::{Result, StateError};
use jsonschema::{Draft, Retrieve, Uri, Validator};
use serde_json::Value;
use std::sync::OnceLock;

/// `schemas/state.json`, embedded at compile time.
pub const STATE_SCHEMA_JSON: &str = include_str!("../../../schemas/state.json");
/// `schemas/binding.json`, embedded (referenced by `state.json` `$ref`).
pub const BINDING_SCHEMA_JSON: &str = include_str!("../../../schemas/binding.json");
/// `schemas/plugin-manifest.json`, embedded (referenced by `state.json` `$ref`).
pub const PLUGIN_MANIFEST_SCHEMA_JSON: &str = include_str!("../../../schemas/plugin-manifest.json");

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
            "https://deckstack.dev/schemas/plugin-manifest.json/v1" => {
                Ok(serde_json::from_str(PLUGIN_MANIFEST_SCHEMA_JSON)?)
            }
            other => Err(format!("unknown schema reference: {other}").into()),
        }
    }
}

/// The compiled `schemas/state.json` validator (draft 2020-12, `format` asserted).
pub fn state_validator() -> &'static Validator {
    static VALIDATOR: OnceLock<Validator> = OnceLock::new();
    VALIDATOR.get_or_init(|| {
        let schema: Value =
            serde_json::from_str(STATE_SCHEMA_JSON).expect("embedded schemas/state.json parses");
        jsonschema::options()
            .with_draft(Draft::Draft202012)
            .with_retriever(LocalRetriever)
            .should_validate_formats(true)
            .build(&schema)
            .expect("schemas/state.json compiles")
    })
}

/// Validate a whole state document against `schemas/state.json`.
///
/// Called on every load and after every (candidate) mutation; failures are `E_SCHEMA`.
pub fn validate_document(doc: &Value) -> Result<()> {
    if let Err(err) = state_validator().validate(doc) {
        return Err(StateError::Schema(format!("{err}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_document_is_valid() {
        let doc = json!({
            "meta": {"version": 1, "rev": 0, "protocol": 1},
            "devices": {},
            "controls": {},
            "bindings": [],
            "plugins": {},
            "streams": {},
            "settings": {},
            "policy": {},
            "telemetry": {}
        });
        validate_document(&doc).unwrap();
    }

    #[test]
    fn bad_documents_are_rejected() {
        let base = json!({
            "meta": {"version": 1, "rev": 0, "protocol": 1},
            "devices": {}, "controls": {}, "bindings": [], "plugins": {},
            "streams": {}, "settings": {}, "policy": {}, "telemetry": {}
        });
        // missing required top-level key
        let mut bad = base.clone();
        bad.as_object_mut().unwrap().remove("policy");
        assert!(validate_document(&bad).is_err());
        // additional top-level property (root is closed)
        let mut bad = base.clone();
        bad.as_object_mut()
            .unwrap()
            .insert("bogus".into(), json!({}));
        assert!(validate_document(&bad).is_err());
        // meta.version is const 1
        let mut bad = base.clone();
        bad["meta"]["version"] = json!(2);
        assert!(validate_document(&bad).is_err());
        // bad color pattern
        let mut bad = base.clone();
        bad["controls"] = json!({"keys": {"0": {"image": {"kind": "color", "value": "red"}}}});
        assert!(validate_document(&bad).is_err());
        // file image with '..' segment (M3)
        let mut bad = base.clone();
        bad["controls"] =
            json!({"keys": {"0": {"image": {"kind": "file", "path": "/a/../../etc/x"}}}});
        assert!(validate_document(&bad).is_err());
        // bad binding id (S2) — external $ref to binding.json
        let mut bad = base.clone();
        bad["bindings"] =
            json!([{"id": "x1", "topic": "deck/x", "steps": [{"do": "sleep", "ms": 5}]}]);
        assert!(validate_document(&bad).is_err());
        // queue_depth out of range
        let mut bad = base.clone();
        bad["streams"] = json!({"keys": {"queue_depth": 3}});
        assert!(validate_document(&bad).is_err());
        // bad telemetry sha256
        let mut bad = base.clone();
        bad["telemetry"] = json!({"streams": {"keys": {"last_sha256": "xyz"}}});
        assert!(validate_document(&bad).is_err());
        // date-time format asserted
        let mut bad = base.clone();
        bad["meta"]["started_at"] = json!("not a date");
        assert!(validate_document(&bad).is_err());
    }

    #[test]
    fn filled_document_is_valid() {
        let sha = "a".repeat(64);
        let doc = json!({
            "meta": {"version": 1, "rev": 42, "protocol": 1, "daemon_pid": 7, "started_at": "2026-11-22T10:00:00Z"},
            "devices": {"ABC123": {"kind": "keys", "model": "M4315", "pid": 510, "slot": "1-4-3", "connection": "connected", "last_seen_rev": 40}},
            "controls": {
                "keys": {"0": {"image": {"kind": "color", "value": "#3cc86e"}, "label": "Salon", "brightness": 80}},
                "dial": {"image": {"kind": "frame", "target": "dial", "seq": 7, "sha256": sha}},
                "knobs": {"0": {"led": {"mode": "static", "color": "#3cc86e", "brightness": 40}, "value": 10}},
                "faders": {}, "rollers": {}
            },
            "bindings": [{"id": "b0001", "topic": "deck/control/keys.0/down", "allow_exec": false,
                          "steps": [{"do": "plugin", "id": "demo", "action": "bump", "args": {"by": 1}}]}],
            "plugins": {"demo": {"status": "running", "stats": {"restarts": 0, "violations": 0, "invokes": 3}}},
            "streams": {"keys": {"queue_depth": 2}},
            "settings": {"log": {"level": "info"}},
            "policy": {"net": {"allow_hosts": ["lights.lan"]}, "exec": {"allow": ["/usr/bin/ffmpeg"], "deny": ["/bin/sh"]}, "led_rate_per_s": 30},
            "telemetry": {"streams": {"keys": {"sent": 960, "dropped": 12, "fps": 32.0, "in_flight": true, "last_seq": 959}}},
            "focus": {"app": "code", "title": "PLAN.md", "pid": 123, "at_rev": 42}
        });
        validate_document(&doc).unwrap();
    }
}
