//! `when` condition evaluation (ticket DS-042; contract `docs/contracts/actions.md` §2,
//! component `docs/components/event-router.md` "Routing model").
//!
//! A stored `when` has exactly one operand selector — `path` (a state path, read through
//! the state task at event time) or `payload` (a field of the triggering event's
//! payload, S7) — plus an `op` and, for every op except `exists`, a `value`. The storable
//! shape is normative in `schemas/binding.json` (DS-041 validates it verbatim); this
//! module is the **evaluation** half.
//!
//! Semantics (deliberately boring, never panicking):
//!
//! - **missing operand** (unknown state path, absent payload field, or a malformed
//!   `when` with neither/both of `path`/`payload`) → the condition does **not** fire:
//!   `exists` is `false` and every other op is `false` as well (SQL-NULL-like: there is
//!   nothing to compare, so the binding simply does not match — actions.md §2 "a stored
//!   `when` that does not match simply does not fire");
//! - `exists` → the operand is present (`true`) or missing (`false`);
//! - `eq` / `ne` (`neq`) → deep JSON equality of operand and `value` (type-sensitive:
//!   `1` ≠ `"1"`, `true` ≠ `1`);
//! - `gt` / `lt` / `geq` / `leq` → numeric comparison only (`as_f64`); if either side is
//!   not a JSON number the condition is `false` — **type mismatches never panic** and
//!   never fire;
//! - an unknown `op` (cannot be stored via `bind`, but a hand-crafted document could
//!   carry one) → `false`.
//!
//! The evaluator understands the full comparison vocabulary `exists, eq, ne, neq, gt,
//! lt, geq, leq` (`ne` is an alias of `neq`); the subset `schemas/binding.json` accepts at
//! bind time is the normative storable set.

use serde_json::Value;

/// Every operator the evaluator understands (`ne` is an alias of `neq`).
pub const OPS: [&str; 8] = ["exists", "eq", "ne", "neq", "gt", "lt", "geq", "leq"];

/// Evaluate one stored `when` condition.
///
/// - `when` — the stored condition object (`{"path"|"payload": …, "op": …, "value"?}`);
/// - `state_get` — state-path lookup at event time: `Some(value)` when the path exists,
///   `None` when it is missing (the caller maps an `E_PATH` read to `None`);
/// - `payload` — the triggering event's payload.
///
/// Returns whether the binding fires. Never panics on any input shape.
pub fn eval_when(when: &Value, state_get: impl Fn(&str) -> Option<Value>, payload: &Value) -> bool {
    let Some(obj) = when.as_object() else {
        return false;
    };
    let Some(op) = obj.get("op").and_then(Value::as_str) else {
        return false;
    };
    if !OPS.contains(&op) {
        return false;
    }
    // Exactly one operand selector: `path` (state) or `payload` (event field, S7).
    let has_path = obj.contains_key("path");
    let has_payload = obj.contains_key("payload");
    let operand: Option<Value> = match (has_path, has_payload) {
        (true, false) => obj.get("path").and_then(Value::as_str).and_then(state_get),
        (false, true) => obj
            .get("payload")
            .and_then(Value::as_str)
            .and_then(|field| payload.get(field).cloned()),
        _ => None, // neither or both: malformed -> does not fire
    };
    let Some(operand) = operand else {
        // Missing operand: `exists` is false and so is every comparison (no data to
        // compare against — the binding does not fire).
        return false;
    };
    match op {
        "exists" => true,
        "eq" => obj.get("value").is_some_and(|v| &operand == v),
        "ne" | "neq" => obj.get("value").is_some_and(|v| &operand != v),
        "gt" | "lt" | "geq" | "leq" => {
            let Some(value) = obj.get("value") else {
                return false;
            };
            compare(&operand, value, op)
        }
        _ => false,
    }
}

