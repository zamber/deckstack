//! `ds bind <topic> --do '<step>' [--do …] [--when '<cond>']` — append a binding to
//! `/bindings` and print the daemon-assigned id (`docs/contracts/cli.md` §1,
//! `docs/contracts/actions.md` §2, `docs/contracts/state.md` §2; DS-041).
//!
//! Each `--do '<step>'` is either a full JSON step object
//! (`'{"do":"sleep","ms":5}'`) or the shorthand used in the contract examples:
//!
//! ```text
//! set <path> <json-value>            → {"do":"set","path":…,"value":…}
//! emit <topic> [json-payload]        → {"do":"emit","topic":…,"payload":…}
//! plugin <id.action> [json-args]     → {"do":"plugin","id":…,"action":…,"args":…}
//! exec <argv…>                       → {"do":"exec","argv":[…]}   (one token = one arg)
//! sleep <ms>                         → {"do":"sleep","ms":…}
//! ```
//!
//! The steps are parsed and schema-validated **client-side** against the embedded
//! `schemas/binding.json` (via the IPC request schema) for fast errors — malformed JSON
//! or a code-ish step (`{"do":"eval",…}`) exits 1 with the schema error shape — and
//! re-validated daemon-side. Step **execution** is DS-042: `bind` only stores them.

use crate::cli::{Global, OutputMode};
use crate::connect::connect;
use crate::output::{print_object, Failure};
use ds_client::ClientError;
use serde_json::{json, Value};

/// The placeholder `id` sent in the bind request: the IPC schema requires the field
/// shape, the daemon always replaces it with the allocated id (`bNNNN`).
const BINDING_ID_PLACEHOLDER: &str = "b0000";

/// `ds bind <topic> --do '<step>' [--do …] [--when '<cond>']`.
pub fn verb_bind(
    g: &Global,
    topic: &str,
    dos: &[String],
    when: Option<&str>,
) -> Result<i32, Failure> {
    let mut steps = Vec::with_capacity(dos.len());
    for (i, d) in dos.iter().enumerate() {
        let step = parse_step(d)?;
        // Validate the step shape client-side against the SAME normative vocabulary the
        // daemon enforces (the embedded schemas/binding.json via the IPC request schema —
        // no second parser): a code-ish or malformed step exits 1 before connecting.
        validate_step(&step).map_err(|e| Failure::Usage(format!("--do #{}: {e}", i + 1)))?;
        steps.push(step);
    }
    let mut binding = json!({
        "id": BINDING_ID_PLACEHOLDER,
        "topic": topic,
        "steps": steps,
    });
    if let Some(w) = when {
        let cond: Value = serde_json::from_str(w)
            .map_err(|e| Failure::Usage(format!("--when is not valid JSON: {e}")))?;
        if !cond.is_object() {
            return Err(Failure::Usage(format!(
                "--when must be a JSON object like '{{\"payload\":\"app\",\"op\":\"eq\",\"value\":\"code\"}}', got {w:?}"
            )));
        }
        binding["when"] = cond;
    }
    let req = json!({"op": "bind", "binding": binding});
    // Client-side schema validation (same normative schemas the daemon uses) for fast,
    // precise errors before connecting.
    ds_ipc::message::validate_request(&req)
        .map_err(|e| Failure::Client(ClientError::Schema(e.message)))?;
    let mut c = connect(g)?;
    let resp = c.request(req)?;
    let rev = resp.get("rev").and_then(Value::as_u64).unwrap_or(0);
    let id = resp["result"]["id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    match g.mode {
        OutputMode::Json => print_object(
            &json!({"ok": true, "rev": rev, "result": {"id": id}}),
            g.compact,
        ),
        _ => println!("ok rev={rev} id={id}"),
    }
    Ok(0)
}

/// Validate one parsed step against `schemas/binding.json` — the same embedded schema the
/// daemon validates the `bind` request with (`ds_ipc::message::validate_request`), so the
/// CLI and the daemon accept exactly the same closed vocabulary
/// (set/emit/plugin/exec/sleep; no code fields). The step is wrapped in a minimal probe
/// binding; this adds no parser of its own, it reuses the normative schema.
fn validate_step(step: &Value) -> Result<(), String> {
    let probe = json!({
        "op": "bind",
        "binding": {
            "id": BINDING_ID_PLACEHOLDER,
            "topic": "deck/probe/x",
            "steps": [step],
        }
    });
    ds_ipc::message::validate_request(&probe)
        .map_err(|e| format!("step {step} violates schemas/binding.json: {}", e.message))?;
    Ok(())
}

