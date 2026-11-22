//! Bindings as data: the `/bindings` **array** of the state document
//! (`schemas/state.json` §bindings, `schemas/binding.json`, `docs/contracts/state.md` §2,
//! `docs/contracts/actions.md` §2, S2).
//!
//! - Each element is a `schemas/binding.json` object with a **daemon-assigned** id
//!   `bNNNN` (`b%04d` of a monotonic counter). Ids are allocated on add and **never
//!   reused** within a daemon run: the high-water counter lives in the caller (the
//!   daemon's state task) and is never lowered by an unbind.
//! - `add` appends and returns the new id; `remove_by_id` / `remove_by_topic` delete.
//!   All of them are **one transaction each** (rev bumps by exactly 1) and therefore one
//!   subscription event on `/bindings` per committed change.
//! - Atomicity without touching the store internals: read `/bindings` + current rev,
//!   then commit with `if_rev`; a concurrent writer makes the commit fail with
//!   `E_CONFLICT` and the loop retries on the fresh document. Two concurrent `add`s can
//!   therefore never allocate the same id.
//! - Validation is two-layered: the whole document is schema-validated on every commit
//!   (`state.json` `$ref`s `binding.json`), and [`validate_binding`] additionally enforces
//!   the topic segment grammar and the `when` rules in code with precise error messages
//!   (the same corpus is used client-side by `ds bind` and daemon-side by the `bind` op).
//!
//! Step **execution** is DS-042 — this module only stores and validates the step shapes.

use crate::error::{Result, StateError};
use crate::schema::BINDING_SCHEMA_JSON;
use crate::store::{Op, Store, Transaction};
use jsonschema::{Draft, Validator};
use serde_json::{json, Value};
use std::sync::OnceLock;

/// The placeholder `id` a `bind` request carries (the IPC schema requires the field
/// shape; the daemon always replaces it with the allocated id — see
/// `docs/contracts/actions.md` §2).
pub const BINDING_ID_PLACEHOLDER: &str = "b0000";

/// Maximum length of a whole topic string (mirrors `router::topics::MAX_TOPIC_LEN`).
pub const MAX_TOPIC_LEN: usize = 200;
/// Maximum number of segments (`.`- and `/`-separated) in a topic (mirrors
/// `router::topics::MAX_SEGMENTS`).
pub const MAX_TOPIC_SEGMENTS: usize = 16;
/// How often `add`/`remove_*` retry after an `if_rev` conflict before giving up.
const MAX_COMMIT_RETRIES: u32 = 16;

/// The compiled `schemas/binding.json` validator (draft 2020-12, `format` asserted).
pub fn binding_validator() -> &'static Validator {
    static VALIDATOR: OnceLock<Validator> = OnceLock::new();
    VALIDATOR.get_or_init(|| {
        let schema: Value = serde_json::from_str(BINDING_SCHEMA_JSON)
            .expect("embedded schemas/binding.json parses");
        jsonschema::options()
            .with_draft(Draft::Draft202012)
            .should_validate_formats(true)
            .build(&schema)
            .expect("schemas/binding.json compiles")
    })
}

/// Whether `s` is a binding id: `b` followed by at least 4 digits (`^b[0-9]{4,}$`).
pub fn is_binding_id(s: &str) -> bool {
    s.len() >= 5 && s.starts_with('b') && s[1..].chars().all(|c| c.is_ascii_digit())
}

fn schema_err(msg: String) -> StateError {
    StateError::Schema(msg)
}

fn path_err(msg: String) -> StateError {
    StateError::Path(msg)
}

fn internal(msg: String) -> StateError {
    StateError::Internal(msg)
}

/// Validate a whole binding object against `schemas/binding.json` **plus** the code-level
/// topic grammar and `when` rules. `b` must be a complete binding (with `id`) — the
/// stored shape; for a request-shaped binding, inject [`BINDING_ID_PLACEHOLDER`] as `id`
/// first.
pub fn validate_binding(b: &Value) -> Result<()> {
    if !b.is_object() {
        return Err(schema_err(format!("binding must be an object, got {b}")));
    }
    if let Err(err) = binding_validator().validate(b) {
        return Err(schema_err(format!(
            "binding violates schemas/binding.json: {err}"
        )));
    }
    let topic = b
        .get("topic")
        .and_then(Value::as_str)
        .ok_or_else(|| schema_err("binding has no topic".to_string()))?;
    validate_binding_topic(topic)?;
    if let Some(when) = b.get("when") {
        validate_when(when)?;
    }
    Ok(())
}