/// Numeric comparison for `gt`/`lt`/`geq`/`leq`. Non-numeric operands or values make the
/// condition `false` (type mismatch never panics, never fires).
fn compare(operand: &Value, value: &Value, op: &str) -> bool {
    let (Some(a), Some(b)) = (operand.as_f64(), value.as_f64()) else {
        return false;
    };
    match op {
        "gt" => a > b,
        "lt" => a < b,
        "geq" => a >= b,
        "leq" => a <= b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    /// A tiny in-memory "state document" for path lookups (JSON-pointer-ish, flat map).
    fn doc_get(doc: &HashMap<String, Value>) -> impl Fn(&str) -> Option<Value> + '_ {
        move |path: &str| doc.get(path).cloned()
    }

    fn state(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn exists_on_state_path_and_payload() {
        let doc = state(&[("/focus/app", json!("code")), ("/n", json!(0))]);
        let g = doc_get(&doc);
        // present path / payload field -> exists true
        assert!(eval_when(
            &json!({"path": "/focus/app", "op": "exists"}),
            &g,
            &json!({})
        ));
        assert!(eval_when(
            &json!({"payload": "app", "op": "exists"}),
            &g,
            &json!({"app": "code"})
        ));
        // missing path -> exists:false (event-router.md failure modes)
        assert!(!eval_when(
            &json!({"path": "/nope", "op": "exists"}),
            &g,
            &json!({})
        ));
        // missing payload field -> exists:false
        assert!(!eval_when(
            &json!({"payload": "app", "op": "exists"}),
            &g,
            &json!({"x": 1})
        ));
        // a stored JSON null still counts as present
        let doc2 = state(&[("/nullpath", Value::Null)]);
        let g2 = doc_get(&doc2);
        assert!(eval_when(
            &json!({"path": "/nullpath", "op": "exists"}),
            &g2,
            &json!({})
        ));
    }

    #[test]
    fn eq_and_ne_on_payload_and_path() {
        let doc = state(&[("/controls/keys/0/brightness", json!(80))]);
        let g = doc_get(&doc);
        let payload = json!({"app": "code", "pid": 7, "pressed": true});
        // eq on payload field (S7)
        assert!(eval_when(
            &json!({"payload": "app", "op": "eq", "value": "code"}),
            &g,
            &payload
        ));
        assert!(!eval_when(
            &json!({"payload": "app", "op": "eq", "value": "terminal"}),
            &g,
            &payload
        ));
        // eq on state path
        assert!(eval_when(
            &json!({"path": "/controls/keys/0/brightness", "op": "eq", "value": 80}),
            &g,
            &json!({})
        ));
        // ne / neq aliases
        assert!(eval_when(
            &json!({"payload": "app", "op": "ne", "value": "terminal"}),
            &g,
            &payload
        ));
        assert!(eval_when(
            &json!({"payload": "app", "op": "neq", "value": "terminal"}),
            &g,
            &payload
        ));
        assert!(!eval_when(
            &json!({"payload": "app", "op": "ne", "value": "code"}),
            &g,
            &payload
        ));
        // type mismatches: eq false, ne true — never panic
        assert!(!eval_when(
            &json!({"payload": "pid", "op": "eq", "value": "7"}),
            &g,
            &payload
        ));
        assert!(eval_when(
            &json!({"payload": "pid", "op": "ne", "value": "7"}),
            &g,
            &payload
        ));
        assert!(!eval_when(
            &json!({"payload": "pressed", "op": "eq", "value": 1}),
            &g,
            &payload
        ));
    }

    #[test]
    fn numeric_ops_gt_lt_geq_leq() {
        let doc = state(&[("/level", json!(5))]);
        let g = doc_get(&doc);
        let payload = json!({"delta": -2, "value": 10});
        for (op, want) in [("gt", true), ("lt", false), ("geq", true), ("leq", false)] {
            assert_eq!(
                eval_when(
                    &json!({"payload": "value", "op": op, "value": 5}),
                    &g,
                    &payload
                ),
                want,
                "op {op}"
            );
        }
        assert!(eval_when(
            &json!({"payload": "delta", "op": "lt", "value": 0}),
            &g,
            &payload
        ));
        assert!(eval_when(
            &json!({"path": "/level", "op": "geq", "value": 5}),
            &g,
            &json!({})
        ));
        assert!(eval_when(
            &json!({"path": "/level", "op": "leq", "value": 5}),
            &g,
            &json!({})
        ));
        // float vs int compare numerically
        assert!(eval_when(
            &json!({"path": "/level", "op": "gt", "value": 4.5}),
            &g,
            &json!({})
        ));
    }

    #[test]
    fn missing_operand_never_fires_for_any_op() {
        let doc = state(&[]);
        let g = doc_get(&doc);
        let payload = json!({"app": "code"});
        // missing path -> exists:false, and comparisons do not fire either
        for op in ["exists", "eq", "ne", "neq", "gt", "lt", "geq", "leq"] {
            let when = if op == "exists" {
                json!({"path": "/missing", "op": op})
            } else {
                json!({"path": "/missing", "op": op, "value": 1})
            };
            assert!(
                !eval_when(&when, &g, &payload),
                "op {op} on a missing path must not fire"
            );
        }
        // missing payload field, same rule
        for op in ["exists", "eq", "ne", "gt"] {
            let when = if op == "exists" {
                json!({"payload": "absent", "op": op})
            } else {
                json!({"payload": "absent", "op": op, "value": 1})
            };
            assert!(
                !eval_when(&when, &g, &payload),
                "op {op} on an absent payload field"
            );
        }
    }

    #[test]
    fn type_mismatches_never_panic_and_never_fire_comparisons() {
        let doc = state(&[
            ("/s", json!("text")),
            ("/b", json!(true)),
            ("/o", json!({"a": 1})),
        ]);
        let g = doc_get(&doc);
        // gt/lt on strings/bools/objects -> false (numeric-only comparison)
        for path in ["/s", "/b", "/o"] {
            for op in ["gt", "lt", "geq", "leq"] {
                let when = json!({"path": path, "op": op, "value": 3});
                assert!(!eval_when(&when, &g, &json!({})), "{path} {op}");
            }
        }
        // comparison op against a non-numeric `value` -> false
        assert!(!eval_when(
            &json!({"path": "/s", "op": "gt", "value": "zzz"}),
            &g,
            &json!({})
        ));
        // eq across types -> false, ne -> true
        assert!(!eval_when(
            &json!({"path": "/b", "op": "eq", "value": 1}),
            &g,
            &json!({})
        ));
        assert!(eval_when(
            &json!({"path": "/b", "op": "ne", "value": 1}),
            &g,
            &json!({})
        ));
        // deeply nested values compare by value
        assert!(eval_when(
            &json!({"path": "/o", "op": "eq", "value": {"a": 1}}),
            &g,
            &json!({})
        ));
    }

    #[test]
    fn malformed_when_shapes_do_not_fire() {
        let doc = state(&[("/x", json!(1))]);
        let g = doc_get(&doc);
        let payload = json!({"app": "code"});
        // not an object
        assert!(!eval_when(&json!(42), &g, &payload));
        assert!(!eval_when(&json!("eq"), &g, &payload));
        // no op
        assert!(!eval_when(&json!({"payload": "app"}), &g, &payload));
        // unknown op (unrepresentable via bind, but never panic)
        assert!(!eval_when(
            &json!({"payload": "app", "op": "regex", "value": ".*"}),
            &g,
            &payload
        ));
        // both path and payload -> malformed -> false
        assert!(!eval_when(
            &json!({"path": "/x", "payload": "app", "op": "eq", "value": "code"}),
            &g,
            &payload
        ));
        // neither path nor payload -> false
        assert!(!eval_when(&json!({"op": "eq", "value": 1}), &g, &payload));
        // comparison op without a value -> false (schema forbids storing this)
        assert!(!eval_when(
            &json!({"payload": "app", "op": "eq"}),
            &g,
            &payload
        ));
        // non-string path/payload selectors -> false
        assert!(!eval_when(
            &json!({"path": 5, "op": "exists"}),
            &g,
            &payload
        ));
        assert!(!eval_when(
            &json!({"payload": 5, "op": "exists"}),
            &g,
            &payload
        ));
    }

    #[test]
    fn payload_operand_reads_nested_fields_shallowly_only() {
        // `payload` selects a top-level field of the event payload (S7), e.g. "app"
        let empty = state(&[]);
        let g = doc_get(&empty);
        let payload = json!({"focus": {"app": "code"}});
        // top-level field present
        assert!(eval_when(
            &json!({"payload": "focus", "op": "exists"}),
            &g,
            &payload
        ));
        // dotted names are a single field name, not a path into the payload
        assert!(!eval_when(
            &json!({"payload": "focus.app", "op": "exists"}),
            &g,
            &payload
        ));
    }
}