/// Parse one `--do` step string into a step object (JSON form or shorthand).
fn parse_step(s: &str) -> Result<Value, Failure> {
    let t = s.trim();
    if t.is_empty() {
        return Err(Failure::Usage("--do needs a non-empty step".to_string()));
    }
    if t.starts_with('{') {
        return match serde_json::from_str::<Value>(t) {
            Ok(v) if v.is_object() => Ok(v),
            Ok(_) => Err(Failure::Usage(format!(
                "--do step must be a JSON object, got {s:?}"
            ))),
            Err(e) => Err(Failure::Usage(format!("--do '{s}' is not valid JSON: {e}"))),
        };
    }
    let mut it = t.split_whitespace();
    let verb = it.next().expect("non-empty checked above");
    let rest: Vec<&str> = it.collect();
    let joined = rest.join(" ");
    let step = match verb {
        "set" => {
            let Some(path) = rest.first() else {
                return Err(Failure::Usage(
                    "step 'set' needs <path> <json-value>, e.g. set /controls/keys/1/image {\"kind\":\"color\",\"value\":\"#ff0000\"}"
                        .to_string(),
                ));
            };
            let value_str = rest.get(1).map(|_| joined[path.len()..].trim_start().to_string());
            let Some(value_str) = value_str else {
                return Err(Failure::Usage(format!(
                    "step 'set {path}' needs a JSON value"
                )));
            };
            let value: Value = serde_json::from_str(&value_str).map_err(|e| {
                Failure::Usage(format!("step 'set {path}' value is not valid JSON: {e}"))
            })?;
            json!({"do": "set", "path": path, "value": value})
        }
        "emit" => {
            let Some(topic) = rest.first() else {
                return Err(Failure::Usage(
                    "step 'emit' needs <topic> [json-payload], e.g. emit app/demo/red {}".to_string(),
                ));
            };
            let payload = match rest.get(1) {
                None => json!({}),
                Some(_) => {
                    let p = joined[topic.len()..].trim_start().to_string();
                    serde_json::from_str(&p).map_err(|e| {
                        Failure::Usage(format!("step 'emit {topic}' payload is not valid JSON: {e}"))
                    })?
                }
            };
            json!({"do": "emit", "topic": topic, "payload": payload})
        }
        "plugin" => {
            let Some(spec) = rest.first() else {
                return Err(Failure::Usage(
                    "step 'plugin' needs <id.action> [json-args], e.g. plugin demo.bump {\"by\":1}"
                        .to_string(),
                ));
            };
            let Some((id, action)) = spec.split_once('.') else {
                return Err(Failure::Usage(format!(
                    "step 'plugin {spec}' needs <id.action> (dot-separated), e.g. demo.bump"
                )));
            };
            let args = match rest.get(1) {
                None => json!({}),
                Some(_) => {
                    let a = joined[spec.len()..].trim_start().to_string();
                    serde_json::from_str(&a).map_err(|e| {
                        Failure::Usage(format!(
                            "step 'plugin {spec}' args are not valid JSON: {e}"
                        ))
                    })?
                }
            };
            json!({"do": "plugin", "id": id, "action": action, "args": args})
        }
        "exec" => {
            if rest.is_empty() {
                return Err(Failure::Usage(
                    "step 'exec' needs an absolute argv[0], e.g. exec /usr/bin/ffmpeg -i v.mp4"
                        .to_string(),
                ));
            }
            json!({"do": "exec", "argv": rest})
        }
        "sleep" => {
            let Some(ms) = rest.first() else {
                return Err(Failure::Usage("step 'sleep' needs <ms>, e.g. sleep 200".to_string()));
            };
            let ms: u64 = ms.parse().map_err(|_| {
                Failure::Usage(format!("step 'sleep' needs an integer ms, got {ms:?}"))
            })?;
            json!({"do": "sleep", "ms": ms})
        }
        other => {
            return Err(Failure::Usage(format!(
                "unknown step verb '{other}' in --do '{s}' (valid: set, emit, plugin, exec, sleep — or a full JSON step object)"
            )))
        }
    };
    Ok(step)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_json_steps_verbatim() {
        assert_eq!(
            parse_step(r##"{"do":"sleep","ms":5}"##).unwrap(),
            json!({"do": "sleep", "ms": 5})
        );
        assert_eq!(
            parse_step(r##"{"do":"set","path":"/x","value":{"kind":"color","value":"#00ff00"}}"##)
                .unwrap(),
            json!({"do": "set", "path": "/x", "value": {"kind": "color", "value": "#00ff00"}})
        );
        // malformed JSON -> usage error (exit 1)
        assert!(parse_step(r##"{"do":"sleep""##).is_err());
        assert!(parse_step("42").is_err());
        assert!(parse_step("").is_err());
    }

    #[test]
    fn parses_the_contract_shorthand_steps() {
        assert_eq!(
            parse_step(r##"plugin demo.bump {"by":1}"##).unwrap(),
            json!({"do": "plugin", "id": "demo", "action": "bump", "args": {"by": 1}})
        );
        assert_eq!(
            parse_step(r##"set /controls/keys/1/image {"kind":"color","value":"#ff0000"}"##)
                .unwrap(),
            json!({"do": "set", "path": "/controls/keys/1/image",
                   "value": {"kind": "color", "value": "#ff0000"}})
        );
        // JSON values may contain spaces (the rest of the line is the value)
        assert_eq!(
            parse_step(r##"set /settings/note {"text": "a b"}"##).unwrap(),
            json!({"do": "set", "path": "/settings/note", "value": {"text": "a b"}})
        );
        assert_eq!(
            parse_step(r##"emit app/demo/red {}"##).unwrap(),
            json!({"do": "emit", "topic": "app/demo/red", "payload": {}})
        );
        assert_eq!(
            parse_step(r##"emit app/label "Hi there""##).unwrap(),
            json!({"do": "emit", "topic": "app/label", "payload": "Hi there"})
        );
        assert_eq!(
            parse_step("sleep 200").unwrap(),
            json!({"do": "sleep", "ms": 200})
        );
        assert_eq!(
            parse_step("exec /usr/bin/ffmpeg -i v.mp4").unwrap(),
            json!({"do": "exec", "argv": ["/usr/bin/ffmpeg", "-i", "v.mp4"]})
        );
        // errors
        assert!(parse_step("eval x").is_err());
        assert!(parse_step("set /onlypath").is_err());
        assert!(parse_step("plugin noDot").is_err());
        assert!(parse_step("sleep abc").is_err());
        assert!(parse_step("exec").is_err());
        assert!(parse_step("emit").is_err());
    }

    #[test]
    fn validate_step_uses_the_normative_binding_schema() {
        // every vocabulary step validates
        assert!(validate_step(&json!({"do": "sleep", "ms": 5})).is_ok());
        assert!(
            validate_step(&json!({"do": "set", "path": "/controls/keys/1/image",
            "value": {"kind": "color", "value": "#ff0000"}}))
            .is_ok()
        );
        assert!(validate_step(&json!({"do": "emit", "topic": "app/demo/x"})).is_ok());
        assert!(validate_step(&json!({"do": "plugin", "id": "demo", "action": "bump"})).is_ok());
        assert!(validate_step(&json!({"do": "exec", "argv": ["/usr/bin/true"]})).is_ok());
        // code-ish step rejected (R8/T2) with the schema in the message
        let err = validate_step(&json!({"do": "eval", "code": "1"})).unwrap_err();
        assert!(err.contains("binding.json"), "{err}");
        // exec with a relative argv[0] rejected (M1/S3)
        assert!(validate_step(&json!({"do": "exec", "argv": ["sh", "-c", "x"]})).is_err());
        // sleep bounds (schemas/binding.json: 1..=60000)
        assert!(validate_step(&json!({"do": "sleep", "ms": 0})).is_err());
        assert!(validate_step(&json!({"do": "sleep", "ms": 60_001})).is_err());
        // unknown verb / non-object
        assert!(validate_step(&json!({"do": "restart"})).is_err());
        assert!(validate_step(&json!(42)).is_err());
    }
}
