//! `ds unbind <id> | ds unbind --topic <topic>` — remove bindings from `/bindings`
//! (`docs/contracts/cli.md` §1, `docs/contracts/state.md` §2; DS-041).
//!
//! One transaction per invocation (rev +1, one `/bindings` event). Unknown id (or a
//! topic with no binding) answers `E_PATH` → exit 2. Success prints the removed ids:
//! human `ok rev=N removed=b0001`, `--json` `{"ok":true,"rev":N,"result":{"removed":["b0001"]}}`.

use crate::cli::{Global, OutputMode};
use crate::connect::connect;
use crate::output::{print_object, Failure};
use ds_client::ClientError;
use serde_json::{json, Value};

/// Whether `s` looks like a binding id (`b` + at least 4 digits).
fn is_binding_id(s: &str) -> bool {
    s.len() >= 5 && s.starts_with('b') && s[1..].chars().all(|c| c.is_ascii_digit())
}

/// `ds unbind <id>` / `ds unbind --topic <topic>`.
pub fn verb_unbind(g: &Global, id: Option<&str>, topic: Option<&str>) -> Result<i32, Failure> {
    let mut req = json!({"op": "unbind"});
    if let Some(b) = id {
        if !is_binding_id(b) {
            return Err(Failure::Usage(format!(
                "bad binding id {b:?} (expected bNNNN, e.g. b0001)"
            )));
        }
        req["binding"] = json!(b);
    }
    if let Some(t) = topic {
        req["topic"] = json!(t);
    }
    // Client-side schema validation (same normative schemas the daemon uses).
    ds_ipc::message::validate_request(&req)
        .map_err(|e| Failure::Client(ClientError::Schema(e.message)))?;
    let mut c = connect(g)?;
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
    match g.mode {
        OutputMode::Json => print_object(
            &json!({"ok": true, "rev": rev, "result": {"removed": removed}}),
            g.compact,
        ),
        _ => println!("ok rev={rev} removed={}", removed.join(",")),
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_id_format() {
        assert!(is_binding_id("b0001"));
        assert!(is_binding_id("b99999"));
        assert!(!is_binding_id("b123"));
        assert!(!is_binding_id("x0001"));
        assert!(!is_binding_id("b000a"));
        assert!(!is_binding_id(""));
    }
}
