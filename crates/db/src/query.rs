//! Query filters compiled to SQL over the JSON copies of documents.
//!
//! Two rules keep this safe and fast:
//!
//! - **Values are always bound parameters.** Field paths are the only text placed
//!   in the SQL, and only after [`json_path`] has checked every segment is an
//!   identifier or an index, so a path can't carry SQL.
//! - **Paths are literals,** so `json_extract(doc_json.json, '$.date')` in a query
//!   matches an expression index on the same text (Spike C: 18.8 ms to 0.21 ms).
//!
//! Every column is qualified with its table: inside a `json_each` subquery a bare
//! `json` silently means `json_each`'s hidden column of that name (Spike C).
//!
//! A comparison on a missing field is SQL NULL, which `WHERE`, `AND` and `OR` treat
//! as false, as the filter language wants. Only negation needs care: `$not`, `$ne`
//! and `$nin` wrap their condition in `IFNULL(.., 0)` first, so a missing field
//! counts as "not equal". Positive comparisons stay bare `json_extract(..) = ?`
//! terms, which is the form SQLite's planner can match to an expression index.
//!
//! Links are matched through the `links` table rather than the JSON: `$link` on a
//! field, and `$linksTo` / `$linkedFrom` on the whole document, each become an
//! `EXISTS` over its indexes.

use protocol::db::{DEFAULT_QUERY_LIMIT, Direction, MAX_QUERY_LIMIT, Query, valid_type};
use rusqlite::types::Value as Sql;
use serde_json::{Map, Value};

use crate::{Error, Result};

/// Deepest nesting of `$and`/`$or`/`$not` and paths a filter may use.
const MAX_DEPTH: usize = 16;
/// Most predicates (fields, operators, `$in` items) one filter may hold.
const MAX_TERMS: usize = 256;

pub struct Compiled {
    pub sql: String,
    pub params: Vec<Sql>,
}

/// The SQL for `q`, selecting the columns [`crate::Store::query`] reads.
pub fn compile(q: &Query) -> Result<Compiled> {
    if !valid_type(&q.doc_type) {
        return bad(format!("not a document type: {:?}", q.doc_type));
    }
    let limit = q.limit.unwrap_or(DEFAULT_QUERY_LIMIT);
    if limit > MAX_QUERY_LIMIT {
        return bad(format!("limit {limit} is over {MAX_QUERY_LIMIT}"));
    }
    let mut c = Compiler { params: vec![Sql::Text(q.doc_type.clone())], terms: 0 };
    let filter = c.filter(&q.filter, 0)?;
    let mut order = String::new();
    for key in &q.order_by {
        let dir = match key.1 {
            Direction::Asc => "ASC",
            Direction::Desc => "DESC",
        };
        order.push_str(&format!("{} {dir}, ", extract(&json_path(&key.0)?)));
    }
    c.params.push(Sql::Integer(limit.into()));
    c.params.push(Sql::Integer(q.offset.unwrap_or(0).into()));
    let sql = format!(
        "SELECT d.id, d.type, d.created, d.updated, doc_json.json
         FROM doc_json JOIN documents d ON d.doc = doc_json.doc
         WHERE doc_json.type = ?1 AND {filter}
         ORDER BY {order}doc_json.doc LIMIT ?{} OFFSET ?{}",
        c.params.len() - 1,
        c.params.len()
    );
    Ok(Compiled { sql, params: c.params })
}

/// SQL that returns a row if any of the documents `rows` (by row number) matches
/// `q`'s type and filter, ignoring its order and limit. Live queries use it to
/// skip re-running when no changed document matches.
pub fn compile_probe(q: &Query, rows: &[i64]) -> Result<Compiled> {
    if !valid_type(&q.doc_type) {
        return bad(format!("not a document type: {:?}", q.doc_type));
    }
    let mut c = Compiler { params: vec![Sql::Text(q.doc_type.clone())], terms: 0 };
    let filter = c.filter(&q.filter, 0)?;
    let list = c.bind(Sql::Text(serde_json::to_string(rows).expect("numbers serialize")));
    let sql = format!(
        "SELECT 1 FROM doc_json JOIN documents d ON d.doc = doc_json.doc
         WHERE doc_json.doc IN (SELECT value FROM json_each({list}))
           AND doc_json.type = ?1 AND {filter}
         LIMIT 1"
    );
    Ok(Compiled { sql, params: c.params })
}

/// `text` as an FTS5 query matching documents that contain every word, with each
/// word quoted so query syntax in it (`AND`, `"`, `*`, `:`) is just text. `None`
/// when there are no words.
pub fn fts_words(text: &str) -> Option<String> {
    let words: Vec<String> =
        text.split_whitespace().map(|w| format!("\"{}\"", w.replace('"', "\"\""))).collect();
    (!words.is_empty()).then(|| words.join(" "))
}

