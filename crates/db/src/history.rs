//! History and undo, built on change sets and the Loro versions recorded for each.
//!
//! Every write is a change set; `doc_versions` keeps, for each document it touched,
//! the Loro version before and after. Loro keeps every operation (compaction folds
//! updates into the snapshot without dropping any), so any recorded version can be
//! read back with `fork_at`.
//!
//! Undoing a change set writes a new change set (marked `undoes`), so the undo is in
//! history too and can itself be undone. For an update it applies the Loro diff
//! from the version after back to the version before onto the current state: what
//! that change did is reverted and later changes to other fields stay. Undoing a
//! create deletes the document, and undoing a delete restores it.

use loro::{Frontiers, LoroDoc};
use rusqlite::{OptionalExtension, params};
use serde_json::{Map, Value};

use crate::{
    Before, Error, Kind, Result, Store, change_set, delete_row, invalid, load, log_change, loro_json, now_ms,
    restore_row,
};

/// One change set's effect on one document.
#[derive(Debug, Clone, PartialEq)]
pub struct Version {
    pub change_set: i64,
    /// When, in milliseconds since the epoch.
    pub at: i64,
    /// Who: the service name or app id that made it.
    pub origin: String,
    pub kind: Kind,
    /// The change set this one undid, if it was an undo.
    pub undoes: Option<i64>,
}

/// A change set: one write, possibly to several documents.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangeSet {
    pub id: i64,
    pub at: i64,
    pub origin: String,
    pub undoes: Option<i64>,
    /// Ids of the documents it touched.
    pub docs: Vec<String>,
}

/// A document a change set touched, as `undo` reads it.
struct Touched {
    row: i64,
    id: String,
    doc_type: String,
    deleted: bool,
    kind: String,
    before: Vec<u8>,
    after: Vec<u8>,
}

