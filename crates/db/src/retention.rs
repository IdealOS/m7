//! History compaction: thin old versions out, then delete the blobs nothing needs.
//!
//! Every version of the last [`Retention::keep_all`] stays. Older than that, a
//! document keeps its last version of each day; older than
//! [`Retention::keep_daily`], its last version of each week (weeks start on
//! Monday, UTC). A document's creation and pinned versions always stay.
//!
//! Thinning drops rows from `doc_versions`, which is what history lists and undo
//! works from. It doesn't shrink documents: Loro keeps every operation and can cut
//! history only at one point, so the space it frees is in blobs. Each version
//! records the blobs its data refers to (`version_blobs`), so once a version is
//! gone, a blob no live document and no kept version uses can be deleted.
//!
//! The kept versions cover every state a document was in during the last
//! `keep_all`, so a blob a recent snapshot refers to is never deleted, and
//! restoring one of dbd's snapshots never finds a blob missing. A deleted document
//! keeps its last version (for undo), so it keeps its blobs too.
//!
//! The Pi has no clock that survives power-off: until the network sets the time,
//! timestamps can be in 1970. Versions and blobs dated before [`CLOCK_FLOOR`] are
//! never thinned or deleted, since their real age is unknown.

use std::time::SystemTime;

use loro::Frontiers;
use rusqlite::{Transaction, params};

use crate::refs::{self, Ref};
use crate::{Blobs, Error, Result, Store, load, loro_json};

pub const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// 2025-01-01 UTC. Anything dated earlier was written with the clock unset.
pub const CLOCK_FLOOR: i64 = 1_735_689_600_000;

/// How long history is kept in full and day by day, and how old an unused blob
/// must be before it's deleted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Retention {
    /// Every version younger than this stays (ms).
    pub keep_all: i64,
    /// Older than `keep_all` but younger than this, the last version of each day
    /// stays; older still, the last of each week (ms).
    pub keep_daily: i64,
    /// An unused blob, or a blob file the database doesn't know, is deleted only
    /// once it's this old, so an upload that hasn't been referred to yet stays (ms).
    pub blob_grace: i64,
}

impl Default for Retention {
    /// 30 days in full, daily for a year, weekly after that; blobs a day old.
    fn default() -> Retention {
        Retention { keep_all: 30 * DAY_MS, keep_daily: 365 * DAY_MS, blob_grace: DAY_MS }
    }
}

/// What a compaction removed.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Compacted {
    /// Rows of `doc_versions`: one document's state after one change set.
    pub versions: u64,
    /// Change sets with no versions left.
    pub change_sets: u64,
    /// Blobs deleted, including files the database didn't know about.
    pub blobs: u64,
    pub blob_bytes: u64,
}

/// Thin history in `store` as of `now` (ms since the epoch), then delete the blobs
/// in `blobs` that nothing needs any more.
pub fn compact_history(store: &mut Store, blobs: &Blobs, now: i64, r: &Retention) -> Result<Compacted> {
    let (versions, change_sets) = store.thin_history(now, r)?;
    let mut out = Compacted { versions, change_sets, ..Compacted::default() };
    let before = now - r.blob_grace;
    for (hash, size) in store.unused_blobs(before)? {
        // Forget it first: a crash before the file goes leaves a file the database
        // doesn't know, which the next pass deletes.
        if store.forget_blob(&hash)? {
            blobs.remove(&hash)?;
            out.blobs += 1;
            out.blob_bytes += size;
        }
    }
    for (hash, modified, size) in blobs.files()? {
        let old = modified
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| (d.as_millis() as i64) < before)
            .unwrap_or(false);
        if old && !store.knows_blob(&hash)? {
            blobs.remove(&hash)?;
            out.blobs += 1;
            out.blob_bytes += size;
        }
    }
    Ok(out)
}

