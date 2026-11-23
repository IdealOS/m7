//! System schemas: the common document types (`schemas/*.json`), checked on write.
//!
//! Documents are schemaless, except that a document of a system type must match
//! its schema. Unknown fields are always allowed, so apps can add their own.
//!
//! The schemas use a small subset of JSON Schema, which this module checks itself
//! rather than pulling in a full validator: `type`, `properties`, `required`,
//! `items`, `enum`, `const`, `anyOf`, `minimum`/`maximum`, `minLength`/`maxLength`,
//! `minItems`/`maxItems`, `$defs` with local `$ref`, and annotations (`title`,
//! `description`, `format`, `default`, `examples`). Two keywords are our own:
//! `x-kind` (`"link"` or `"blob"`) says a value is `{"$link": id}` or
//! `{"$blob": hash}`, and a schema's top-level `x-indexes` lists the fields that
//! get an expression index. A schema using anything else fails to load (a test
//! loads them all), so a keyword can't be silently ignored.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use rusqlite::Connection;
use serde_json::{Map, Value};

use crate::Result;
use crate::query::json_path;
use crate::refs::{BLOB, LINK, valid_hash};

/// The system schemas, by type name. Each file's name is its type.
const FILES: &[(&str, &str)] = &[
    ("App", include_str!("../../../schemas/App.json")),
    ("Contact", include_str!("../../../schemas/Contact.json")),
    ("Email", include_str!("../../../schemas/Email.json")),
    ("Image", include_str!("../../../schemas/Image.json")),
    ("Message", include_str!("../../../schemas/Message.json")),
    ("Playlist", include_str!("../../../schemas/Playlist.json")),
    ("Setting", include_str!("../../../schemas/Setting.json")),
    ("TextDocument", include_str!("../../../schemas/TextDocument.json")),
    ("Track", include_str!("../../../schemas/Track.json")),
];

const KEYWORDS: &[&str] = &[
    "type",
    "properties",
    "required",
    "items",
    "enum",
    "const",
    "anyOf",
    "minimum",
    "maximum",
    "minLength",
    "maxLength",
    "minItems",
    "maxItems",
    "$ref",
    "x-kind",
    "additionalProperties",
];
const ANNOTATIONS: &[&str] = &["title", "description", "format", "default", "examples"];
const TOP_LEVEL: &[&str] = &["$schema", "$id", "$defs", "x-indexes"];

/// One system schema.
#[derive(Debug)]
pub struct Schema {
    pub name: String,
    /// The schema as written, for clients that want to show or check it.
    pub json: Value,
    /// Fields with an expression index.
    pub indexes: Vec<String>,
}

static SCHEMAS: OnceLock<BTreeMap<String, Schema>> = OnceLock::new();

/// Every system schema, by type name.
pub fn all() -> &'static BTreeMap<String, Schema> {
    SCHEMAS.get_or_init(|| {
        FILES
            .iter()
            .map(|(name, text)| {
                let schema = load(name, text).unwrap_or_else(|e| panic!("schemas/{name}.json: {e}"));
                (name.to_string(), schema)
            })
            .collect()
    })
}

/// The system schema for documents of `doc_type`, if it has one.
pub fn get(doc_type: &str) -> Option<&'static Schema> {
    all().get(doc_type)
}

/// Parse and check one schema file.
pub fn load(name: &str, text: &str) -> std::result::Result<Schema, String> {
    let json: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let Value::Object(top) = &json else { return Err("a schema is an object".into()) };
    if top.get("title").and_then(Value::as_str) != Some(name) {
        return Err(format!("its title must be its type name, {name}"));
    }
    if top.get("type").and_then(Value::as_str) != Some("object") {
        return Err("a document schema has \"type\": \"object\"".into());
    }
    let defs = match top.get("$defs") {
        None => Map::new(),
        Some(Value::Object(d)) => d.clone(),
        Some(_) => return Err("$defs is an object".into()),
    };
    check(&json, "", &defs, true)?;
    for (k, def) in &defs {
        check(def, &format!("$defs/{k}"), &defs, false)?;
    }
    let indexes = match top.get("x-indexes") {
        None => Vec::new(),
        Some(Value::Array(fields)) => fields
            .iter()
            .map(|f| {
                let f = f.as_str().ok_or("x-indexes lists field paths")?;
                json_path(f).map_err(|e| e.to_string())?;
                Ok(f.to_owned())
            })
            .collect::<std::result::Result<_, String>>()?,
        Some(_) => return Err("x-indexes lists field paths".into()),
    };
    Ok(Schema { name: name.into(), json, indexes })
}

