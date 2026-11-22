//! Navigation of the JSON state document by [`Path`]: get (with one-segment `*`
//! wildcards), set (creating intermediate objects) and delete. Arrays are indexed
//! numerically, except `/bindings/<id>`, where the segment is matched against the
//! `id` field of the binding objects (S2: topics contain `/`, so bindings are array
//! elements addressed by id, not path segments).

use crate::error::{Result, StateError};
use crate::path::Path;
use serde_json::{Map, Value};

fn not_found(path: &Path) -> StateError {
    StateError::Path(format!("path not found: {}", path.as_str()))
}

fn resolve_child<'a>(node: &'a Value, seg: &str) -> Option<&'a Value> {
    match node {
        Value::Object(map) => map.get(seg),
        Value::Array(arr) => {
            if let Ok(i) = seg.parse::<usize>() {
                arr.get(i)
            } else {
                arr.iter()
                    .find(|e| e.get("id").and_then(Value::as_str) == Some(seg))
            }
        }
        _ => None,
    }
}

fn array_index(arr: &[Value], seg: &str) -> Option<usize> {
    if let Ok(i) = seg.parse::<usize>() {
        return (i < arr.len()).then_some(i);
    }
    arr.iter()
        .position(|e| e.get("id").and_then(Value::as_str) == Some(seg))
}

fn navigate<'a>(mut node: &'a Value, segs: &[String]) -> Option<&'a Value> {
    for seg in segs {
        node = resolve_child(node, seg)?;
    }
    Some(node)
}

fn navigate_mut<'a>(node: &'a mut Value, segs: &[String]) -> Option<&'a mut Value> {
    let mut cur = node;
    for seg in segs {
        cur = match cur {
            Value::Object(map) => map.get_mut(seg)?,
            Value::Array(arr) => {
                let i = array_index(arr, seg)?;
                arr.get_mut(i)?
            }
            _ => return None,
        };
    }
    Some(cur)
}

/// Get a value by path. Supports at most one `*` segment: the result is then an object
/// keyed by the matched segment (or an array, for array parents such as `/bindings/*`).
/// Returns `None` when an exact path does not exist.
pub fn get(doc: &Value, path: &Path) -> Result<Option<Value>> {
    let segs = path.segments();
    let wildcard_count = segs.iter().filter(|s| s.as_str() == "*").count();
    if wildcard_count > 1 {
        return Err(StateError::Path(
            "at most one '*' wildcard segment per query path".to_string(),
        ));
    }
    if wildcard_count == 1 {
        let pos = segs
            .iter()
            .position(|s| s == "*")
            .expect("count checked above");
        let parent = navigate(doc, &segs[..pos]).ok_or_else(|| not_found(path))?;
        return match parent {
            Value::Object(map) => {
                let mut out = Map::new();
                for (k, _) in map {
                    let mut full: Vec<String> = segs[..pos].to_vec();
                    full.push(k.clone());
                    full.extend_from_slice(&segs[pos + 1..]);
                    if let Some(v) = navigate(doc, &full) {
                        out.insert(k.clone(), v.clone());
                    }
                }
                Ok(Some(Value::Object(out)))
            }
            Value::Array(arr) => {
                let mut vals = Vec::new();
                for i in 0..arr.len() {
                    let mut full: Vec<String> = segs[..pos].to_vec();
                    full.push(i.to_string());
                    full.extend_from_slice(&segs[pos + 1..]);
                    if let Some(v) = navigate(doc, &full) {
                        vals.push(v.clone());
                    }
                }
                Ok(Some(Value::Array(vals)))
            }
            _ => Err(StateError::Path(format!(
                "wildcard parent of '{}' is not an object or array",
                path.as_str()
            ))),
        };
    }
    Ok(navigate(doc, segs).cloned())
}

/// Set `value` at `path`, creating intermediate objects as needed. Array children are
/// replaced by numeric index or by `id` (for `/bindings/<id>`); an unknown id is an
/// error — appending is done by writing the whole `/bindings` array.
pub fn set(doc: &mut Value, path: &Path, value: Value) -> Result<()> {
    if path.has_wildcard() {
        return Err(StateError::Path(
            "wildcard '*' is not allowed in write paths".to_string(),
        ));
    }
    set_at(doc, path.segments(), value)
}