impl Store {
    /// Pin or unpin document `id`'s version after change set `change_set`. A
    /// pinned version is never thinned out.
    pub fn pin(&mut self, id: &str, change_set: i64, pinned: bool) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE doc_versions SET pinned = ?3
             WHERE doc = (SELECT doc FROM documents WHERE id = ?1) AND change_set = ?2",
            params![id, change_set, pinned],
        )?;
        if n == 0 {
            return Err(Error::NotFound(format!("version {change_set} of document {id}")));
        }
        Ok(())
    }

    /// Drop the versions `r` doesn't keep as of `now`, then the change sets left
    /// with none. Returns how many of each went.
    pub fn thin_history(&mut self, now: i64, r: &Retention) -> Result<(u64, u64)> {
        let tx = self.conn.transaction()?;
        // Each version's bucket: its UTC day while younger than keep_daily, its week
        // (negative, so the two never collide) after. Day 0 of the epoch was a
        // Thursday; shifting by three days starts weeks on Monday.
        let versions = tx.execute(
            "DELETE FROM doc_versions WHERE rowid IN (
                SELECT rid FROM (
                    SELECT v.rowid AS rid, v.kind, v.pinned, c.at,
                        row_number() OVER (
                            PARTITION BY v.doc, CASE WHEN ?1 - c.at < ?3 THEN c.at / ?5
                                                     ELSE -1 - (c.at + 3 * ?5) / (7 * ?5) END
                            ORDER BY v.change_set DESC) AS rn
                    FROM doc_versions v JOIN change_sets c ON c.id = v.change_set)
                WHERE rn > 1 AND ?1 - at >= ?2 AND at >= ?4 AND kind <> 'create' AND pinned = 0)",
            params![now, r.keep_all, r.keep_daily, CLOCK_FLOOR, DAY_MS],
        )? as u64;
        // A change set with no versions is gone from history, unless a logged
        // update or an undo still points at it. The newest stays: ids are
        // allocated after it, and a new change set must not reuse an old id.
        let change_sets = tx.execute(
            "DELETE FROM change_sets WHERE id < (SELECT max(id) FROM change_sets)
                AND id NOT IN (SELECT change_set FROM doc_versions)
                AND id NOT IN (SELECT change_set FROM changes)
                AND id NOT IN (SELECT undoes FROM change_sets WHERE undoes IS NOT NULL)",
            [],
        )? as u64;
        tx.commit()?;
        Ok((versions, change_sets))
    }

    /// Blobs stored before `before` that no live document and no kept version
    /// refers to, with their sizes.
    pub fn unused_blobs(&self, before: i64) -> Result<Vec<(String, u64)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT hash, size FROM blobs b WHERE created < ?1 AND created >= ?2
                AND NOT EXISTS (SELECT 1 FROM blob_refs r WHERE r.hash = b.hash)
                AND NOT EXISTS (SELECT 1 FROM version_blobs v WHERE v.hash = b.hash)
             ORDER BY hash",
        )?;
        let rows =
            stmt.query_map(params![before, CLOCK_FLOOR], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Forget blob `hash` if still nothing refers to it. Returns whether it went.
    fn forget_blob(&mut self, hash: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "DELETE FROM blobs WHERE hash = ?1
                AND NOT EXISTS (SELECT 1 FROM blob_refs WHERE hash = ?1)
                AND NOT EXISTS (SELECT 1 FROM version_blobs WHERE hash = ?1)",
            [hash],
        )? > 0)
    }

    fn knows_blob(&self, hash: &str) -> Result<bool> {
        Ok(self.conn.prepare_cached("SELECT 1 FROM blobs WHERE hash = ?1")?.exists([hash])?)
    }
}

/// Record the blobs document `row` refers to now as the ones its version after
/// change set `set` uses. Call it once the document's `blob_refs` are up to date
/// (and, for a delete, before they're cleared).
pub(crate) fn record_blobs(tx: &Transaction, row: i64, set: i64) -> Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO version_blobs (doc, change_set, hash)
         SELECT DISTINCT doc, ?2, hash FROM blob_refs WHERE doc = ?1",
        params![row, set],
    )?;
    Ok(())
}

/// Fill `version_blobs` for versions recorded before schema version 5, by reading
/// each one back. Nothing to do if no blobs were ever stored.
pub(crate) fn backfill(tx: &Transaction, peer: u64) -> Result<()> {
    let any: bool = tx.prepare("SELECT 1 FROM blobs")?.exists([])?;
    if !any {
        return Ok(());
    }
    let versions: Vec<(i64, i64, Vec<u8>)> = tx
        .prepare("SELECT doc, change_set, after FROM doc_versions ORDER BY doc, change_set")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut current: Option<(i64, loro::LoroDoc)> = None;
    for (row, set, after) in versions {
        if current.as_ref().is_none_or(|(r, _)| *r != row) {
            current = Some((row, load(tx, row, peer)?));
        }
        let (_, doc) = current.as_ref().expect("just loaded");
        let data = serde_json::Value::Object(loro_json::read(&doc.fork_at(&Frontiers::decode(&after)?)?));
        for r in refs::find(&data).unwrap_or_default() {
            if let Ref::Blob { hash, .. } = r {
                tx.execute(
                    "INSERT OR IGNORE INTO version_blobs (doc, change_set, hash)
                     SELECT ?1, ?2, hash FROM blobs WHERE hash = ?3",
                    params![row, set, hash],
                )?;
            }
        }
    }
    Ok(())
}
