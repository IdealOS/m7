//! Links and blob references inside document data.
//!
//! A link is a field whose value is `{"$link": "<document id>"}`; a blob reference
//! is `{"$blob": "<sha-256 hex>"}`. They live in the document like any other value,
//! so history, undo and sync carry them for free. Each write rewrites the
//! document's rows in `links` and `blob_refs` from them, the same way it rewrites
//! `doc_json`, so queries can follow links in either direction and blobs know who
//! uses them.
//!
//! A link may point at a document that doesn't exist (yet, or any more); a blob
//! reference must name a blob that was stored first.

use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Map, Value};

use crate::{Error, Result};

pub const LINK: &str = "$link";
pub const BLOB: &str = "$blob";

/// Longest id a link may hold.
const MAX_ID: usize = 128;

/// A link or blob reference found in a document, by the field path holding it.
#[derive(Debug, PartialEq)]
pub enum Ref {
    Link { path: String, id: String },
    Blob { path: String, hash: String },
}

/// Whether `o` is a link or blob reference object (whether or not it's well formed).
pub fn is_ref(o: &Map<String, Value>) -> bool {
    o.contains_key(LINK) || o.contains_key(BLOB)
}

/// Every link and blob reference in `data`, refusing malformed ones.
pub fn find(data: &Value) -> Result<Vec<Ref>> {
    let mut out = Vec::new();
    walk(data, &mut String::new(), &mut out)?;
    Ok(out)
}

fn walk(v: &Value, path: &mut String, out: &mut Vec<Ref>) -> Result<()> {
    let child = |path: &mut String, key: &str, v: &Value, out: &mut Vec<Ref>| {
        let len = path.len();
        if !path.is_empty() {
            path.push('.');
        }
        path.push_str(key);
        let r = walk(v, path, out);
        path.truncate(len);
        r
    };
    match v {
        Value::Object(o) if is_ref(o) => {
            let at = if path.is_empty() { "the document" } else { path.as_str() };
            let (key, value) = match (o.len(), o.iter().next()) {
                (1, Some((k, Value::String(s)))) => (k.as_str(), s),
                _ => {
                    return Err(Error::Invalid(format!(
                        "{at}: a link is {{\"$link\": \"<id>\"}} and a blob is {{\"$blob\": \"<sha-256>\"}}, \
                         with nothing else in the object"
                    )));
                }
            };
            if path.is_empty() {
                return Err(Error::Invalid("a document can't itself be a link or blob".into()));
            }
            if key == LINK {
                if value.is_empty() || value.len() > MAX_ID {
                    return Err(Error::Invalid(format!("{at}: a link needs a document id")));
                }
                out.push(Ref::Link { path: path.clone(), id: value.clone() });
            } else {
                if !valid_hash(value) {
                    return Err(Error::Invalid(format!(
                        "{at}: a blob is named by its SHA-256 in lowercase hex"
                    )));
                }
                out.push(Ref::Blob { path: path.clone(), hash: value.clone() });
            }
            Ok(())
        }
        Value::Object(o) => o.iter().try_for_each(|(k, v)| child(path, k, v, out)),
        Value::Array(a) => a.iter().enumerate().try_for_each(|(i, v)| child(path, &i.to_string(), v, out)),
        _ => Ok(()),
    }
}

/// Whether `s` looks like a SHA-256 in lowercase hex.
pub fn valid_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Replace document `row`'s links and blob references with the ones in `data`.
pub fn write(tx: &Transaction, row: i64, data: &Value) -> Result<()> {
    let refs = find(data)?;
    clear(tx, row)?;
    for r in refs {
        match r {
            Ref::Link { path, id } => {
                tx.execute("INSERT INTO links (src, path, dst) VALUES (?1, ?2, ?3)", params![row, path, id])?;
            }
            Ref::Blob { path, hash } => {
                let known = tx
                    .query_row("SELECT 1 FROM blobs WHERE hash = ?1", [&hash], |_| Ok(()))
                    .optional()?
                    .is_some();
                if !known {
                    return Err(Error::Invalid(format!("{path}: no blob {hash}; store it first")));
                }
                tx.execute(
                    "INSERT INTO blob_refs (doc, path, hash) VALUES (?1, ?2, ?3)",
                    params![row, path, hash],
                )?;
            }
        }
    }
    Ok(())
}

/// Forget document `row`'s links and blob references (it was deleted).
pub fn clear(tx: &Transaction, row: i64) -> Result<()> {
    tx.execute("DELETE FROM links WHERE src = ?1", [row])?;
    tx.execute("DELETE FROM blob_refs WHERE doc = ?1", [row])?;
    Ok(())
}

/// Fill `links` for documents written before schema version 3. Blob references
/// are skipped: no blobs existed then.
pub fn backfill(tx: &Transaction) -> Result<()> {
    let docs: Vec<(i64, String)> = tx
        .prepare("SELECT doc, json FROM doc_json")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (row, json) in docs {
        let Ok(data) = serde_json::from_str::<Value>(&json) else { continue };
        // A document that was valid data before links existed may not be a valid
        // link now; it keeps its data, without the link.
        for r in find(&data).unwrap_or_default() {
            if let Ref::Link { path, id } = r {
                tx.execute("INSERT INTO links (src, path, dst) VALUES (?1, ?2, ?3)", params![row, path, id])?;
            }
        }
    }
    Ok(())
}

/// A link out of a document.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    /// The field holding it.
    pub path: String,
    /// The target's id.
    pub id: String,
    /// Whether the target exists and isn't deleted.
    pub exists: bool,
}

/// A link into a document, from another live one.
#[derive(Debug, Clone, PartialEq)]
pub struct Backlink {
    /// The linking document's id and type.
    pub id: String,
    pub doc_type: String,
    /// The field holding the link.
    pub path: String,
}

impl crate::Store {
    /// Document `id`'s links out (from its fields) and in (from other live
    /// documents). `id` must be a live document.
    pub fn links(&self, id: &str) -> Result<(Vec<Link>, Vec<Backlink>)> {
        let row: i64 = self
            .conn
            .query_row("SELECT doc FROM documents WHERE id = ?1 AND deleted IS NULL", [id], |r| r.get(0))
            .optional()?
            .ok_or_else(|| Error::NotFound(id.into()))?;
        let out = self
            .conn
            .prepare_cached(
                "SELECT l.path, l.dst, d.doc IS NOT NULL FROM links l
                 LEFT JOIN documents d ON d.id = l.dst AND d.deleted IS NULL
                 WHERE l.src = ?1 ORDER BY l.path",
            )?
            .query_map([row], |r| Ok(Link { path: r.get(0)?, id: r.get(1)?, exists: r.get(2)? }))?
            .collect::<rusqlite::Result<_>>()?;
        let back = self
            .conn
            .prepare_cached(
                "SELECT d.id, d.type, l.path FROM links l JOIN documents d ON d.doc = l.src
                 WHERE l.dst = ?1 ORDER BY d.doc, l.path",
            )?
            .query_map([id], |r| Ok(Backlink { id: r.get(0)?, doc_type: r.get(1)?, path: r.get(2)? }))?
            .collect::<rusqlite::Result<_>>()?;
        Ok((out, back))
    }
}