fn set_at(node: &mut Value, segs: &[String], value: Value) -> Result<()> {
    let Some((first, rest)) = segs.split_first() else {
        *node = value;
        return Ok(());
    };
    if rest.is_empty() {
        match node {
            Value::Object(map) => {
                map.insert(first.clone(), value);
                Ok(())
            }
            Value::Array(arr) => {
                if let Some(i) = array_index(arr, first) {
                    arr[i] = value;
                    Ok(())
                } else if first.parse::<usize>().is_ok() {
                    Err(StateError::Path(format!(
                        "array index {first} out of range"
                    )))
                } else {
                    Err(StateError::Path(format!(
                        "no array element with id '{first}'"
                    )))
                }
            }
            _ => Err(StateError::Path(format!(
                "cannot set '{first}' on a non-object, non-array value"
            ))),
        }
    } else {
        match node {
            Value::Object(map) => {
                let child = map.entry(first.clone()).or_insert(Value::Null);
                if child.is_null() {
                    *child = Value::Object(Map::new());
                }
                if !child.is_object() && !child.is_array() {
                    return Err(StateError::Path(format!("'{first}' is not a container")));
                }
                set_at(child, rest, value)
            }
            Value::Array(arr) => {
                let i = array_index(arr, first)
                    .ok_or_else(|| StateError::Path(format!("no array element '{first}'")))?;
                set_at(&mut arr[i], rest, value)
            }
            _ => Err(StateError::Path(format!(
                "cannot descend into '{first}' on a non-container value"
            ))),
        }
    }
}

/// Delete the value at `path`. Missing paths are an `E_PATH` error.
pub fn delete(doc: &mut Value, path: &Path) -> Result<()> {
    let segs = path.segments();
    if segs.is_empty() {
        return Err(StateError::Path(
            "cannot delete the root document '/'".to_string(),
        ));
    }
    if path.has_wildcard() {
        return Err(StateError::Path(
            "wildcard '*' is not allowed in delete paths".to_string(),
        ));
    }
    let (last, parents) = segs.split_last().expect("non-empty checked above");
    let parent = navigate_mut(doc, parents).ok_or_else(|| not_found(path))?;
    match parent {
        Value::Object(map) => {
            if map.remove(last).is_none() {
                return Err(not_found(path));
            }
            Ok(())
        }
        Value::Array(arr) => {
            if let Some(i) = array_index(arr, last) {
                arr.remove(i);
                Ok(())
            } else {
                Err(not_found(path))
            }
        }
        _ => Err(not_found(path)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc() -> Value {
        json!({"controls": {"keys": {"0": {"label": "a"}}}, "bindings": [{"id": "b0001", "topic": "deck/x"}]})
    }

    #[test]
    fn get_exact_and_missing() {
        let d = doc();
        let p = Path::parse("/controls/keys/0/label").unwrap();
        assert_eq!(get(&d, &p).unwrap(), Some(json!("a")));
        let p = Path::parse("/controls/keys/1").unwrap();
        assert_eq!(get(&d, &p).unwrap(), None);
    }

    #[test]
    fn get_wildcard_object_and_array() {
        let mut d = doc();
        d["controls"]["keys"]["3"] = json!({"label": "b"});
        let p = Path::parse("/controls/keys/*").unwrap();
        let v = get(&d, &p).unwrap().unwrap();
        assert_eq!(v, json!({"0": {"label": "a"}, "3": {"label": "b"}}));
        let p = Path::parse("/bindings/*").unwrap();
        let v = get(&d, &p).unwrap().unwrap();
        assert_eq!(v, json!([{"id": "b0001", "topic": "deck/x"}]));
        assert!(get(&d, &Path::parse("/controls/*/*").unwrap()).is_err());
    }

    #[test]
    fn get_binding_by_id() {
        let d = doc();
        let p = Path::parse("/bindings/b0001/topic").unwrap();
        assert_eq!(get(&d, &p).unwrap(), Some(json!("deck/x")));
    }

    #[test]
    fn set_creates_intermediate_objects() {
        let mut d = json!({});
        let p = Path::parse("/controls/keys/0/image").unwrap();
        set(&mut d, &p, json!({"kind": "color", "value": "#ffffff"})).unwrap();
        assert_eq!(
            get(&d, &p).unwrap(),
            Some(json!({"kind": "color", "value": "#ffffff"}))
        );
    }

    #[test]
    fn set_replaces_array_element_by_id() {
        let mut d = doc();
        let p = Path::parse("/bindings/b0001").unwrap();
        set(&mut d, &p, json!({"id": "b0001", "topic": "deck/y"})).unwrap();
        assert_eq!(
            get(&d, &Path::parse("/bindings/b0001/topic").unwrap()).unwrap(),
            Some(json!("deck/y"))
        );
        assert!(set(
            &mut d,
            &Path::parse("/bindings/b9999").unwrap(),
            json!({"id": "b9999"})
        )
        .is_err());
    }

    #[test]
    fn delete_removes() {
        let mut d = doc();
        let p = Path::parse("/controls/keys/0/label").unwrap();
        delete(&mut d, &p).unwrap();
        assert_eq!(get(&d, &p).unwrap(), None);
        assert!(delete(&mut d, &p).is_err());
        let p = Path::parse("/bindings/b0001").unwrap();
        delete(&mut d, &p).unwrap();
        assert_eq!(
            get(&d, &Path::parse("/bindings").unwrap()).unwrap(),
            Some(json!([]))
        );
        assert!(delete(&mut d, &Path::root()).is_err());
    }
}
