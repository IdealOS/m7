//! The document store behind the `db` service.
//!
//! Each document is a Loro document, the source of truth. SQLite (WAL mode) holds,
//! all written in one transaction per change:
//!
//! - the Loro snapshot (`doc_state`) and a log of incremental updates since it
//!   (`changes`), folded back into the snapshot every [`COMPACT_AFTER`] updates;
//! - a JSON copy of the data (`doc_json`), which queries run against;
//! - a contentless FTS5 row of every string in the document, for search.
//!
//! Every write belongs to a change set, which records when it happened and who made
//! it (`origin`); history and undo build on these.
//!
//! Spike C measured this write path at about 0.1 ms per write on a Mac.

pub mod access;
pub mod blobs;
mod history;
pub mod live;
mod loro_json;
pub mod query;
mod refs;
pub mod retention;
pub mod schemas;
pub mod sha256;
pub mod snapshot;

pub use blobs::{BlobInfo, Blobs};
pub use history::FieldChange;
pub use history::{ChangeSet, Version};
pub use live::Watches;
pub use refs::{Backlink, Link};
pub use retention::{Compacted, Retention, compact_history};
pub use snapshot::{SnapshotInfo, Snapshots};

use std::path::Path;

use loro::{ExportMode, Frontiers, LoroDoc};
use protocol::db::{Doc, Query, valid_type};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Map, Value};

/// Updates logged per document before they're folded into its snapshot.
pub const COMPACT_AFTER: i64 = 64;

/// Most bytes of a document's JSON copy. Bulk data belongs in blobs.
pub const MAX_DOC_BYTES: usize = 1024 * 1024;

const SCHEMA: &str = "
CREATE TABLE meta (key TEXT PRIMARY KEY, value ANY) STRICT;
CREATE TABLE documents (
    doc     INTEGER PRIMARY KEY,
    id      TEXT NOT NULL UNIQUE,   -- UUIDv7
    type    TEXT NOT NULL,
    created INTEGER NOT NULL,       -- ms since the epoch
    updated INTEGER NOT NULL,
    deleted INTEGER                 -- set when deleted; the state stays for history
) STRICT;
CREATE TABLE doc_state (
    doc      INTEGER PRIMARY KEY REFERENCES documents (doc),
    snapshot BLOB NOT NULL,
    updates  INTEGER NOT NULL DEFAULT 0  -- rows in changes since the snapshot
) STRICT;
-- Only live documents have a JSON copy, so queries never see deleted ones.
CREATE TABLE doc_json (
    doc  INTEGER PRIMARY KEY REFERENCES documents (doc),
    type TEXT NOT NULL,
    json TEXT NOT NULL
) STRICT;
CREATE INDEX doc_json_type ON doc_json (type, doc);
CREATE TABLE change_sets (
    id     INTEGER PRIMARY KEY,
    at     INTEGER NOT NULL,
    origin TEXT NOT NULL            -- who made it: a service name or app id
) STRICT;
-- Loro updates not yet folded into their document's snapshot.
CREATE TABLE changes (
    seq        INTEGER PRIMARY KEY,
    doc        INTEGER NOT NULL REFERENCES documents (doc),
    change_set INTEGER NOT NULL REFERENCES change_sets (id),
    bytes      BLOB NOT NULL
) STRICT;
CREATE INDEX changes_doc ON changes (doc, seq);
CREATE VIRTUAL TABLE fts USING fts5 (text, content='', contentless_delete=1);
";

