//! The write path M3 plans for `dbd`, reduced to what the spike measures.
//!
//! Each document is a Loro doc. SQLite holds three views of it, all written in one
//! transaction: the Loro snapshot plus a log of incremental updates (the source of
//! truth), a JSON copy for queries, and a contentless FTS5 row for search.

use loro::{ExportMode, LoroDoc, LoroList, LoroMap, LoroValue, ToJson, ValueOrContainer};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::path::Path;

use crate::corpus::Email;

pub type Error = Box<dyn std::error::Error>;
pub type Result<T> = std::result::Result<T, Error>;

const SCHEMA: &str = "
CREATE TABLE documents (
    doc     INTEGER PRIMARY KEY,
    id      TEXT NOT NULL UNIQUE,      -- UUIDv7
    type    TEXT NOT NULL,
    created INTEGER NOT NULL,
    updated INTEGER NOT NULL
);
CREATE TABLE doc_state (doc INTEGER PRIMARY KEY, snapshot BLOB NOT NULL);
CREATE TABLE doc_json  (doc INTEGER PRIMARY KEY, type TEXT NOT NULL, json TEXT NOT NULL);
CREATE TABLE changes   (seq INTEGER PRIMARY KEY, doc INTEGER NOT NULL, at INTEGER NOT NULL, bytes BLOB NOT NULL);
CREATE INDEX changes_doc ON changes (doc, seq);
CREATE VIRTUAL TABLE fts USING fts5 (subject, body, content='', contentless_delete=1);
";

/// Expression index on the field the inbox sorts by. Added after the bulk load so the
/// benchmark can compare the same query with and without it.
pub const DATE_INDEX: &str = "CREATE INDEX email_date ON doc_json (type, json_extract(json, '$.date'))";

pub struct Store {
    pub conn: Connection,
}

impl Store {
    pub fn create(path: &Path) -> Result<Store> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store { conn })
    }

    pub fn sqlite_version() -> &'static str {
        rusqlite::version()
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn new_doc() -> LoroDoc {
    let doc = LoroDoc::new();
    // A real store would give each device its own peer ID; one is enough here.
    doc.set_peer_id(1).expect("valid peer id");
    doc
}

fn root(doc: &LoroDoc) -> LoroMap {
    doc.get_map("root")
}

pub fn json_of(doc: &LoroDoc) -> Result<String> {
    let LoroValue::Map(map) = doc.get_deep_value() else { return Err("root is not a map".into()) };
    let value = map.get("root").cloned().unwrap_or(LoroValue::Null);
    Ok(value.to_json())
}

fn text_field(doc: &LoroDoc, key: &str) -> String {
    match root(doc).get(key) {
        Some(ValueOrContainer::Value(LoroValue::String(s))) => s.to_string(),
        _ => String::new(),
    }
}

fn write_views(tx: &Transaction, doc_row: i64, doc: &LoroDoc, fts_replace: bool) -> Result<()> {
    tx.execute(
        "INSERT INTO doc_json (doc, type, json) VALUES (?1, 'Email', ?2)
         ON CONFLICT (doc) DO UPDATE SET json = excluded.json",
        params![doc_row, json_of(doc)?],
    )?;
    if fts_replace {
        tx.execute("DELETE FROM fts WHERE rowid = ?1", [doc_row])?;
    }
    tx.execute(
        "INSERT INTO fts (rowid, subject, body) VALUES (?1, ?2, ?3)",
        params![doc_row, text_field(doc, "subject"), text_field(doc, "body")],
    )?;
    Ok(())
}

/// Insert a new email. Returns its row ID.
pub fn insert(tx: &Transaction, email: &Email) -> Result<i64> {
    let doc = new_doc();
    let m = root(&doc);
    m.insert("from", email.from.as_str())?;
    m.insert("to", email.to.as_str())?;
    m.insert("subject", email.subject.as_str())?;
    m.insert("body", email.body.as_str())?;
    m.insert("date", email.date)?;
    m.insert("read", email.read)?;
    let labels = m.insert_container("labels", LoroList::new())?;
    for label in &email.labels {
        labels.push(*label)?;
    }
    doc.commit();

    let now = now_ms();
    let id = uuid::Uuid::now_v7().to_string();
    tx.execute(
        "INSERT INTO documents (id, type, created, updated) VALUES (?1, 'Email', ?2, ?2)",
        params![id, now],
    )?;
    let row = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO doc_state (doc, snapshot) VALUES (?1, ?2)",
        params![row, doc.export(ExportMode::Snapshot)?],
    )?;
    write_views(tx, row, &doc, false)?;
    Ok(row)
}

