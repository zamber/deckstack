//! JSON-Schema subset validation of action arguments against the declared `params` schema.
//!
//! The host validates `args` against the full draft 2020-12 schema (jsonschema crate) before
//! every invoke; this module is the SDK-side early check so a bad payload fails inside the
//! handler boundary with `E_SCHEMA` instead of a panic. Supported keywords: `type`,
//! `properties`, `required`, `additionalProperties` (bool), `enum`, `const`, `items`,
//! `minItems`/`maxItems`, `minLength`/`maxLength`, `minimum`/`maximum`, `pattern` is NOT
//! evaluated (no regex dependency) — the host remains the enforcing validator, which is
//! documented in the crate README.

use serde_json::Value;

use crate::error::PluginError;

fn type_matches(schema_type: &str, v: &Value) -> bool {
    match schema_type {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        "number" => v.is_number(),
        "integer" => v.as_i64().is_some() || v.as_u64().is_some(),
        _ => true, // unknown type keyword: defer to the host
    }
}

/// Validate `args` against `schema`. Returns `Err(E_SCHEMA …)` on the first violation found.
pub fn validate_params(schema: &Value, args: &Value) -> Result<(), PluginError> {
    validate(schema, args, "$")
}

fn fail(path: &str, msg: String) -> PluginError {
    PluginError::schema(format!("args at {path}: {msg}"))
}

fn validate(schema: &Value, v: &Value, path: &str) -> Result<(), PluginError> {
    let Some(s) = schema.as_object() else {
        return Ok(());
    };
    if let Some(t) = s.get("type") {
        let ok = match t {
            Value::String(one) => type_matches(one, v),
            Value::Array(list) => list
                .iter()
                .filter_map(Value::as_str)
                .any(|one| type_matches(one, v)),
            _ => true,
        };
        if !ok {
            return Err(fail(path, format!("expected type {t}, got {v}")));
        }
    }
    if let Some(list) = s.get("enum").and_then(Value::as_array) {
        if !list.contains(v) {
            return Err(fail(path, format!("{v} is not one of {list:?}")));
        }
    }
    if let Some(c) = s.get("const") {
        if c != v {
            return Err(fail(path, format!("{v} != const {c}")));
        }
    }
    if let Some(n) = s.get("minimum").and_then(Value::as_f64) {
        if v.as_f64().is_none_or(|x| x < n) {
            return Err(fail(path, format!("{v} < minimum {n}")));
        }
    }
    if let Some(n) = s.get("maximum").and_then(Value::as_f64) {
        if v.as_f64().is_none_or(|x| x > n) {
            return Err(fail(path, format!("{v} > maximum {n}")));
        }
    }
    if let Some(n) = s.get("minLength").and_then(Value::as_u64) {
        if v.as_str().is_none_or(|x| (x.chars().count() as u64) < n) {
            return Err(fail(path, format!("string shorter than minLength {n}")));
        }
    }
    if let Some(n) = s.get("maxLength").and_then(Value::as_u64) {
        if v.as_str().is_none_or(|x| (x.chars().count() as u64) > n) {
            return Err(fail(path, format!("string longer than maxLength {n}")));
        }
    }
    if let Some(arr) = v.as_array() {
        if let Some(n) = s.get("minItems").and_then(Value::as_u64) {
            if (arr.len() as u64) < n {
                return Err(fail(path, format!("array shorter than minItems {n}")));
            }
        }
        if let Some(n) = s.get("maxItems").and_then(Value::as_u64) {
            if (arr.len() as u64) > n {
                return Err(fail(path, format!("array longer than maxItems {n}")));
            }
        }
        if let Some(item_schema) = s.get("items") {
            for (i, item) in arr.iter().enumerate() {
                validate(item_schema, item, &format!("{path}[{i}]"))?;
            }
        }
    }
    if let Some(obj) = v.as_object() {
        if let Some(props) = s.get("properties").and_then(Value::as_object) {
            for (key, sub) in props {
                if let Some(val) = obj.get(key) {
                    validate(sub, val, &format!("{path}.{key}"))?;
                }
            }
        }
        if let Some(req) = s.get("required").and_then(Value::as_array) {
            for r in req.iter().filter_map(Value::as_str) {
                if !obj.contains_key(r) {
                    return Err(fail(path, format!("missing required property {r:?}")));
                }
            }
        }
        if s.get("additionalProperties").and_then(Value::as_bool) == Some(false) {
            if let Some(props) = s.get("properties").and_then(Value::as_object) {
                for key in obj.keys() {
                    if !props.contains_key(key) {
                        return Err(fail(
                            path,
                            format!("additional property {key:?} not allowed"),
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bump_schema() -> Value {
        json!({"type":"object",
              "properties":{"by":{"type":"integer","minimum":1}},
              "required":["by"],
              "additionalProperties":false})
    }

    #[test]
    fn accepts_valid_args() {
        validate_params(&bump_schema(), &json!({"by":2})).unwrap();
    }

    #[test]
    fn rejects_wrong_type_missing_required_and_additional() {
        assert_eq!(
            validate_params(&bump_schema(), &json!({"by":"x"}))
                .unwrap_err()
                .code,
            crate::error::ErrorCode::ESchema
        );
        assert!(validate_params(&bump_schema(), &json!({})).is_err());
        assert!(validate_params(&bump_schema(), &json!({"by":1,"extra":true})).is_err());
        assert!(
            validate_params(&bump_schema(), &json!({"by":0})).is_err(),
            "minimum"
        );
        assert!(
            validate_params(&bump_schema(), &json!([1])).is_err(),
            "not an object"
        );
    }

    #[test]
    fn nested_and_array_keywords() {
        let schema = json!({"type":"object","properties":{
            "items":{"type":"array","minItems":1,"items":{"type":"string","minLength":2}},
            "mode":{"enum":["a","b"]},
            "n":{"const":3}
        }});
        validate_params(&schema, &json!({"items":["ab"],"mode":"a","n":3})).unwrap();
        assert!(validate_params(&schema, &json!({"items":[],"mode":"a","n":3})).is_err());
        assert!(validate_params(&schema, &json!({"items":["a"],"mode":"a","n":3})).is_err());
        assert!(validate_params(&schema, &json!({"items":["ab"],"mode":"c","n":3})).is_err());
        assert!(validate_params(&schema, &json!({"items":["ab"],"mode":"a","n":4})).is_err());
    }
}