/// Fail on keywords this module doesn't implement, and on malformed ones.
fn check(s: &Value, at: &str, defs: &Map<String, Value>, top: bool) -> std::result::Result<(), String> {
    let Value::Object(o) = s else { return Err(format!("{at}: a schema is an object")) };
    for (k, v) in o {
        let known = KEYWORDS.contains(&k.as_str())
            || ANNOTATIONS.contains(&k.as_str())
            || (top && TOP_LEVEL.contains(&k.as_str()));
        if !known {
            return Err(format!("{at}: unsupported keyword {k}"));
        }
        match k.as_str() {
            "properties" => {
                let Value::Object(props) = v else { return Err(format!("{at}: properties is an object")) };
                for (name, p) in props {
                    check(p, &format!("{at}/{name}"), defs, false)?;
                }
            }
            "items" | "additionalProperties" if v.is_object() => check(v, &format!("{at}/{k}"), defs, false)?,
            "anyOf" => {
                let Value::Array(alts) = v else { return Err(format!("{at}: anyOf is a list")) };
                for (i, alt) in alts.iter().enumerate() {
                    check(alt, &format!("{at}/anyOf/{i}"), defs, false)?;
                }
            }
            "$ref" => {
                let target = v.as_str().and_then(|r| r.strip_prefix("#/$defs/"));
                if !target.is_some_and(|t| defs.contains_key(t)) {
                    return Err(format!("{at}: $ref must name one of this schema's $defs"));
                }
            }
            "x-kind" if v != "link" && v != "blob" => {
                return Err(format!("{at}: x-kind is \"link\" or \"blob\""));
            }
            _ => {}
        }
    }
    Ok(())
}

impl Schema {
    /// Check `data` against this schema; the error says where it doesn't match.
    pub fn validate(&self, data: &Value) -> std::result::Result<(), String> {
        let defs = self.json.get("$defs").and_then(Value::as_object);
        validate(&self.json, data, "", defs)
    }
}