/// Changes to the schema, in order: entry `i` takes a database from version
/// `i + 1` to `i + 2`. A new database gets [`SCHEMA`] (version 1) and then all of them.
const MIGRATIONS: &[&str] = &[
    // 2: history and undo. Each change set's effect on each document, as Loro
    // versions before and after, kept forever (unlike `changes`, which compaction
    // empties). `undoes` marks a change set made by undoing another.
    "CREATE TABLE doc_versions (
        doc        INTEGER NOT NULL REFERENCES documents (doc),
        change_set INTEGER NOT NULL REFERENCES change_sets (id),
        kind       TEXT NOT NULL,   -- create, update, delete or restore
        before     BLOB NOT NULL,   -- Loro frontiers, encoded
        after      BLOB NOT NULL,
        PRIMARY KEY (doc, change_set)
    ) STRICT;
    CREATE INDEX doc_versions_set ON doc_versions (change_set);
    ALTER TABLE change_sets ADD COLUMN undoes INTEGER REFERENCES change_sets (id);",
    // 3: links and blobs. Both are kept like doc_json: rows for live documents only,
    // rewritten with each change from the `{"$link": id}` and `{"$blob": hash}`
    // values in the document (see `refs`).
    "CREATE TABLE links (
        src  INTEGER NOT NULL REFERENCES documents (doc),
        path TEXT NOT NULL,         -- the field holding the link, such as `from`
        dst  TEXT NOT NULL,         -- the target's id; it may not exist (yet, or any more)
        PRIMARY KEY (src, path)
    ) STRICT;
    CREATE INDEX links_dst ON links (dst, src);
    CREATE TABLE blobs (
        hash    TEXT PRIMARY KEY,   -- SHA-256, lowercase hex; the file is blobs/ab/cdef...
        size    INTEGER NOT NULL,
        created INTEGER NOT NULL
    ) STRICT;
    CREATE TABLE blob_refs (
        doc  INTEGER NOT NULL REFERENCES documents (doc),
        path TEXT NOT NULL,
        hash TEXT NOT NULL REFERENCES blobs (hash),
        PRIMARY KEY (doc, path)
    ) STRICT;
    CREATE INDEX blob_refs_hash ON blob_refs (hash);",
    // 4: who created each document (the origin of its create), so an app can be
    // given the documents it made (`$createdBy`).
    "ALTER TABLE documents ADD COLUMN creator TEXT;
    UPDATE documents SET creator = (
        SELECT cs.origin FROM doc_versions v JOIN change_sets cs ON cs.id = v.change_set
        WHERE v.doc = documents.doc AND v.kind = 'create');
    CREATE INDEX documents_creator ON documents (creator, doc);",
    // 5: history compaction (see `retention`). Pinned versions are never thinned
    // out, and each version records the blobs its data refers to, so a blob is
    // deleted only once no kept version needs it. Thinning a version drops its rows.
    "ALTER TABLE doc_versions ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;
    CREATE TABLE version_blobs (
        doc        INTEGER NOT NULL,
        change_set INTEGER NOT NULL,
        hash       TEXT NOT NULL REFERENCES blobs (hash),
        PRIMARY KEY (doc, change_set, hash),
        FOREIGN KEY (doc, change_set) REFERENCES doc_versions (doc, change_set) ON DELETE CASCADE
    ) STRICT;
    CREATE INDEX version_blobs_hash ON version_blobs (hash);",
];

const SCHEMA_VERSION: i64 = 1 + MIGRATIONS.len() as i64;