impl Store {
    /// Every recorded change to document `id`, oldest first (deleted documents too).
    pub fn history(&self, id: &str) -> Result<Vec<Version>> {
        let row = self.row_of(id)?;
        let mut stmt = self.conn.prepare_cached(
            "SELECT v.change_set, c.at, c.origin, v.kind, c.undoes
             FROM doc_versions v JOIN change_sets c ON c.id = v.change_set
             WHERE v.doc = ?1 ORDER BY v.change_set",
        )?;
        let rows = stmt
            .query_map([row], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, String>(3)?, r.get(4)?)))?;
        rows.map(|r| {
            let (change_set, at, origin, kind, undoes) = r?;
            Ok(Version { change_set, at, origin, kind: Kind::parse(&kind)?, undoes })
        })
        .collect()
    }

    /// Document `id`'s data just after change set `change_set`.
    pub fn at(&self, id: &str, change_set: i64) -> Result<Map<String, Value>> {
        let row = self.row_of(id)?;
        let after: Vec<u8> = self
            .conn
            .query_row(
                "SELECT after FROM doc_versions WHERE doc = ?1 AND change_set = ?2",
                params![row, change_set],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| Error::Invalid(format!("change set {change_set} didn't touch document {id}")))?;
        let doc = self.loro(id)?;
        Ok(loro_json::read(&doc.fork_at(&Frontiers::decode(&after)?)?))
    }

    /// The most recent change sets, newest first.
    pub fn change_sets(&self, limit: u32) -> Result<Vec<ChangeSet>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id, at, origin, undoes FROM change_sets ORDER BY id DESC LIMIT ?1")?;
        let sets: Vec<(i64, i64, String, Option<i64>)> = stmt
            .query_map([limit.min(protocol::db::MAX_QUERY_LIMIT)], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut docs = self.conn.prepare_cached(
            "SELECT d.id FROM doc_versions v JOIN documents d ON d.doc = v.doc WHERE v.change_set = ?1 ORDER BY d.doc",
        )?;
        sets.into_iter()
            .map(|(id, at, origin, undoes)| {
                let ids = docs.query_map([id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
                Ok(ChangeSet { id, at, origin, undoes, docs: ids })
            })
            .collect()
    }

    /// Undo change set `target`, as a new change set made by `origin`. Returns the
    /// new change set's id.
    pub fn undo(&mut self, target: i64, origin: &str) -> Result<i64> {
        let peer = self.peer;
        let tx = self.conn.transaction()?;
        let versions: Vec<Touched> = tx
            .prepare(
                "SELECT d.doc, d.id, d.type, d.deleted, v.kind, v.before, v.after
                 FROM doc_versions v JOIN documents d ON d.doc = v.doc
                 WHERE v.change_set = ?1 ORDER BY d.doc",
            )?
            .query_map([target], |r| {
                Ok(Touched {
                    row: r.get(0)?,
                    id: r.get(1)?,
                    doc_type: r.get(2)?,
                    deleted: r.get::<_, Option<i64>>(3)?.is_some(),
                    kind: r.get(4)?,
                    before: r.get(5)?,
                    after: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        if versions.is_empty() {
            return invalid(format!("change set {target} has no recorded changes to undo"));
        }
        let now = now_ms();
        let set = change_set(&tx, now, origin, Some(target))?;
        for Touched { row, id, doc_type, deleted, kind, before, after } in versions {
            let doc: LoroDoc = load(&tx, row, peer)?;
            match (Kind::parse(&kind)?, deleted) {
                // Undoing a create or a restore deletes, if it's still there.
                (Kind::Create | Kind::Restore, false) => {
                    delete_row(&tx, row, set, now, &doc.oplog_frontiers())?
                }
                // Undoing a delete restores, if it's still gone.
                (Kind::Delete, true) => restore_row(&tx, row, &doc_type, &doc, set, now)?,
                (Kind::Create | Kind::Restore, true) | (Kind::Delete, false) => {}
                (Kind::Update, true) => {
                    return invalid(format!(
                        "document {id} was deleted after change set {target}; undo the delete first"
                    ));
                }
                (Kind::Update, false) => {
                    let start = Before::of(&doc);
                    let was = loro_json::read(&doc);
                    let revert = doc.diff(&Frontiers::decode(&after)?, &Frontiers::decode(&before)?)?;
                    doc.apply_diff(revert)?;
                    doc.commit();
                    // Reverting can write ops that set keys to the values they already
                    // have (a later change got there first); those aren't a change.
                    if loro_json::read(&doc) != was {
                        log_change(&tx, row, &doc_type, &doc, &start, set, now)?;
                    }
                }
            }
        }
        let recorded: i64 =
            tx.query_row("SELECT count(*) FROM doc_versions WHERE change_set = ?1", [set], |r| r.get(0))?;
        if recorded == 0 {
            // Dropping the transaction forgets the empty change set.
            return invalid(format!("change set {target}'s effects are already gone"));
        }
        tx.commit()?;
        Ok(set)
    }

    fn row_of(&self, id: &str) -> Result<i64> {
        self.conn
            .query_row("SELECT doc FROM documents WHERE id = ?1", [id], |r| r.get(0))
            .optional()?
            .ok_or_else(|| Error::NotFound(id.into()))
    }
}

/// One field that differs between two versions of a document. Objects are compared
/// field by field; arrays and scalars as whole values.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldChange {
    pub path: String,
    /// `None` when the field didn't exist in the first version.
    pub before: Option<Value>,
    /// `None` when it doesn't exist in the second.
    pub after: Option<Value>,
}

impl Store {
    /// What changed in document `id` from just after change set `from` to just after
    /// change set `to` (0 for `from` means before it was created).
    pub fn diff(&self, id: &str, from: i64, to: i64) -> Result<Vec<FieldChange>> {
        let before = if from == 0 { Map::new() } else { self.at(id, from)? };
        let after = self.at(id, to)?;
        let mut out = Vec::new();
        diff_maps(&before, &after, "", &mut out);
        Ok(out)
    }
}

fn diff_maps(a: &Map<String, Value>, b: &Map<String, Value>, at: &str, out: &mut Vec<FieldChange>) {
    let path = |k: &str| if at.is_empty() { k.to_owned() } else { format!("{at}.{k}") };
    let mut keys: Vec<&String> = a.keys().chain(b.keys().filter(|k| !a.contains_key(*k))).collect();
    keys.sort();
    for k in keys {
        match (a.get(k), b.get(k)) {
            (Some(Value::Object(x)), Some(Value::Object(y))) => diff_maps(x, y, &path(k), out),
            (x, y) if x != y => {
                out.push(FieldChange { path: path(k), before: x.cloned(), after: y.cloned() })
            }
            _ => {}
        }
    }
}
