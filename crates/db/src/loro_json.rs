//! JSON in and out of Loro documents.
//!
//! A document's data lives under the root map `data`: objects become Loro maps,
//! arrays Loro lists, and scalars plain values. Strings are plain values for now,
//! not collaborative text (that comes with text binding in the compositor).
//!
//! Updates are JSON merge patches (RFC 7396): a key set to `null` is removed, an
//! object merges into the object already there, and anything else replaces the old
//! value. Only what changed is written, so an update logs a small Loro change.

use loro::{Container, LoroDoc, LoroList, LoroMap, LoroValue, ValueOrContainer};
use serde_json::{Map, Number, Value};

pub type LoroResult<T> = Result<T, loro::LoroError>;

/// The root map holding a document's data.
pub fn data(doc: &LoroDoc) -> LoroMap {
    doc.get_map("data")
}

/// A JSON scalar as a Loro value. Numbers that fit an `i64` stay integers.
fn scalar(v: &Value) -> LoroValue {
    match v {
        Value::Null => LoroValue::Null,
        Value::Bool(b) => LoroValue::Bool(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => LoroValue::I64(i),
            None => LoroValue::Double(n.as_f64().unwrap_or(0.0)),
        },
        Value::String(s) => LoroValue::String(s.as_str().into()),
        Value::Array(_) | Value::Object(_) => unreachable!("not a scalar"),
    }
}

/// Set `map[key]` to `v`, building containers for objects and arrays.
fn set(map: &LoroMap, key: &str, v: &Value) -> LoroResult<()> {
    match v {
        Value::Object(o) => fill_map(&map.insert_container(key, LoroMap::new())?, o),
        Value::Array(a) => fill_list(&map.insert_container(key, LoroList::new())?, a),
        _ => map.insert(key, scalar(v)),
    }
}

fn fill_map(map: &LoroMap, o: &Map<String, Value>) -> LoroResult<()> {
    for (k, v) in o {
        set(map, k, v)?;
    }
    Ok(())
}

fn fill_list(list: &LoroList, a: &[Value]) -> LoroResult<()> {
    for v in a {
        match v {
            Value::Object(o) => fill_map(&list.push_container(LoroMap::new())?, o)?,
            Value::Array(inner) => fill_list(&list.push_container(LoroList::new())?, inner)?,
            _ => list.push(scalar(v))?,
        }
    }
    Ok(())
}

/// Write a new document's data.
pub fn create(doc: &LoroDoc, data_json: &Map<String, Value>) -> LoroResult<()> {
    fill_map(&data(doc), data_json)
}

/// Apply a merge patch to `map`, writing only what changes.
pub fn merge(map: &LoroMap, patch: &Map<String, Value>) -> LoroResult<()> {
    for (k, v) in patch {
        let current = map.get(k);
        match (v, &current) {
            (Value::Null, None) => {}
            (Value::Null, Some(_)) => map.delete(k)?,
            (Value::Object(o), Some(ValueOrContainer::Container(Container::Map(inner)))) => merge(inner, o)?,
            _ => {
                // Replace, unless it's already equal.
                if current.as_ref().map(to_json) != Some(strip_nulls(v)) {
                    set(map, k, &strip_nulls(v))?;
                }
            }
        }
    }
    Ok(())
}

/// A merge patch's nulls inside a new object mean "absent", so a replaced object
/// is stored without them (RFC 7396 applies the patch to an empty object).
fn strip_nulls(v: &Value) -> Value {
    match v {
        Value::Object(o) => Value::Object(
            o.iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k.clone(), strip_nulls(v))).collect(),
        ),
        other => other.clone(),
    }
}

/// The JSON for one value or container.
fn to_json(v: &ValueOrContainer) -> Value {
    value_json(&v.get_deep_value())
}

fn value_json(v: &LoroValue) -> Value {
    match v {
        LoroValue::Null => Value::Null,
        LoroValue::Bool(b) => Value::Bool(*b),
        LoroValue::I64(i) => Value::Number((*i).into()),
        LoroValue::Double(d) => Number::from_f64(*d).map(Value::Number).unwrap_or(Value::Null),
        LoroValue::String(s) => Value::String(s.to_string()),
        LoroValue::List(items) => Value::Array(items.iter().map(value_json).collect()),
        LoroValue::Map(m) => Value::Object(m.iter().map(|(k, v)| (k.to_string(), value_json(v))).collect()),
        // Not produced by this module; shown as null rather than failing a read.
        LoroValue::Binary(_) | LoroValue::Container(_) => Value::Null,
    }
}

/// A document's data as a JSON object.
pub fn read(doc: &LoroDoc) -> Map<String, Value> {
    match value_json(&data(doc).get_deep_value()) {
        Value::Object(o) => o,
        _ => Map::new(),
    }
}

/// Every string in `v`, for the full-text index.
pub fn strings<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
    match v {
        Value::String(s) => out.push(s),
        Value::Array(a) => a.iter().for_each(|v| strings(v, out)),
        // Link targets and blob hashes are ids, not words anyone searches for.
        Value::Object(o) if crate::refs::is_ref(o) => {}
        Value::Object(o) => o.values().for_each(|v| strings(v, out)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn doc_with(v: Value) -> LoroDoc {
        let doc = LoroDoc::new();
        create(&doc, &obj(v)).unwrap();
        doc.commit();
        doc
    }

    #[test]
    fn json_round_trips_through_loro() {
        let v = json!({"s": "hi", "n": 3, "f": 2.5, "b": true, "z": null, "big": 18446744073709551615u64,
            "list": [1, {"a": [true]}, [2, 3]], "obj": {"x": {"y": "deep"}}});
        let back = Value::Object(read(&doc_with(v.clone())));
        let mut expected = v;
        expected["big"] = json!(18446744073709551615u64 as f64); // beyond i64: stored as a double
        assert_eq!(back, expected);
    }

    #[test]
    fn merge_patches_follow_rfc_7396() {
        let doc = doc_with(json!({"a": 1, "keep": "x", "obj": {"p": 1, "q": 2}, "list": [1, 2]}));
        merge(
            &data(&doc),
            &obj(json!({"a": null, "obj": {"q": null, "r": 3}, "list": [9], "new": {"m": null, "n": 1}})),
        )
        .unwrap();
        doc.commit();
        assert_eq!(
            Value::Object(read(&doc)),
            json!({"keep": "x", "obj": {"p": 1, "r": 3}, "list": [9], "new": {"n": 1}})
        );
    }

    #[test]
    fn unchanged_values_write_nothing() {
        let doc = doc_with(json!({"a": 1, "obj": {"p": [1, 2]}, "s": "same"}));
        let before = doc.oplog_vv();
        merge(&data(&doc), &obj(json!({"a": 1, "obj": {"p": [1, 2]}, "s": "same", "gone": null}))).unwrap();
        doc.commit();
        assert_eq!(doc.oplog_vv(), before, "no ops for a patch that changes nothing");
    }

    #[test]
    fn strings_are_collected_from_everywhere() {
        let v = json!({"a": "one", "b": ["two", {"c": "three"}], "d": 4});
        let mut out = Vec::new();
        strings(&v, &mut out);
        out.sort();
        assert_eq!(out, ["one", "three", "two"]);
    }
}