/// Rebuild a document from its snapshot plus every update logged since.
pub fn load(tx: &Transaction, doc_row: i64) -> Result<LoroDoc> {
    let snapshot: Vec<u8> = tx
        .query_row("SELECT snapshot FROM doc_state WHERE doc = ?1", [doc_row], |r| r.get(0))
        .optional()?
        .ok_or("no such document")?;
    let doc = new_doc();
    let mut updates = vec![snapshot];
    let mut stmt = tx.prepare_cached("SELECT bytes FROM changes WHERE doc = ?1 ORDER BY seq")?;
    for bytes in stmt.query_map([doc_row], |r| r.get::<_, Vec<u8>>(0))? {
        updates.push(bytes?);
    }
    doc.import_batch(&updates)?;
    Ok(doc)
}

/// The edit an email client makes most: mark read and add a label. Logs only the
/// incremental Loro update; the snapshot is left for a later compaction pass.
pub fn mark_read(tx: &Transaction, doc_row: i64, label: &str) -> Result<usize> {
    let doc = load(tx, doc_row)?;
    let before = doc.oplog_vv();
    let m = root(&doc);
    m.insert("read", true)?;
    if let Some(ValueOrContainer::Container(loro::Container::List(labels))) = m.get("labels") {
        labels.push(label)?;
    }
    doc.commit();
    let update = doc.export(ExportMode::updates(&before))?;
    let now = now_ms();
    tx.execute("INSERT INTO changes (doc, at, bytes) VALUES (?1, ?2, ?3)", params![doc_row, now, update])?;
    tx.execute("UPDATE documents SET updated = ?2 WHERE doc = ?1", params![doc_row, now])?;
    write_views(tx, doc_row, &doc, true)?;
    Ok(update.len())
}

/// Fold logged updates into a fresh snapshot and drop them.
pub fn compact(tx: &Transaction, doc_row: i64) -> Result<()> {
    let doc = load(tx, doc_row)?;
    tx.execute(
        "UPDATE doc_state SET snapshot = ?2 WHERE doc = ?1",
        params![doc_row, doc.export(ExportMode::Snapshot)?],
    )?;
    tx.execute("DELETE FROM changes WHERE doc = ?1", [doc_row])?;
    Ok(())
}

/// The plan's example inbox query, compiled by hand to the SQL M3's compiler would emit.
///
/// `json_each` has a hidden column named `json`, so inside its subquery a bare `json`
/// silently means that column and every `EXISTS` is false. Always qualify the table.
pub const INBOX_QUERY: &str = "
SELECT doc FROM doc_json
WHERE type = 'Email'
  AND json_extract(json, '$.read') = 0
  AND EXISTS (SELECT 1 FROM json_each(doc_json.json, '$.labels') WHERE value = 'INBOX')
ORDER BY json_extract(json, '$.date') DESC
LIMIT 50";

/// A query whose filter matches few rows, so an index on the sort field does not help.
pub const SENDER_QUERY: &str = "
SELECT doc FROM doc_json
WHERE type = 'Email' AND json_extract(json, '$.from') = 'ana@example.com'
  AND EXISTS (SELECT 1 FROM json_each(doc_json.json, '$.labels') WHERE value = 'Family')";

pub const SEARCH_QUERY: &str = "SELECT rowid FROM fts WHERE fts MATCH ?1 ORDER BY rank LIMIT 50";

pub fn count_rows(conn: &Connection, sql: &str, arg: Option<&str>) -> Result<usize> {
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = match arg {
        Some(a) => stmt.query_map([a], |_| Ok(()))?.count(),
        None => stmt.query_map([], |_| Ok(()))?.count(),
    };
    Ok(rows)
}