/// Validate a binding **topic pattern** (actions.md §1 grammar, reused): root `deck` or
/// `app`, segments separated by `/` **or** `.`, each segment `[A-Za-z0-9_-]+` or a
/// whole-segment `*` wildcard, at most [`MAX_TOPIC_SEGMENTS`] segments and
/// [`MAX_TOPIC_LEN`] chars. The schema pattern is the coarse net; this is the grammar.
pub fn validate_binding_topic(topic: &str) -> Result<()> {
    if topic.is_empty() || topic.len() > MAX_TOPIC_LEN {
        return Err(schema_err(format!(
            "topic length must be 1..={MAX_TOPIC_LEN}: {topic:?}"
        )));
    }
    if !topic.starts_with("deck/") && !topic.starts_with("app/") {
        return Err(schema_err(format!(
            "topic must start with 'deck/' or 'app/': {topic:?}"
        )));
    }
    let mut segs = 0usize;
    for raw in topic.split('/') {
        if raw.is_empty() {
            return Err(schema_err(format!("empty topic segment in {topic:?}")));
        }
        for part in raw.split('.') {
            if part.is_empty() {
                return Err(schema_err(format!("empty topic sub-segment in {topic:?}")));
            }
            if part != "*"
                && !part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return Err(schema_err(format!(
                    "illegal character in topic segment {part:?} of {topic:?} ('*' is allowed only as a whole segment)"
                )));
            }
            segs += 1;
        }
    }
    if segs > MAX_TOPIC_SEGMENTS {
        return Err(schema_err(format!(
            "more than {MAX_TOPIC_SEGMENTS} topic segments in {topic:?}"
        )));
    }
    Ok(())
}

/// Validate a `when` condition beyond the schema: exactly one of `path`/`payload`, and
/// `value` present for every op except `exists` (and absent for `exists`).
pub fn validate_when(when: &Value) -> Result<()> {
    if !when.is_object() {
        return Err(schema_err(format!("'when' must be an object, got {when}")));
    }
    let has_path = when.get("path").is_some();
    let has_payload = when.get("payload").is_some();
    if has_path == has_payload {
        return Err(schema_err(
            "'when' needs exactly one of 'path' or 'payload'".to_string(),
        ));
    }
    let op = when
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| schema_err("'when' has no op".to_string()))?;
    const OPS: [&str; 5] = ["exists", "eq", "neq", "gt", "lt"];
    if !OPS.contains(&op) {
        return Err(schema_err(format!(
            "'when.op' must be one of {OPS:?}, got {op:?}"
        )));
    }
    let has_value = when.get("value").is_some();
    if op == "exists" && has_value {
        return Err(schema_err(
            "'when' with op 'exists' takes no 'value'".to_string(),
        ));
    }
    if op != "exists" && !has_value {
        return Err(schema_err(format!(
            "'when' with op {op:?} requires 'value'"
        )));
    }
    Ok(())
}

/// The numeric part of a binding id (`b0007` → 7), if it parses.
fn id_number(id: &str) -> Option<u64> {
    id.strip_prefix('b')?.parse::<u64>().ok()
}

/// Allocate the next binding id from the high-water counter `seq` and the current
/// `/bindings` array: `seq` is raised to at least the maximum id present, then
/// incremented, and the id is `b%04d`. Ids are therefore monotonic and never reused
/// while the caller keeps `seq` alive (the daemon's state task does, across unbinds).
pub fn allocate_id(seq: &mut u64, bindings: &Value) -> String {
    let mut max = *seq;
    if let Some(arr) = bindings.as_array() {
        for b in arr {
            if let Some(id) = b.get("id").and_then(Value::as_str) {
                if let Some(n) = id_number(id) {
                    max = max.max(n);
                }
            }
        }
    }
    *seq = max + 1;
    format!("b{:04}", *seq)
}