#[derive(Debug)]
pub enum Error {
    /// No live document has this id.
    NotFound(String),
    /// The request makes no sense: a bad type name, filter or document.
    Invalid(String),
    Sqlite(rusqlite::Error),
    /// Loro couldn't apply, load or export a document's state.
    Loro(String),
    /// A file outside the database (a snapshot or blob) couldn't be read or written.
    Io(String),
    /// The caller may not do this (see [`access`]).
    Denied(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            // Blobs and snapshots say what they are ("blob <hash>"); a bare id is a document.
            Error::NotFound(what) if what.contains(' ') => write!(f, "no {what}"),
            Error::NotFound(id) => write!(f, "no document {id}"),
            Error::Invalid(why) => f.write_str(why),
            Error::Sqlite(e) => write!(f, "database: {e}"),
            Error::Loro(e) => write!(f, "document state: {e}"),
            Error::Io(e) => write!(f, "file: {e}"),
            Error::Denied(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Error {
        Error::Sqlite(e)
    }
}

impl From<loro::LoroError> for Error {
    fn from(e: loro::LoroError) -> Error {
        Error::Loro(e.to_string())
    }
}

impl From<loro::LoroEncodeError> for Error {
    fn from(e: loro::LoroEncodeError) -> Error {
        Error::Loro(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

fn invalid<T>(why: impl Into<String>) -> Result<T> {
    Err(Error::Invalid(why.into()))
}

pub struct Store {
    conn: Connection,
    /// This device's Loro peer id, chosen when the database was created.
    peer: u64,
    /// While [`Store::guarded`] runs: what written documents must match.
    guard: Option<Map<String, Value>>,
}

impl Store {
    /// Open the database at `path`, creating it if needed.
    pub fn open(path: &Path) -> Result<Store> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Store::init(conn)
    }

    /// A database that lives only as long as this value, for tests.
    pub fn open_in_memory() -> Result<Store> {
        Store::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Store> {
        conn.pragma_update(None, "foreign_keys", true)?;
        migrate(&conn)?;
        schemas::sync_indexes(&conn)?;
        let peer = read_peer(&conn)?;
        let store = Store { conn, peer, guard: None };
        store.optimize()?;
        Ok(store)
    }

    /// Create a document of `doc_type` holding `data`.
    pub fn create(&mut self, doc_type: &str, data: &Map<String, Value>, origin: &str) -> Result<Doc> {
        if !valid_type(doc_type) {
            return invalid(format!("not a document type: {doc_type:?}"));
        }
        let doc = self.new_loro()?;
        loro_json::create(&doc, data)?;
        doc.commit();
        let json = checked_json(&doc)?;

        let tx = self.conn.transaction()?;
        let now = now_ms();
        let id = uuid::Uuid::now_v7().to_string();
        tx.execute(
            "INSERT INTO documents (id, type, created, updated, creator) VALUES (?1, ?2, ?3, ?3, ?4)",
            params![id, doc_type, now, origin],
        )?;
        let row = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO doc_state (doc, snapshot) VALUES (?1, ?2)",
            params![row, doc.export(ExportMode::Snapshot)?],
        )?;
        let set = change_set(&tx, now, origin, None)?;
        record_version(&tx, row, set, Kind::Create, &Frontiers::default(), &doc.oplog_frontiers())?;
        write_views(&tx, row, doc_type, &json, false)?;
        retention::record_blobs(&tx, row, set)?;
        check_guard(&tx, row, doc_type, self.guard.as_ref())?;
        tx.commit()?;
        Ok(Doc { id, doc_type: doc_type.into(), created: now, updated: now, data: loro_json::read(&doc) })
    }

    /// Apply a JSON merge patch to document `id`. A patch that changes nothing
    /// writes nothing.
    pub fn update(&mut self, id: &str, patch: &Map<String, Value>, origin: &str) -> Result<Doc> {
        let peer = self.peer;
        let tx = self.conn.transaction()?;
        let Some((row, doc_type, created)) = live_row(&tx, id)? else {
            return Err(Error::NotFound(id.into()));
        };
        let doc = load(&tx, row, peer)?;
        let before = Before::of(&doc);
        loro_json::merge(&loro_json::data(&doc), patch)?;
        doc.commit();
        let mut updated: i64 =
            tx.query_row("SELECT updated FROM documents WHERE doc = ?1", [row], |r| r.get(0))?;
        if doc.oplog_vv() != before.vv {
            updated = now_ms();
            let set = change_set(&tx, updated, origin, None)?;
            log_change(&tx, row, &doc_type, &doc, &before, set, updated)?;
            check_guard(&tx, row, &doc_type, self.guard.as_ref())?;
        }
        tx.commit()?;
        Ok(Doc { id: id.into(), doc_type, created, updated, data: loro_json::read(&doc) })
    }

    /// Edit the text at field `path` (dotted keys) of document `id`: at character
    /// `at`, delete `delete` characters, insert `insert`. The field becomes
    /// collaborative text. Returns the change set, or 0 if nothing changed.
    pub fn splice(
        &mut self,
        id: &str,
        path: &str,
        at: usize,
        delete: usize,
        insert: &str,
        origin: &str,
    ) -> Result<i64> {
        let keys: Vec<&str> = path.split('.').collect();
        if keys.iter().any(|k| k.is_empty()) {
            return invalid(format!("not a field path: {path:?}"));
        }
        let peer = self.peer;
        let tx = self.conn.transaction()?;
        let Some((row, doc_type, _)) = live_row(&tx, id)? else {
            return Err(Error::NotFound(id.into()));
        };
        let doc = load(&tx, row, peer)?;
        let before = Before::of(&doc);
        loro_json::splice(&doc, &keys, at, delete, insert).map_err(Error::Invalid)?;
        doc.commit();
        if doc.oplog_vv() == before.vv {
            return Ok(0);
        }
        let now = now_ms();
        let set = change_set(&tx, now, origin, None)?;
        log_change(&tx, row, &doc_type, &doc, &before, set, now)?;
        check_guard(&tx, row, &doc_type, self.guard.as_ref())?;
        tx.commit()?;
        Ok(set)
    }

    /// The live document `id`.
    pub fn get(&self, id: &str) -> Result<Doc> {
        self.conn
            .query_row(
                "SELECT d.id, d.type, d.created, d.updated, doc_json.json
                 FROM documents d JOIN doc_json ON doc_json.doc = d.doc WHERE d.id = ?1",
                [id],
                doc_from_row,
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(id.into()))?
    }

    /// Delete document `id`. Its state and history stay (for undo); it leaves
    /// queries and search.
    pub fn delete(&mut self, id: &str, origin: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        let Some((row, _, _)) = live_row(&tx, id)? else { return Err(Error::NotFound(id.into())) };
        let now = now_ms();
        let set = change_set(&tx, now, origin, None)?;
        let version = load(&tx, row, self.peer)?.oplog_frontiers();
        delete_row(&tx, row, set, now, &version)?;
        tx.commit()?;
        Ok(())
    }

    /// Documents matching `q` (see [`protocol::db`] for the filter language).
    pub fn query(&self, q: &Query) -> Result<Vec<Doc>> {
        let compiled = query::compile(q)?;
        let mut stmt = self.conn.prepare_cached(&compiled.sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(&compiled.params), doc_from_row)?;
        rows.map(|r| r?).collect()
    }

    /// Documents containing every word of `text`, best matches first, optionally
    /// only of `doc_type`.
    pub fn search(&self, text: &str, doc_type: Option<&str>, limit: u32) -> Result<Vec<Doc>> {
        let Some(fts_query) = query::fts_words(text) else { return Ok(Vec::new()) };
        let limit = limit.min(protocol::db::MAX_QUERY_LIMIT);
        let mut stmt = self.conn.prepare_cached(
            "SELECT d.id, d.type, d.created, d.updated, doc_json.json
             FROM fts JOIN doc_json ON doc_json.doc = fts.rowid JOIN documents d ON d.doc = fts.rowid
             WHERE fts MATCH ?1 AND (?2 IS NULL OR doc_json.type = ?2)
             ORDER BY fts.rank LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![fts_query, doc_type, limit], doc_from_row)?;
        rows.map(|r| r?).collect()
    }

    /// How many documents match `q` (its order, limit and offset aside).
    pub fn count(&self, q: &Query) -> Result<u64> {
        let compiled = query::compile_count(q)?;
        let mut stmt = self.conn.prepare_cached(&compiled.sql)?;
        Ok(stmt.query_row(rusqlite::params_from_iter(&compiled.params), |r| r.get::<_, i64>(0))? as u64)
    }

    /// The database file's size, with its write-ahead log (0 in memory).
    pub fn file_bytes(&self) -> Result<u64> {
        let path: String =
            self.conn
                .query_row("SELECT file FROM pragma_database_list WHERE name = 'main'", [], |r| r.get(0))?;
        if path.is_empty() {
            return Ok(0);
        }
        let size = |p: String| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        Ok(size(path.clone()) + size(format!("{path}-wal")))
    }

    /// Every type with live documents, and how many.
    pub fn types(&self) -> Result<std::collections::BTreeMap<String, u64>> {
        let mut stmt = self.conn.prepare_cached("SELECT type, count(*) FROM doc_json GROUP BY type")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Counts for `stats`: (documents, types, change sets, blobs, blob bytes).
    pub fn counts(&self) -> Result<(u64, u64, u64, u64, u64)> {
        let one =
            |sql: &str| -> Result<u64> { Ok(self.conn.query_row(sql, [], |r| r.get::<_, i64>(0))? as u64) };
        Ok((
            one("SELECT count(*) FROM doc_json")?,
            one("SELECT count(DISTINCT type) FROM doc_json")?,
            one("SELECT count(*) FROM change_sets")?,
            one("SELECT count(*) FROM blobs")?,
            one("SELECT IFNULL(sum(size), 0) FROM blobs")?,
        ))
    }

    /// The Loro document behind `id`, rebuilt from its snapshot and logged updates.
    /// For tests and the history API.
    pub fn loro(&self, id: &str) -> Result<LoroDoc> {
        let row: i64 = self
            .conn
            .query_row("SELECT doc FROM documents WHERE id = ?1", [id], |r| r.get(0))
            .optional()?
            .ok_or_else(|| Error::NotFound(id.into()))?;
        let tx = self.conn.unchecked_transaction()?;
        load(&tx, row, self.peer)
    }

    /// Refresh SQLite's statistics where they look stale (`PRAGMA optimize`), so the
    /// planner picks the right expression index. Without them it prefers an index
    /// that avoids sorting, even when another would skip almost every row. Cheap
    /// when nothing changed; dbd runs it at startup and hourly.
    pub fn optimize(&self) -> Result<()> {
        // 0x10002: also analyze tables that have never been analyzed, with a row
        // limit so a big table can't make this slow.
        self.conn.execute_batch("PRAGMA optimize = 0x10002")?;
        Ok(())
    }

    /// Whether live document `id`'s JSON copy (what queries see) still equals its
    /// Loro document (the source of truth).
    pub fn verify(&self, id: &str) -> Result<bool> {
        let stored = self.get(id)?.data;
        Ok(loro_json::read(&self.loro(id)?) == stored)
    }

    /// Run `write` with a guard: every document it creates or changes must match
    /// `filter` once changed, or the change is rolled back and refused with
    /// [`Error::Denied`]. The check runs inside the write's transaction, against
    /// exactly what would be committed.
    pub fn guarded<T>(
        &mut self,
        filter: Map<String, Value>,
        write: impl FnOnce(&mut Store) -> Result<T>,
    ) -> Result<T> {
        let outer = self.guard.replace(filter);
        let result = write(self);
        self.guard = outer;
        result
    }

    /// Whether live document `id` matches `filter`. `NotFound` if there's none.
    pub fn matches(&self, id: &str, filter: &Map<String, Value>) -> Result<bool> {
        let (row, doc_type): (i64, String) = self
            .conn
            .query_row("SELECT doc, type FROM documents WHERE id = ?1 AND deleted IS NULL", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?
            .ok_or_else(|| Error::NotFound(id.into()))?;
        let q = Query { filter: filter.clone(), ..Query::all(doc_type) };
        self.any_match(&q, &[row])
    }

    /// Who created document `id` (live or deleted), if the store knows.
    pub fn creator(&self, id: &str) -> Result<Option<String>> {
        self.conn
            .query_row("SELECT creator FROM documents WHERE id = ?1", [id], |r| r.get(0))
            .optional()?
            .ok_or_else(|| Error::NotFound(id.into()))
    }

    /// Run `sql` and return its rows' first column, for tests and `--explain`.
    /// This device's Loro peer id, which also names its backups.
    pub fn peer(&self) -> u64 {
        self.peer
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    fn new_loro(&self) -> Result<LoroDoc> {
        let doc = LoroDoc::new();
        doc.set_peer_id(self.peer)?;
        Ok(doc)
    }
}

/// A live document's row number, type and creation time.
fn live_row(tx: &Transaction, id: &str) -> Result<Option<(i64, String, i64)>> {
    Ok(tx
        .query_row("SELECT doc, type, created FROM documents WHERE id = ?1 AND deleted IS NULL", [id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()?)
}

fn load(tx: &Transaction, row: i64, peer: u64) -> Result<LoroDoc> {
    let snapshot: Vec<u8> =
        tx.query_row("SELECT snapshot FROM doc_state WHERE doc = ?1", [row], |r| r.get(0))?;
    let mut parts = vec![snapshot];
    let mut stmt = tx.prepare_cached("SELECT bytes FROM changes WHERE doc = ?1 ORDER BY seq")?;
    for bytes in stmt.query_map([row], |r| r.get::<_, Vec<u8>>(0))? {
        parts.push(bytes?);
    }
    let doc = LoroDoc::new();
    doc.import_batch(&parts)?;
    doc.set_peer_id(peer)?;
    Ok(doc)
}

/// Fold the logged updates into a new snapshot. Loro keeps the full history inside
/// the snapshot, so nothing is lost.
fn compact(tx: &Transaction, row: i64, doc: &LoroDoc) -> Result<()> {
    tx.execute(
        "UPDATE doc_state SET snapshot = ?2, updates = 0 WHERE doc = ?1",
        params![row, doc.export(ExportMode::Snapshot)?],
    )?;
    tx.execute("DELETE FROM changes WHERE doc = ?1", [row])?;
    Ok(())
}

/// The database's schema version (`PRAGMA user_version`).
fn schema_version(conn: &Connection) -> Result<i64> {
    Ok(conn.pragma_query_value(None, "user_version", |r| r.get(0))?)
}

/// Bring `conn`'s schema up to [`SCHEMA_VERSION`], creating it if it's empty.
fn migrate(conn: &Connection) -> Result<()> {
    let version = schema_version(conn)?;
    if version > SCHEMA_VERSION {
        return invalid(format!(
            "database schema version {version} is newer than this build ({SCHEMA_VERSION})"
        ));
    }
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    if version == 0 {
        tx.execute_batch(SCHEMA)?;
        // The random half of a UUIDv7: unique enough to tell devices apart.
        let peer = uuid::Uuid::now_v7().as_u64_pair().1 >> 1;
        tx.execute("INSERT INTO meta (key, value) VALUES ('peer', ?1)", [peer as i64])?;
    }
    for migration in &MIGRATIONS[(version.max(1) - 1) as usize..] {
        tx.execute_batch(migration)?;
    }
    if (1..3).contains(&version) {
        refs::backfill(&tx)?;
    }
    if (3..5).contains(&version) {
        retention::backfill(&tx, read_peer(&tx)?)?;
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}

fn read_peer(conn: &Connection) -> Result<u64> {
    let peer: i64 = conn.query_row("SELECT value FROM meta WHERE key = 'peer'", [], |r| r.get(0))?;
    Ok(peer as u64)
}

/// Refuse the write in `tx` if document `row` no longer matches `guard`.
fn check_guard(tx: &Transaction, row: i64, doc_type: &str, guard: Option<&Map<String, Value>>) -> Result<()> {
    let Some(filter) = guard else { return Ok(()) };
    let q = Query { filter: filter.clone(), ..Query::all(doc_type) };
    let probe = query::compile_probe(&q, &[row])?;
    let ok = tx.prepare_cached(&probe.sql)?.exists(rusqlite::params_from_iter(&probe.params))?;
    if ok {
        Ok(())
    } else {
        Err(Error::Denied(format!("the {doc_type} would be outside what this app may write")))
    }
}

fn change_set(tx: &Transaction, at: i64, origin: &str, undoes: Option<i64>) -> Result<i64> {
    tx.execute(
        "INSERT INTO change_sets (at, origin, undoes) VALUES (?1, ?2, ?3)",
        params![at, origin, undoes],
    )?;
    Ok(tx.last_insert_rowid())
}

/// What a change does to a document, as recorded in `doc_versions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Create,
    Update,
    Delete,
    Restore,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Create => "create",
            Kind::Update => "update",
            Kind::Delete => "delete",
            Kind::Restore => "restore",
        }
    }

    fn parse(text: &str) -> Result<Kind> {
        Ok(match text {
            "create" => Kind::Create,
            "update" => Kind::Update,
            "delete" => Kind::Delete,
            "restore" => Kind::Restore,
            other => return invalid(format!("unknown change kind {other:?} in the database")),
        })
    }
}

fn record_version(
    tx: &Transaction,
    row: i64,
    set: i64,
    kind: Kind,
    before: &Frontiers,
    after: &Frontiers,
) -> Result<()> {
    tx.execute(
        "INSERT INTO doc_versions (doc, change_set, kind, before, after) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![row, set, kind.as_str(), before.encode(), after.encode()],
    )?;
    Ok(())
}

/// A document's version before an edit, to log the edit's update and record it.
struct Before {
    vv: loro::VersionVector,
    frontiers: Frontiers,
}

impl Before {
    fn of(doc: &LoroDoc) -> Before {
        Before { vv: doc.oplog_vv(), frontiers: doc.oplog_frontiers() }
    }
}

/// Log the edit made to `doc` since `before` as part of change set `set`: its Loro
/// update, its version, the new JSON copy and search row.
fn log_change(
    tx: &Transaction,
    row: i64,
    doc_type: &str,
    doc: &LoroDoc,
    before: &Before,
    set: i64,
    at: i64,
) -> Result<()> {
    let json = checked_json(doc)?;
    let bytes = doc.export(ExportMode::updates(&before.vv))?;
    tx.execute("INSERT INTO changes (doc, change_set, bytes) VALUES (?1, ?2, ?3)", params![row, set, bytes])?;
    tx.execute("UPDATE documents SET updated = ?2 WHERE doc = ?1", params![row, at])?;
    let logged: i64 = tx.query_row(
        "UPDATE doc_state SET updates = updates + 1 WHERE doc = ?1 RETURNING updates",
        [row],
        |r| r.get(0),
    )?;
    if logged >= COMPACT_AFTER {
        compact(tx, row, doc)?;
    }
    record_version(tx, row, set, Kind::Update, &before.frontiers, &doc.oplog_frontiers())?;
    write_views(tx, row, doc_type, &json, true)?;
    retention::record_blobs(tx, row, set)
}

/// Take a document out of queries and search, keeping its state.
fn delete_row(tx: &Transaction, row: i64, set: i64, at: i64, version: &Frontiers) -> Result<()> {
    tx.execute("UPDATE documents SET deleted = ?2, updated = ?2 WHERE doc = ?1", params![row, at])?;
    tx.execute("DELETE FROM doc_json WHERE doc = ?1", [row])?;
    tx.execute("DELETE FROM fts WHERE rowid = ?1", [row])?;
    // The deleted state still refers to its blobs, for undo.
    record_version(tx, row, set, Kind::Delete, version, version)?;
    retention::record_blobs(tx, row, set)?;
    refs::clear(tx, row)
}

/// Bring a deleted document back into queries and search.
fn restore_row(tx: &Transaction, row: i64, doc_type: &str, doc: &LoroDoc, set: i64, at: i64) -> Result<()> {
    let json = checked_json(doc)?;
    tx.execute("UPDATE documents SET deleted = NULL, updated = ?2 WHERE doc = ?1", params![row, at])?;
    let version = doc.oplog_frontiers();
    record_version(tx, row, set, Kind::Restore, &version, &version)?;
    write_views(tx, row, doc_type, &json, false)?;
    retention::record_blobs(tx, row, set)
}

/// The document's JSON copy, refusing ones too big to store.
fn checked_json(doc: &LoroDoc) -> Result<Value> {
    let json = Value::Object(loro_json::read(doc));
    let len = serde_json::to_vec(&json).map(|v| v.len()).unwrap_or(usize::MAX);
    if len > MAX_DOC_BYTES {
        return invalid(format!("document is {len} bytes; the most is {MAX_DOC_BYTES} (use a blob)"));
    }
    Ok(json)
}

/// Write everything derived from a live document's data: its JSON copy, search row,
/// links and blob references. Checks the data against its type's system schema, if
/// it has one, and its links and blob references, first.
fn write_views(tx: &Transaction, row: i64, doc_type: &str, json: &Value, replace: bool) -> Result<()> {
    if let Some(schema) = schemas::get(doc_type) {
        schema.validate(json).map_err(|why| Error::Invalid(format!("not a valid {doc_type}: {why}")))?;
    }
    refs::write(tx, row, json)?;
    tx.execute(
        "INSERT INTO doc_json (doc, type, json) VALUES (?1, ?2, ?3)
         ON CONFLICT (doc) DO UPDATE SET json = excluded.json",
        params![row, doc_type, json.to_string()],
    )?;
    if replace {
        tx.execute("DELETE FROM fts WHERE rowid = ?1", [row])?;
    }
    let mut words = Vec::new();
    loro_json::strings(json, &mut words);
    tx.execute("INSERT INTO fts (rowid, text) VALUES (?1, ?2)", params![row, words.join("\n")])?;
    Ok(())
}

fn doc_from_row(r: &rusqlite::Row) -> rusqlite::Result<Result<Doc>> {
    let json: String = r.get(4)?;
    let data = match serde_json::from_str(&json) {
        Ok(Value::Object(o)) => o,
        _ => return Ok(invalid("a stored JSON copy is not an object")),
    };
    Ok(Ok(Doc { id: r.get(0)?, doc_type: r.get(1)?, created: r.get(2)?, updated: r.get(3)?, data }))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