/// A field path (`from.name`, `items.0`) as a JSON path literal (`$.from.name`,
/// `$.items[0]`). Segments must be identifiers or array indexes.
pub fn json_path(field: &str) -> Result<String> {
    let mut path = String::from("$");
    if field.is_empty() || field.split('.').count() > MAX_DEPTH {
        return bad(format!("not a field path: {field:?}"));
    }
    for seg in field.split('.') {
        let ident = seg.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && seg.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        let index = !seg.is_empty() && seg.len() <= 9 && seg.bytes().all(|b| b.is_ascii_digit());
        if ident {
            path.push('.');
            path.push_str(seg);
        } else if index {
            path.push_str(&format!("[{seg}]"));
        } else {
            return bad(format!("not a field path: {field:?} (segments are names or indexes)"));
        }
    }
    Ok(path)
}

fn extract(path: &str) -> String {
    format!("json_extract(doc_json.json, '{path}')")
}

fn json_type(path: &str) -> String {
    format!("json_type(doc_json.json, '{path}')")
}

fn bad<T>(why: impl Into<String>) -> Result<T> {
    Err(Error::Invalid(why.into()))
}

struct Compiler {
    params: Vec<Sql>,
    terms: usize,
}

impl Compiler {
    fn bind(&mut self, v: Sql) -> String {
        self.params.push(v);
        format!("?{}", self.params.len())
    }

    fn term(&mut self) -> Result<()> {
        self.terms += 1;
        if self.terms > MAX_TERMS { bad(format!("filter has more than {MAX_TERMS} terms")) } else { Ok(()) }
    }

    /// An object of field conditions and `$and`/`$or`/`$not`, all of which must hold.
    fn filter(&mut self, f: &Map<String, Value>, depth: usize) -> Result<String> {
        if depth > MAX_DEPTH {
            return bad(format!("filter nests deeper than {MAX_DEPTH}"));
        }
        let mut parts = Vec::new();
        for (key, v) in f {
            parts.push(match key.as_str() {
                "$and" | "$or" => {
                    let Value::Array(items) = v else { return bad(format!("{key} takes a list of filters")) };
                    let mut subs = Vec::new();
                    for item in items {
                        let Value::Object(o) = item else {
                            return bad(format!("{key} takes a list of filters"));
                        };
                        subs.push(self.filter(o, depth + 1)?);
                    }
                    let (join, empty) = if key == "$and" { (" AND ", "1") } else { (" OR ", "0") };
                    if subs.is_empty() { empty.to_owned() } else { format!("({})", subs.join(join)) }
                }
                "$not" => {
                    let Value::Object(o) = v else { return bad("$not takes a filter") };
                    not(&self.filter(o, depth + 1)?)
                }
                // Documents with any field linking to the given id.
                "$linksTo" => {
                    self.term()?;
                    let id = self.bind(Sql::Text(link_id(key, v)?));
                    format!(
                        "EXISTS (SELECT 1 FROM links WHERE links.src = doc_json.doc AND links.dst = {id})"
                    )
                }
                // Documents the given document links to.
                "$linkedFrom" => {
                    self.term()?;
                    let id = self.bind(Sql::Text(link_id(key, v)?));
                    format!(
                        "EXISTS (SELECT 1 FROM links JOIN documents src ON src.doc = links.src \
                         WHERE src.id = {id} AND src.deleted IS NULL AND links.dst = d.id)"
                    )
                }
                k if k.starts_with('$') => return bad(format!("unknown filter operator {k}")),
                field => self.field(field, v)?,
            });
        }
        Ok(if parts.is_empty() { "1".into() } else { format!("({})", parts.join(" AND ")) })
    }

    /// One field's condition: a scalar to equal, or an object of operators.
    fn field(&mut self, field: &str, v: &Value) -> Result<String> {
        let path = &json_path(field)?;
        match v {
            Value::Object(ops) if ops.keys().all(|k| k.starts_with('$')) && !ops.is_empty() => {
                let mut parts = Vec::new();
                for (op, arg) in ops {
                    parts.push(if op == "$link" { self.link(field, arg)? } else { self.op(path, op, arg)? });
                }
                Ok(format!("({})", parts.join(" AND ")))
            }
            Value::Object(_) => {
                bad("to match inside an object, use a dotted path (`from.name`) or operators")
            }
            Value::Array(_) => bad("arrays can't be compared; use $contains for membership"),
            scalar => self.equals(path, scalar),
        }
    }