/// Read `/bindings` (array) + current rev from the store.
fn bindings_and_rev(store: &Store) -> Result<(u64, Vec<Value>)> {
    let reply = store.get("/bindings")?;
    let rev = reply
        .rev
        .ok_or_else(|| internal("/bindings unexpectedly carries no rev".to_string()))?;
    let arr = reply
        .value
        .as_array()
        .cloned()
        .ok_or_else(|| internal("/bindings is not an array".to_string()))?;
    Ok((rev, arr))
}

/// One commit attempt: replace `/bindings` with `new_arr`, guarded by `if_rev`. A
/// concurrent writer fails this with `E_CONFLICT` and **nothing is applied** — the
/// caller retries with a freshly computed array (see [`add`]/[`remove_matching`]).
fn commit_bindings_once(store: &Store, if_rev: u64, new_arr: Vec<Value>) -> Result<u64> {
    let tx =
        Transaction::new(vec![Op::set("/bindings", Value::Array(new_arr))]).with_if_rev(if_rev);
    Ok(store.commit(&tx)?.rev)
}

/// Append a binding to `/bindings` in **one transaction** (rev +1, one `/bindings`
/// event). The request-shaped `binding` may carry any id (e.g.
/// [`BINDING_ID_PLACEHOLDER`]) — it is ignored and replaced by the allocated id.
/// Returns `(id, rev)` of the committed binding.
///
/// Atomicity: the array is read together with the current rev and committed with
/// `if_rev`; on a conflict the loop recomputes from the fresh document, so two
/// concurrent `add`s can never allocate the same id or drop each other's binding.
pub fn add(store: &Store, seq: &mut u64, binding: &Value) -> Result<(String, u64)> {
    if !binding.is_object() {
        return Err(schema_err(format!(
            "binding must be an object, got {binding}"
        )));
    }
    // Shape-check the request (with the placeholder id) for a precise, early error.
    let mut cand = binding.clone();
    cand.as_object_mut()
        .expect("checked above")
        .insert("id".to_string(), json!(BINDING_ID_PLACEHOLDER));
    validate_binding(&cand)?;
    let mut attempts = 0;
    loop {
        let (rev, arr) = bindings_and_rev(store)?;
        let id = allocate_id(seq, &Value::Array(arr.clone()));
        let mut stored = binding.clone();
        stored
            .as_object_mut()
            .expect("checked above")
            .insert("id".to_string(), json!(id));
        validate_binding(&stored)?;
        let mut new_arr = arr;
        new_arr.push(stored);
        match commit_bindings_once(store, rev, new_arr) {
            Ok(new_rev) => return Ok((id, new_rev)),
            Err(e @ StateError::Conflict { .. }) => {
                attempts += 1;
                if attempts >= MAX_COMMIT_RETRIES {
                    return Err(e);
                }
                continue; // retry on the fresh document
            }
            Err(e) => return Err(e),
        }
    }
}