fn validate(
    s: &Value,
    v: &Value,
    at: &str,
    defs: Option<&Map<String, Value>>,
) -> std::result::Result<(), String> {
    let field = || if at.is_empty() { "the document".to_owned() } else { at.to_owned() };
    let Value::Object(s) = s else { return Ok(()) };
    if let Some(r) = s.get("$ref").and_then(Value::as_str) {
        let name = r.trim_start_matches("#/$defs/");
        if let Some(def) = defs.and_then(|d| d.get(name)) {
            validate(def, v, at, defs)?;
        }
    }
    if let Some(kind) = s.get("x-kind").and_then(Value::as_str) {
        let (key, ok): (&str, fn(&str) -> bool) =
            if kind == "link" { (LINK, |id| !id.is_empty()) } else { (BLOB, valid_hash) };
        let good =
            v.as_object().is_some_and(|o| o.len() == 1 && o.get(key).and_then(Value::as_str).is_some_and(ok));
        if !good {
            return Err(format!("{} must be a {kind}: {{\"{key}\": ...}}", field()));
        }
    }
    if let Some(t) = s.get("type") {
        let types: Vec<&str> = match t {
            Value::String(t) => vec![t.as_str()],
            Value::Array(ts) => ts.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if !types.iter().any(|t| has_type(v, t)) {
            return Err(format!("{} must be {}", field(), types.join(" or ")));
        }
    }
    if let Some(Value::Array(allowed)) = s.get("enum")
        && !allowed.contains(v)
    {
        return Err(format!("{} must be one of {}", field(), Value::Array(allowed.clone())));
    }
    if let Some(c) = s.get("const")
        && c != v
    {
        return Err(format!("{} must be {c}", field()));
    }
    if let Some(Value::Array(alts)) = s.get("anyOf")
        && !alts.iter().any(|alt| validate(alt, v, at, defs).is_ok())
    {
        return Err(format!("{} matches none of the allowed shapes", field()));
    }
    if let Some(n) = v.as_f64() {
        if let Some(min) = s.get("minimum").and_then(Value::as_f64)
            && n < min
        {
            return Err(format!("{} must be at least {min}", field()));
        }
        if let Some(max) = s.get("maximum").and_then(Value::as_f64)
            && n > max
        {
            return Err(format!("{} must be at most {max}", field()));
        }
    }
    let count = |key: &str| s.get(key).and_then(Value::as_u64).map(|n| n as usize);
    let within = |len: usize, what: &str| -> std::result::Result<(), String> {
        if let Some(min) = count(&format!("min{what}"))
            && len < min
        {
            return Err(format!("{} is too short (at least {min})", field()));
        }
        if let Some(max) = count(&format!("max{what}"))
            && len > max
        {
            return Err(format!("{} is too long (at most {max})", field()));
        }
        Ok(())
    };
    match v {
        Value::String(text) => within(text.chars().count(), "Length")?,
        Value::Array(items) => {
            within(items.len(), "Items")?;
            if let Some(item) = s.get("items") {
                for (i, x) in items.iter().enumerate() {
                    validate(item, x, &join(at, &i.to_string()), defs)?;
                }
            }
        }
        Value::Object(o) => {
            if let Some(Value::Array(required)) = s.get("required") {
                for name in required.iter().filter_map(Value::as_str) {
                    if !o.contains_key(name) {
                        return Err(format!("{} is required", join(at, name)));
                    }
                }
            }
            let props = s.get("properties").and_then(Value::as_object);
            for (k, x) in o {
                match props.and_then(|p| p.get(k)) {
                    Some(p) => validate(p, x, &join(at, k), defs)?,
                    // Unknown fields are allowed unless additionalProperties says otherwise.
                    None => match s.get("additionalProperties") {
                        Some(Value::Bool(false)) => return Err(format!("{} isn't allowed", join(at, k))),
                        Some(extra @ Value::Object(_)) => validate(extra, x, &join(at, k), defs)?,
                        _ => {}
                    },
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn has_type(v: &Value, t: &str) -> bool {
    match t {
        "string" => v.is_string(),
        "number" => v.is_number(),
        "integer" => v.is_i64() || v.is_u64() || v.as_f64().is_some_and(|f| f.fract() == 0.0),
        "boolean" => v.is_boolean(),
        "object" => v.is_object(),
        "array" => v.is_array(),
        "null" => v.is_null(),
        _ => false,
    }
}

fn join(at: &str, key: &str) -> String {
    if at.is_empty() { key.to_owned() } else { format!("{at}.{key}") }
}

/// Make `doc_json`'s expression indexes match the schemas' `x-indexes`: create the
/// missing ones and drop ones no schema lists any more. Each is on `(type, field)`,
/// the shape the query compiler's SQL can use.
pub fn sync_indexes(conn: &Connection) -> Result<()> {
    let mut wanted = BTreeMap::new();
    for schema in all().values() {
        for field in &schema.indexes {
            let name = format!("schema_{}_{}", schema.name, field.replace('.', "_"));
            wanted.insert(name, json_path(field)?);
        }
    }
    let existing: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'doc_json' AND name LIKE 'schema\\_%' ESCAPE '\\'")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for name in existing.iter().filter(|n| !wanted.contains_key(*n)) {
        conn.execute_batch(&format!("DROP INDEX \"{name}\""))?;
    }
    // Names and paths are checked identifiers (type names and json_path segments),
    // so they're safe to place in SQL.
    for (name, path) in wanted {
        conn.execute_batch(&format!(
            "CREATE INDEX IF NOT EXISTS \"{name}\" ON doc_json (type, json_extract(json, '{path}'))"
        ))?;
    }
    Ok(())
}