    fn op(&mut self, path: &str, op: &str, arg: &Value) -> Result<String> {
        Ok(match op {
            "$eq" => self.equals(path, scalar(op, arg)?)?,
            "$ne" => not(&self.equals(path, scalar(op, arg)?)?),
            "$gt" | "$gte" | "$lt" | "$lte" => {
                self.term()?;
                let sym = match op {
                    "$gt" => ">",
                    "$gte" => ">=",
                    "$lt" => "<",
                    _ => "<=",
                };
                let types = match arg {
                    Value::Number(_) => "('integer', 'real')",
                    Value::String(_) => "('text')",
                    _ => return bad(format!("{op} compares numbers or strings")),
                };
                let p = self.bind(sql_value(arg));
                format!("({} {sym} {p} AND {} IN {types})", extract(path), json_type(path))
            }
            "$in" | "$nin" => {
                let Value::Array(items) = arg else { return bad(format!("{op} takes a list")) };
                let mut alts = Vec::new();
                for item in items {
                    alts.push(self.equals(path, scalar(op, item)?)?);
                }
                let any = if alts.is_empty() { "0".to_owned() } else { format!("({})", alts.join(" OR ")) };
                if op == "$in" { any } else { not(&any) }
            }
            "$exists" => {
                self.term()?;
                let Value::Bool(want) = arg else { return bad("$exists takes true or false") };
                format!("({} IS {} NULL)", json_type(path), if *want { "NOT" } else { "" })
            }
            "$contains" => {
                self.term()?;
                let item = scalar(op, arg)?;
                // json_each over a missing path or a non-array yields no rows; qualify
                // every column (json_each has its own hidden `json`).
                let test = self.typed_match("json_each.value", "json_each.type", item);
                format!(
                    "EXISTS (SELECT 1 FROM json_each(doc_json.json, '{path}') WHERE {} = 'array' AND {test})",
                    json_type(path)
                )
            }
            other => return bad(format!("unknown operator {other}")),
        })
    }

    /// The field (or, for an array, one of its items) links to the given id.
    fn link(&mut self, field: &str, arg: &Value) -> Result<String> {
        self.term()?;
        let id = self.bind(Sql::Text(link_id("$link", arg)?));
        let exact = self.bind(Sql::Text(field.to_owned()));
        let below = self.bind(Sql::Text(format!("{field}.")));
        Ok(format!(
            "EXISTS (SELECT 1 FROM links WHERE links.src = doc_json.doc AND links.dst = {id} \
             AND (links.path = {exact} OR substr(links.path, 1, length({below})) = {below}))"
        ))
    }

    /// The field equals `v`, with types: `true` never equals `1`. `null` matches a
    /// null or missing field.
    fn equals(&mut self, path: &str, v: &Value) -> Result<String> {
        self.term()?;
        if v.is_null() {
            let t = json_type(path);
            return Ok(format!("({t} IS NULL OR {t} = 'null')"));
        }
        Ok(self.typed_match(&extract(path), &json_type(path), v))
    }

    /// `value_sql = v`, also checking `type_sql` so numbers, strings and booleans
    /// never compare equal to each other. NULL when the value is missing.
    fn typed_match(&mut self, value_sql: &str, type_sql: &str, v: &Value) -> String {
        match v {
            Value::Bool(b) => format!("{type_sql} = '{}'", if *b { "true" } else { "false" }),
            Value::Null => format!("{type_sql} = 'null'"),
            Value::Number(_) => {
                let p = self.bind(sql_value(v));
                format!("({value_sql} = {p} AND {type_sql} IN ('integer', 'real'))")
            }
            _ => {
                let p = self.bind(sql_value(v));
                format!("({value_sql} = {p} AND {type_sql} = 'text')")
            }
        }
    }
}

/// Negate a condition that may be NULL (a missing field): missing counts as false
/// before negating, so it comes out true.
fn not(cond: &str) -> String {
    format!("(NOT IFNULL({cond}, 0))")
}

/// The document id a link operator takes.
fn link_id(op: &str, v: &Value) -> Result<String> {
    match v {
        Value::String(id) if !id.is_empty() => Ok(id.clone()),
        _ => bad(format!("{op} takes a document id")),
    }
}

fn scalar<'a>(op: &str, v: &'a Value) -> Result<&'a Value> {
    match v {
        Value::Array(_) | Value::Object(_) => bad(format!("{op} takes a string, number, boolean or null")),
        v => Ok(v),
    }
}

fn sql_value(v: &Value) -> Sql {
    match v {
        Value::Number(n) => match n.as_i64() {
            Some(i) => Sql::Integer(i),
            None => Sql::Real(n.as_f64().unwrap_or(0.0)),
        },
        Value::String(s) => Sql::Text(s.clone()),
        Value::Bool(b) => Sql::Integer(*b as i64),
        _ => Sql::Null,
    }
}
