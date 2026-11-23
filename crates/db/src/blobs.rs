//! The blob store: immutable files named by the SHA-256 of their contents.
//!
//! Bulk data (attachments, audio, images, app bundles) doesn't go in documents. A
//! blob is stored once, as `<dir>/ab/cdef…` (its hash split after two characters),
//! and documents refer to it with `{"$blob": "<hash>"}`. The same bytes stored twice
//! are one blob. Files are written to a temporary name, synced, then renamed into
//! place read-only, so a blob is never seen half-written and never changes.
//!
//! The `blobs` table records which blobs exist; `blob_refs` (kept by every write)
//! records which live documents use them. Deleting unused blobs waits for history
//! compaction (M8): a blob an old version refers to must stay for undo.

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
    pub fn new(dir: impl Into<PathBuf>) -> Result<Blobs> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(|e| io(&dir, e))?;
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
