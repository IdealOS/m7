//! The blob store: immutable files named by the SHA-256 of their contents.
//!
//! Bulk data (attachments, audio, images, app bundles) doesn't go in documents. A
//! blob is stored once, as `<dir>/ab/cdef…` (its hash split after two characters),
//! and documents refer to it with `{"$blob": "<hash>"}`. The same bytes stored twice
//! are one blob. Files are written to a temporary name, synced, then renamed into
//! place read-only, so a blob is never seen half-written and never changes.
//!
//! The `blobs` table records which blobs exist; `blob_refs` (kept by every write)
//! records which live documents use them, and `version_blobs` which versions do.
//! History compaction ([`crate::retention`]) deletes the blobs neither needs.

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, params};

use crate::refs::valid_hash;
use crate::sha256::Sha256;
use crate::{Error, Result, Store, now_ms};

/// Largest blob the store takes.
pub const MAX_BLOB_BYTES: u64 = protocol::db::MAX_BLOB_BYTES;

/// A stored blob.
#[derive(Debug, Clone, PartialEq)]
pub struct BlobInfo {
    pub hash: String,
    pub size: u64,
    /// When it was first stored, in milliseconds since the epoch.
    pub created: i64,
    /// How many fields of live documents refer to it.
    pub refs: u64,
}

/// The directory blob files live in.
pub struct Blobs {
    dir: PathBuf,
}

impl Blobs {
    /// The blob store in `dir`, created if needed. Uploads a crash left half-written
    /// (`.incoming-*`) are deleted, so open it once, before any upload starts.
    pub fn new(dir: impl Into<PathBuf>) -> Result<Blobs> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(|e| io(&dir, e))?;
        for entry in std::fs::read_dir(&dir).map_err(|e| io(&dir, e))? {
            let entry = entry.map_err(|e| io(&dir, e))?;
            if entry.file_name().to_string_lossy().starts_with(".incoming-") {
                let path = entry.path();
                std::fs::remove_file(&path).map_err(|e| io(&path, e))?;
            }
        }
        Ok(Blobs { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where blob `hash` is (or would be) stored.
    pub fn path(&self, hash: &str) -> Result<PathBuf> {
        if !valid_hash(hash) {
            return Err(Error::Invalid(format!("not a blob hash: {hash:?}")));
        }
        Ok(self.dir.join(&hash[..2]).join(&hash[2..]))
    }

    /// Store everything `from` holds, at most `max` bytes, and return its hash and
    /// size. Storing bytes that are already stored just returns their hash.
    pub fn write(&self, from: &mut impl Read, max: u64) -> Result<(String, u64)> {
        let tmp = self.dir.join(format!(".incoming-{}", uuid::Uuid::now_v7()));
        let result = self.write_tmp(from, max, &tmp);
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    fn write_tmp(&self, from: &mut impl Read, max: u64, tmp: &Path) -> Result<(String, u64)> {
        let mut file = File::create(tmp).map_err(|e| io(tmp, e))?;
        let mut hasher = Sha256::default();
        let mut size = 0u64;
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = from.read(&mut buf).map_err(|e| Error::Io(format!("reading the blob: {e}")))?;
            if n == 0 {
                break;
            }
            size += n as u64;
            if size > max {
                return Err(Error::Invalid(format!("blob is over {max} bytes")));
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n]).map_err(|e| io(tmp, e))?;
        }
        file.sync_all().map_err(|e| io(tmp, e))?;
        drop(file);
        let hash = hasher.hex();
        let path = self.path(&hash)?;
        if path.exists() {
            std::fs::remove_file(tmp).map_err(|e| io(tmp, e))?;
            return Ok((hash, size));
        }
        let parent = path.parent().expect("blob paths have a directory");
        std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
        std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(0o444)).map_err(|e| io(tmp, e))?;
        std::fs::rename(tmp, &path).map_err(|e| io(&path, e))?;
        Ok((hash, size))
    }

    /// Delete blob `hash`'s file, if it's there. Only history compaction does this,
    /// once the database has forgotten the blob.
    pub fn remove(&self, hash: &str) -> Result<()> {
        let path = self.path(hash)?;
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(&path, e)),
        }
        // An emptied `ab/` directory goes too; one still holding blobs stays.
        let _ = std::fs::remove_dir(path.parent().expect("blob paths have a directory"));
        Ok(())
    }

    /// Every blob file: its hash, when it was last modified, and its size.
    /// Anything that isn't named like a blob is left out.
    pub fn files(&self) -> Result<Vec<(String, std::time::SystemTime, u64)>> {
        let mut out = Vec::new();
        for dir in std::fs::read_dir(&self.dir).map_err(|e| io(&self.dir, e))? {
            let dir = dir.map_err(|e| io(&self.dir, e))?;
            let prefix = dir.file_name().to_string_lossy().into_owned();
            if prefix.len() != 2 || !dir.file_type().map_err(|e| io(&dir.path(), e))?.is_dir() {
                continue;
            }
            for file in std::fs::read_dir(dir.path()).map_err(|e| io(&dir.path(), e))? {
                let file = file.map_err(|e| io(&dir.path(), e))?;
                let hash = format!("{prefix}{}", file.file_name().to_string_lossy());
                if !valid_hash(&hash) {
                    continue;
                }
                let meta = file.metadata().map_err(|e| io(&file.path(), e))?;
                if meta.is_file() {
                    let modified = meta.modified().map_err(|e| io(&file.path(), e))?;
                    out.push((hash, modified, meta.len()));
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// Open blob `hash` for reading.
    pub fn open(&self, hash: &str) -> Result<File> {
        let path = self.path(hash)?;
        File::open(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Error::NotFound(format!("blob {hash}")),
            _ => io(&path, e),
        })
    }
}

impl Store {
    /// Record that blob `hash` of `size` bytes is stored (after [`Blobs::write`]), so
    /// documents may refer to it. Recording it again changes nothing.
    pub fn add_blob(&mut self, hash: &str, size: u64) -> Result<BlobInfo> {
        if !valid_hash(hash) {
            return Err(Error::Invalid(format!("not a blob hash: {hash:?}")));
        }
        self.conn.execute(
            "INSERT INTO blobs (hash, size, created) VALUES (?1, ?2, ?3) ON CONFLICT (hash) DO NOTHING",
            params![hash, size as i64, now_ms()],
        )?;
        self.blob_info(hash)
    }

    /// The live documents that refer to blob `hash`.
    pub fn blob_docs(&self, hash: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT DISTINCT d.id FROM blob_refs b JOIN documents d ON d.doc = b.doc
             WHERE b.hash = ?1 AND d.deleted IS NULL ORDER BY d.doc",
        )?;
        Ok(stmt.query_map([hash], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    /// Blob `hash`'s size, age and how many live document fields refer to it.
    pub fn blob_info(&self, hash: &str) -> Result<BlobInfo> {
        self.conn
            .query_row(
                "SELECT size, created, (SELECT count(*) FROM blob_refs WHERE hash = ?1) FROM blobs WHERE hash = ?1",
                [hash],
                |r| {
                    Ok(BlobInfo {
                        hash: hash.into(),
                        size: r.get::<_, i64>(0)? as u64,
                        created: r.get(1)?,
                        refs: r.get::<_, i64>(2)? as u64,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("blob {hash}")))
    }
}

fn io(path: &Path, e: std::io::Error) -> Error {
    Error::Io(format!("{}: {e}", path.display()))
}
