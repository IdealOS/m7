//! Snapshots: whole copies of the database, taken while it's in use.
//!
//! A snapshot is a SQLite file made with the online backup API, so it's a
//! consistent copy even while writes go on. Blobs are immutable files named by
//! their hash, so a snapshot only needs the database. Snapshots are named
//! `snapshot-<taken ms>-<newest change set>.db` and live in one directory, by
//! default next to the database.
//!
//! Restoring copies a snapshot back over the live database (the backup API again),
//! after first taking a snapshot of the current state, so a restore can itself be
//! undone. A snapshot from an older build is migrated as it's restored; one from a
//! newer build is refused.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, MAIN_DB, OpenFlags, OptionalExtension};

use crate::{Error, Result, SCHEMA_VERSION, Store, migrate, now_ms, read_peer, schema_version, schemas};

/// How many snapshots [`Snapshots::prune`] keeps by default: two days of hourly ones.
pub const KEEP: usize = 48;

/// One snapshot on disk.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotInfo {
    /// The file name, which is how a snapshot is restored.
    pub name: String,
    /// When it was taken, in milliseconds since the epoch.
    pub taken: i64,
    /// The newest change set it holds.
    pub change_set: i64,
    pub bytes: u64,
}

/// The snapshot directory.
pub struct Snapshots {
    dir: PathBuf,
}

impl Snapshots {
    /// Snapshots kept in `dir`, which is created if needed.
    pub fn new(dir: impl Into<PathBuf>) -> Result<Snapshots> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(|e| io(&dir, e))?;
        Ok(Snapshots { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Take a snapshot of `store`, unless the newest one already holds exactly its
    /// current state; either way, return that snapshot.
    pub fn take(&self, store: &Store) -> Result<SnapshotInfo> {
        let newest = store.newest_change_set()?;
        let at_of = |conn: &Connection| -> Result<Option<i64>> {
            Ok(conn
                .query_row("SELECT at FROM change_sets WHERE id = ?1", [newest], |r| r.get(0))
                .optional()?)
        };
        if let Some(latest) = self.list()?.into_iter().next()
            && latest.change_set == newest
        {
            // The same change set id can come back after a restore, so check it's
            // the same change set too.
            let snap =
                Connection::open_with_flags(self.dir.join(&latest.name), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            if at_of(&snap)? == at_of(&store.conn)? {
                return Ok(latest);
            }
        }
        let mut taken = now_ms();
        // Two snapshots in the same millisecond would share a name.
        while self.dir.join(name(taken, newest)).exists() {
            taken += 1;
        }
        let final_path = self.dir.join(name(taken, newest));
        let tmp = self.dir.join(format!(".{}.tmp", name(taken, newest)));
        store.conn.backup(MAIN_DB, &tmp, rusqlite::backup::NO_PROGRESS)?;
        {
            // The copy keeps the live database's WAL mode; a snapshot is one
            // self-contained file, readable without -wal and -shm files.
            let copy = Connection::open(&tmp)?;
            copy.pragma_update(None, "journal_mode", "DELETE")?;
        }
        std::fs::rename(&tmp, &final_path).map_err(|e| io(&final_path, e))?;
        info(&final_path)?.ok_or_else(|| Error::Invalid("the snapshot vanished as it was written".into()))
    }

    /// Every snapshot, newest first.
    pub fn list(&self) -> Result<Vec<SnapshotInfo>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.dir).map_err(|e| io(&self.dir, e))? {
            let entry = entry.map_err(|e| io(&self.dir, e))?;
            if let Some(i) = info(&entry.path())? {
                out.push(i);
            }
        }
        out.sort_by(|a, b| b.taken.cmp(&a.taken).then(b.name.cmp(&a.name)));
        Ok(out)
    }

    /// Delete all but the newest `keep` snapshots. Returns how many were deleted.
    pub fn prune(&self, keep: usize) -> Result<usize> {
        let old = self.list()?.into_iter().skip(keep).collect::<Vec<_>>();
        for s in &old {
            let path = self.dir.join(&s.name);
            std::fs::remove_file(&path).map_err(|e| io(&path, e))?;
        }
        Ok(old.len())
    }

    /// Replace `store`'s contents with snapshot `name`, first taking a snapshot of
    /// what's there now. Returns that snapshot, so the restore can be undone.
    pub fn restore(&self, store: &mut Store, name: &str) -> Result<SnapshotInfo> {
        let Some(snap) = parse_name(name) else {
            return Err(Error::Invalid(format!("not a snapshot name: {name:?}")));
        };
        let path = self.dir.join(name);
        if info(&path)?.is_none() {
            return Err(Error::NotFound(format!("snapshot {name}")));
        }
        check(&path, store.peer)?;
        let saved = self.take(store)?;
        store.conn.restore(MAIN_DB, &path, rusqlite::backup::NO_PROGRESS)?;
        // The restored copy came from a DELETE-mode file; keep the live one in WAL.
        if store.conn.path().is_some_and(|p| !p.is_empty()) {
            store.conn.pragma_update(None, "journal_mode", "WAL")?;
        }
        migrate(&store.conn)?;
        schemas::sync_indexes(&store.conn)?;
        debug_assert_eq!(snap.1, store.newest_change_set()?);
        Ok(saved)
    }
}

/// Refuse a snapshot that is damaged, from a newer build, or from another device.
fn check(path: &Path, peer: u64) -> Result<()> {
    let snap = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let ok: String = snap.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if ok != "ok" {
        return Err(Error::Invalid(format!("the snapshot is damaged: {ok}")));
    }
    let version = schema_version(&snap)?;
    if version > SCHEMA_VERSION {
        return Err(Error::Invalid(format!(
            "the snapshot's schema version {version} is newer than this build ({SCHEMA_VERSION})"
        )));
    }
    if read_peer(&snap)? != peer {
        return Err(Error::Invalid("the snapshot is of another device's database".into()));
    }
    Ok(())
}

fn name(taken: i64, change_set: i64) -> String {
    format!("snapshot-{taken}-{change_set}.db")
}

/// (taken, change set) from a snapshot's file name, if it is one.
fn parse_name(name: &str) -> Option<(i64, i64)> {
    let (taken, set) = name.strip_prefix("snapshot-")?.strip_suffix(".db")?.split_once('-')?;
    let digits = |s: &str| !s.is_empty() && s.len() <= 19 && s.bytes().all(|b| b.is_ascii_digit());
    (digits(taken) && digits(set)).then(|| Some((taken.parse().ok()?, set.parse().ok()?)))?
}

/// The snapshot at `path`, if it's a snapshot file.
fn info(path: &Path) -> Result<Option<SnapshotInfo>> {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else { return Ok(None) };
    let Some((taken, change_set)) = parse_name(name) else { return Ok(None) };
    let meta = match std::fs::metadata(path) {
        Ok(m) if m.is_file() => m,
        Ok(_) => return Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io(path, e)),
    };
    Ok(Some(SnapshotInfo { name: name.into(), taken, change_set, bytes: meta.len() }))
}

fn io(path: &Path, e: std::io::Error) -> Error {
    Error::Io(format!("{}: {e}", path.display()))
}