/// Remove every binding for which `keep` returns false; returns the removed ids and the
/// new rev. When nothing matches, the result is `E_PATH` (unknown id / topic).
fn remove_matching(
    store: &Store,
    keep: impl Fn(&Value) -> bool,
    what: &str,
) -> Result<(Vec<String>, u64)> {
    let mut attempts = 0;
    loop {
        let (rev, arr) = bindings_and_rev(store)?;
        let removed: Vec<String> = arr
            .iter()
            .filter(|b| !keep(b))
            .filter_map(|b| b.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        if removed.is_empty() {
            return Err(path_err(format!("no binding {what}")));
        }
        let new_arr: Vec<Value> = arr.into_iter().filter(|b| keep(b)).collect();
        match commit_bindings_once(store, rev, new_arr) {
            Ok(new_rev) => return Ok((removed, new_rev)),
            Err(e @ StateError::Conflict { .. }) => {
                attempts += 1;
                if attempts >= MAX_COMMIT_RETRIES {
                    return Err(e);
                }
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

/// `unbind <id>`: remove the binding with this id. Unknown (or malformed) id → `E_PATH`.
pub fn remove_by_id(store: &Store, id: &str) -> Result<(Vec<String>, u64)> {
    if !is_binding_id(id) {
        return Err(path_err(format!(
            "bad binding id {id:?} (expected bNNNN, e.g. b0001)"
        )));
    }
    let id = id.to_string();
    remove_matching(
        store,
        |b| b.get("id").and_then(Value::as_str) != Some(id.as_str()),
        &format!("with id '{id}'"),
    )
}

/// `unbind --topic <topic>`: remove **all** bindings with exactly this topic. No match →
/// `E_PATH`.
pub fn remove_by_topic(store: &Store, topic: &str) -> Result<(Vec<String>, u64)> {
    validate_binding_topic(topic)?;
    let topic = topic.to_string();
    remove_matching(
        store,
        |b| b.get("topic").and_then(Value::as_str) != Some(topic.as_str()),
        &format!("with topic '{topic}'"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    fn step_set() -> Value {
        json!({"do": "set", "path": "/controls/keys/3/image", "value": {"kind": "color", "value": "#00ff00"}})
    }

    fn binding(topic: &str) -> Value {
        json!({"id": BINDING_ID_PLACEHOLDER, "topic": topic, "steps": [step_set()]})
    }

    fn bindings_of(store: &Store) -> Value {
        store.get("/bindings").unwrap().value
    }

    // ---- id allocation -----------------------------------------------------

    #[test]
    fn ids_are_b0001_b0002_and_never_reused_after_unbind() {
        let store = Store::fresh();
        let mut seq = 0u64;
        let (id1, rev1) = add(&store, &mut seq, &binding("deck/control/keys.0/down")).unwrap();
        assert_eq!(id1, "b0001");
        assert_eq!(rev1, 1, "one add = exactly one rev bump");
        let (id2, rev2) = add(&store, &mut seq, &binding("deck/control/keys.1/down")).unwrap();
        assert_eq!(id2, "b0002");
        assert_eq!(rev2, 2);
        // unbind b0001, then add again: the id must NOT be reused
        let (removed, rev3) = remove_by_id(&store, "b0001").unwrap();
        assert_eq!(removed, vec!["b0001".to_string()]);
        assert_eq!(rev3, 3);
        let (id3, _) = add(&store, &mut seq, &binding("app/focus/changed")).unwrap();
        assert_eq!(id3, "b0003", "ids are monotonic, never reused");
        let arr = bindings_of(&store);
        let ids: Vec<&str> = arr
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["b0002", "b0003"]);
    }

    #[test]
    fn allocate_id_raises_seq_to_max_existing_plus_one() {
        // simulates a daemon restart over a persisted document that already has b0007
        let doc = json!([{"id": "b0007", "topic": "deck/x", "steps": [{"do": "sleep", "ms": 1}]}]);
        let mut seq = 0u64;
        assert_eq!(allocate_id(&mut seq, &doc), "b0008");
        assert_eq!(allocate_id(&mut seq, &doc), "b0009");
        // a lower high-water than the document is raised, not lowered
        let mut seq = 3u64;
        assert_eq!(allocate_id(&mut seq, &doc), "b0008");
    }

    #[test]
    fn add_is_one_transaction_and_one_event_on_bindings() {
        let store = Store::fresh();
        let (tx, rx) = std::sync::mpsc::channel();
        store
            .subscribe(
                "/bindings",
                Arc::new(move |ev| {
                    let _ = tx.send(ev);
                }),
            )
            .unwrap();
        let mut seq = 0;
        let (id, rev) = add(&store, &mut seq, &binding("deck/control/dial/rotate")).unwrap();
        assert_eq!((id.as_str(), rev), ("b0001", 1));
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.path, "/bindings");
        assert_eq!(ev.rev, Some(1));
        assert_eq!(ev.value.as_array().unwrap().len(), 1);
        assert_eq!(ev.value[0]["id"], json!("b0001"));
        assert_eq!(ev.prev, Some(json!([])));
        assert!(
            rx.try_recv().is_err(),
            "exactly one event per committed change"
        );
    }

    // ---- unbind ------------------------------------------------------------

    #[test]
    fn unbind_by_id_unknown_or_malformed_is_e_path() {
        let store = Store::fresh();
        let mut seq = 0;
        add(&store, &mut seq, &binding("deck/control/keys.0/down")).unwrap();
        let err = remove_by_id(&store, "b0009").unwrap_err();
        assert!(matches!(err, StateError::Path(_)), "{err}");
        assert_eq!(err.code(), "E_PATH");
        let err = remove_by_id(&store, "x1").unwrap_err();
        assert!(matches!(err, StateError::Path(_)), "{err}");
        // the failed unbind changed nothing and did not bump rev
        assert_eq!(store.rev(), 1);
        assert_eq!(bindings_of(&store).as_array().unwrap().len(), 1);
    }

    #[test]
    fn unbind_by_topic_removes_all_matching_and_no_match_is_e_path() {
        let store = Store::fresh();
        let mut seq = 0;
        add(&store, &mut seq, &binding("deck/control/keys.0/down")).unwrap();
        add(&store, &mut seq, &binding("deck/control/keys.0/down")).unwrap();
        add(&store, &mut seq, &binding("deck/control/keys.1/down")).unwrap();
        let (removed, rev) = remove_by_topic(&store, "deck/control/keys.0/down").unwrap();
        assert_eq!(removed, vec!["b0001".to_string(), "b0002".to_string()]);
        assert_eq!(rev, 4, "one unbind operation = one rev bump");
        let arr = bindings_of(&store);
        assert_eq!(arr.as_array().unwrap().len(), 1);
        assert_eq!(arr[0]["topic"], json!("deck/control/keys.1/down"));
        let err = remove_by_topic(&store, "deck/control/keys.9/down").unwrap_err();
        assert!(matches!(err, StateError::Path(_)), "{err}");
        let err = remove_by_topic(&store, "bad topic").unwrap_err();
        assert!(matches!(err, StateError::Schema(_)), "{err}");
    }

    // ---- shape / schema validation -----------------------------------------

    #[test]
    fn binding_shape_validation() {
        // valid stored binding
        let ok = json!({
            "id": "b0001",
            "topic": "app/focus/changed",
            "when": {"payload": "app", "op": "eq", "value": "code"},
            "allow_exec": false,
            "steps": [{"do": "plugin", "id": "demo", "action": "bump", "args": {"by": 1}}]
        });
        validate_binding(&ok).unwrap();
        // request-shaped binding with the placeholder passes too
        let mut req = ok.clone();
        req["id"] = json!(BINDING_ID_PLACEHOLDER);
        validate_binding(&req).unwrap();
        // code-ish step rejected (T2/R8)
        let mut bad = ok.clone();
        bad["steps"] = json!([{"do": "eval", "code": "x"}]);
        assert!(validate_binding(&bad).is_err());
        // empty steps rejected (non-empty steps rule)
        let mut bad = ok.clone();
        bad["steps"] = json!([]);
        assert!(validate_binding(&bad).is_err());
        // bad id rejected
        let mut bad = ok.clone();
        bad["id"] = json!("x1");
        assert!(validate_binding(&bad).is_err());
        // exec with relative argv[0] rejected (M1)
        let mut bad = ok.clone();
        bad["steps"] = json!([{"do": "exec", "argv": ["sh", "-c", "x"]}]);
        assert!(validate_binding(&bad).is_err());
        // sleep out of range rejected
        let mut bad = ok.clone();
        bad["steps"] = json!([{"do": "sleep", "ms": 0}]);
        assert!(validate_binding(&bad).is_err());
        // unknown step verb rejected
        let mut bad = ok.clone();
        bad["steps"] = json!([{"do": "rm", "path": "/x"}]);
        assert!(validate_binding(&bad).is_err());
        // missing topic rejected
        let mut bad = ok.clone();
        bad.as_object_mut().unwrap().remove("topic");
        assert!(validate_binding(&bad).is_err());
        // non-object rejected
        assert!(validate_binding(&json!(5)).is_err());
    }

    #[test]
    fn when_condition_validation() {
        let base = |when: Value| json!({"id": "b0001", "topic": "deck/x/y", "when": when, "steps": [{"do": "sleep", "ms": 1}]});
        // payload + eq + value: ok
        validate_binding(&base(
            json!({"payload": "app", "op": "eq", "value": "code"}),
        ))
        .unwrap();
        // path + gt + value: ok
        validate_binding(&base(json!({"path": "/focus/app", "op": "gt", "value": 3}))).unwrap();
        // exists without value: ok
        validate_binding(&base(json!({"payload": "control", "op": "exists"}))).unwrap();
        // both path and payload: rejected
        assert!(validate_binding(&base(
            json!({"path": "/focus", "payload": "app", "op": "exists"})
        ))
        .is_err());
        // neither: rejected
        assert!(validate_binding(&base(json!({"op": "eq", "value": 1}))).is_err());
        // eq without value: rejected
        assert!(validate_binding(&base(json!({"payload": "app", "op": "eq"}))).is_err());
        // exists with value: rejected
        assert!(
            validate_binding(&base(json!({"payload": "app", "op": "exists", "value": 1}))).is_err()
        );
        // unknown op: rejected
        assert!(validate_binding(&base(
            json!({"payload": "app", "op": "regex", "value": ".*"})
        ))
        .is_err());
        // ops the contract lists all validate
        for op in ["exists", "eq", "neq", "gt", "lt"] {
            let when = if op == "exists" {
                json!({"payload": "app", "op": op})
            } else {
                json!({"payload": "app", "op": op, "value": 1})
            };
            validate_binding(&base(when)).unwrap();
        }
    }

    #[test]
    fn topic_grammar_reuse() {
        for ok in [
            "deck/control/keys.0/down",
            "deck/control/keys/*",
            "deck/*",
            "app/focus/changed",
            "app/demo/*",
            "deck/control/dial/rotate",
        ] {
            validate_binding_topic(ok).unwrap();
        }
        for bad in [
            "",
            "x/y/z",
            "deck",
            "deck/",
            "deck//x",
            "deck/control/keys*/down",
            "deck/control/keys.0*/down",
            "deck/has space/x",
            "deck/x/y/z/w/1/2/3/4/5/6/7/8/9/10/11/12/13/14/15/16/17",
        ] {
            assert!(
                validate_binding_topic(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
        assert!(validate_binding_topic(&format!("deck/{}", "x".repeat(MAX_TOPIC_LEN))).is_err());
        assert!(is_binding_id("b0001"));
        assert!(is_binding_id("b12345"));
        assert!(!is_binding_id("b123"));
        assert!(!is_binding_id("x0001"));
        assert!(!is_binding_id("b000a"));
    }

    #[test]
    fn add_validates_before_committing_and_rolls_back_on_error() {
        let store = Store::fresh();
        let mut seq = 0;
        let bad = json!({"id": "b0000", "topic": "deck/x", "steps": [{"do": "eval", "code": "x"}]});
        let err = add(&store, &mut seq, &bad).unwrap_err();
        assert!(matches!(err, StateError::Schema(_)), "{err}");
        assert_eq!(store.rev(), 0, "a rejected add must not bump rev");
        assert_eq!(bindings_of(&store), json!([]));
        // a bad topic is rejected the same way
        let bad = json!({"id": "b0000", "topic": "nope/x", "steps": [{"do": "sleep", "ms": 1}]});
        assert!(add(&store, &mut seq, &bad).is_err());
        assert_eq!(store.rev(), 0);
        assert_eq!(seq, 0, "a rejected add must not consume an id");
    }

    #[test]
    fn duplicate_ids_cannot_be_stored_even_via_raw_ops() {
        // the schema's uniqueItems + id pattern plus our allocation make duplicates
        // impossible; a hand-crafted duplicate array is rejected by the commit validation
        let store = Store::fresh();
        let dup = json!([
            {"id": "b0001", "topic": "deck/a/b", "steps": [{"do": "sleep", "ms": 1}]},
            {"id": "b0001", "topic": "deck/a/b", "steps": [{"do": "sleep", "ms": 1}]}
        ]);
        let r = store.commit(&Transaction::new(vec![Op::set("/bindings", dup)]));
        assert!(
            r.is_err(),
            "identical duplicate elements violate uniqueItems"
        );
    }
}
