//! A drive's `idealos-backup` folder: writing backups into it, listing and pruning
//! them, and bringing one back. Blocking file work; the service runs it off the
//! bus's thread.
//!
//! Every file is written under a temporary name, synced, then renamed into place,
//! so a drive pulled out mid-copy leaves no half-written backup or blob, only a
//! `.incoming-*` file the next backup clears away. A blob is checked against its
//! name (its SHA-256) as it's copied, both ways, and a database copy is checked
//! (SQLite's `quick_check`) before it's kept.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use db::sha256::Sha256;
use db::snapshot::{self, SnapshotFacts};
use protocol::backup::{BackupInfo, FOLDER};

/// Why a backup-folder operation failed.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// No such backup.
    NotFound(String),
    /// Reading or writing failed, or something was damaged.
    Failed(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::NotFound(m) | Error::Failed(m) => f.write_str(m),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

fn io(path: &Path, e: std::io::Error) -> Error {
    Error::Failed(format!("{}: {e}", path.display()))
}

fn failed(e: db::Error) -> Error {
    Error::Failed(e.to_string())
}

/// Bytes copied so far and the total, as a copy goes.
pub type Progress<'a> = &'a mut dyn FnMut(u64, u64);

/// What one backup did.
#[derive(Debug, Clone, PartialEq)]
pub struct Written {
    pub backup: BackupInfo,
    pub blobs_copied: u64,
    pub bytes_copied: u64,
}

/// The `idealos-backup` folder on a mounted drive.
pub struct BackupSet {
    root: PathBuf,
}

impl BackupSet {
    /// The backup folder on the drive mounted at `mount`, if it has one.
    pub fn open(mount: &Path) -> Option<BackupSet> {
        let root = mount.join(FOLDER);
        root.is_dir().then_some(BackupSet { root })
    }

    /// Make the drive mounted at `mount` a backup drive (harmless if it is one).
    pub fn create(mount: &Path) -> Result<BackupSet> {
        let root = mount.join(FOLDER);
        std::fs::create_dir_all(root.join("blobs")).map_err(|e| io(&root, e))?;
        let readme = root.join("README.txt");
        if !readme.exists() {
            let text = "idealos backups. Each folder named in hex is one device's: database \
                        snapshots, newest last. blobs/ holds the files they refer to, named by \
                        their SHA-256. Restore from Settings > Backup on any idealos device.\n";
            std::fs::write(&readme, text).map_err(|e| io(&readme, e))?;
        }
        Ok(BackupSet { root })
    }

    /// Every backup on the drive, of every device, newest first.
    pub fn list(&self, this_device: &str) -> Result<Vec<BackupInfo>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.root).map_err(|e| io(&self.root, e))? {
            let entry = entry.map_err(|e| io(&self.root, e))?;
            let device = entry.file_name().to_string_lossy().into_owned();
            if !valid_device(&device) || !entry.path().is_dir() {
                continue;
            }
            for file in std::fs::read_dir(entry.path()).map_err(|e| io(&entry.path(), e))? {
                let file = file.map_err(|e| io(&entry.path(), e))?;
                let name = file.file_name().to_string_lossy().into_owned();
                let Some((taken, change_set)) = parse_backup(&name) else { continue };
                let bytes = file.metadata().map_err(|e| io(&file.path(), e))?.len();
                out.push(BackupInfo {
                    name: format!("{device}/{name}"),
                    this_device: device == this_device,
                    device: device.clone(),
                    taken,
                    change_set,
                    bytes,
                });
            }
        }
        out.sort_by(|a, b| b.taken.cmp(&a.taken).then(b.name.cmp(&a.name)));
        Ok(out)
    }

    /// Back up database snapshot `snapshot` (a file dbd wrote) and the blobs it
    /// records, read from the device's blob directory `blobs`. Blobs already on the
    /// drive aren't copied; a snapshot already backed up isn't copied again.
    pub fn back_up(&self, snapshot: &Path, blobs: &Path, progress: Progress) -> Result<Written> {
        let facts = snapshot::describe(snapshot).map_err(failed)?;
        let device = device_name(facts.peer);
        let file = snapshot.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let (taken, change_set) =
            snapshot::parse_name(file).ok_or_else(|| Error::Failed(format!("not a snapshot: {file}")))?;
        let dir = self.root.join(&device);
        let name = backup_name(taken, change_set);
        let target = dir.join(&name);
        let info = |bytes| BackupInfo {
            name: format!("{device}/{name}"),
            device: device.clone(),
            this_device: true,
            taken,
            change_set,
            bytes,
        };
        if let Ok(meta) = std::fs::metadata(&target) {
            return Ok(Written { backup: info(meta.len()), blobs_copied: 0, bytes_copied: 0 });
        }
        let drive_blobs = self.root.join("blobs");
        let missing: Vec<&(String, u64)> =
            facts.blobs.iter().filter(|(h, _)| !blob_path(&drive_blobs, h).exists()).collect();
        let db_bytes = std::fs::metadata(snapshot).map_err(|e| io(snapshot, e))?.len();
        let total = db_bytes + missing.iter().map(|(_, n)| n).sum::<u64>();
        let mut done = 0;
        for (hash, _) in &missing {
            let to = blob_path(&drive_blobs, hash);
            done += copy(&blob_path(blobs, hash), &to, &drive_blobs, Some(hash), &mut |n| {
                progress(done + n, total)
            })?;
        }
        std::fs::create_dir_all(&dir).map_err(|e| io(&dir, e))?;
        let bytes = copy_checked_db(snapshot, &target, &mut |n| progress(done + n, total))?;
        Ok(Written { backup: info(bytes), blobs_copied: missing.len() as u64, bytes_copied: done + bytes })
    }

    /// Keep `device`'s newest `keep` backups and delete the rest, then delete the
    /// blobs no remaining backup (of any device) records. Returns how many backups
    /// went. Stray `.incoming-*` files from an interrupted copy go too.
    pub fn prune(&self, device: &str, keep: usize) -> Result<u64> {
        let mut pruned = 0;
        for old in self.list(device)?.into_iter().filter(|b| b.device == device).skip(keep) {
            let path = self.root.join(&old.name);
            std::fs::remove_file(&path).map_err(|e| io(&path, e))?;
            pruned += 1;
        }
        let mut needed = std::collections::HashSet::new();
        for b in self.list(device)? {
            // A backup that can't be read keeps every blob: better too many than one
            // its database needs.
            let blobs = snapshot::blobs_of(&self.root.join(&b.name))
                .map_err(|e| Error::Failed(format!("{}: {e}", b.name)))?;
            needed.extend(blobs.into_iter().map(|(h, _)| h));
        }
        let dir = self.root.join("blobs");
        for (hash, path) in blob_files(&dir)? {
            if hash.as_ref().is_none_or(|h| !needed.contains(h)) {
                std::fs::remove_file(&path).map_err(|e| io(&path, e))?;
            }
        }
        for entry in std::fs::read_dir(&self.root).map_err(|e| io(&self.root, e))? {
            let path = entry.map_err(|e| io(&self.root, e))?.path();
            if path.is_dir() {
                remove_incoming(&path)?;
            }
        }
        Ok(pruned)
    }

    /// Bring back backup `name`'s blobs into the device's blob directory `blobs`
    /// (skipping ones it has), checking each, and return the backup's database copy
    /// and what it holds. Restoring the database itself is dbd's job.
    pub fn restore_blobs(
        &self,
        name: &str,
        blobs: &Path,
        progress: Progress,
    ) -> Result<(PathBuf, SnapshotFacts)> {
        let path = self.backup_path(name)?;
        let facts = snapshot::describe(&path).map_err(failed)?;
        let drive_blobs = self.root.join("blobs");
        let missing: Vec<&(String, u64)> =
            facts.blobs.iter().filter(|(h, _)| !blob_path(blobs, h).exists()).collect();
        let total = missing.iter().map(|(_, n)| n).sum::<u64>();
        let mut done = 0;
        for (hash, _) in missing {
            let from = blob_path(&drive_blobs, hash);
            if !from.exists() {
                return Err(Error::Failed(format!("blob {hash} is missing from the drive")));
            }
            let to = blob_path(blobs, hash);
            done += copy(&from, &to, blobs, Some(hash), &mut |n| progress(done + n, total))?;
            // As dbd stores them: read-only.
            set_read_only(&to)?;
        }
        Ok((path, facts))
    }

    /// The file of backup `name` (`<device>/backup-<ms>-<cs>.db`).
    pub fn backup_path(&self, name: &str) -> Result<PathBuf> {
        let ok = name
            .split_once('/')
            .is_some_and(|(device, file)| valid_device(device) && parse_backup(file).is_some());
        if !ok {
            return Err(Error::NotFound(format!("not a backup name: {name:?}")));
        }
        let path = self.root.join(name);
        if !path.is_file() {
            return Err(Error::NotFound(format!("no backup {name} on the drive")));
        }
        Ok(path)
    }
}

/// How a device's folder is named: its peer id in hex.
pub fn device_name(peer: u64) -> String {
    format!("{peer:016x}")
}

fn valid_device(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn backup_name(taken: i64, change_set: i64) -> String {
    format!("backup-{taken}-{change_set}.db")
}

/// (taken, change set) from a backup's file name.
fn parse_backup(name: &str) -> Option<(i64, i64)> {
    snapshot::parse_name(&format!("snapshot-{}", name.strip_prefix("backup-")?))
}

fn valid_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Where blob `hash` is in blob directory `dir`, laid out as dbd lays out its own.
pub fn blob_path(dir: &Path, hash: &str) -> PathBuf {
    dir.join(&hash[..2]).join(&hash[2..])
}

/// Every file in blob directory `dir`, with its hash if it's named like a blob.
fn blob_files(dir: &Path) -> Result<Vec<(Option<String>, PathBuf)>> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return Ok(out) };
    for sub in entries {
        let sub = sub.map_err(|e| io(dir, e))?;
        let prefix = sub.file_name().to_string_lossy().into_owned();
        if !sub.path().is_dir() {
            continue;
        }
        for file in std::fs::read_dir(sub.path()).map_err(|e| io(&sub.path(), e))? {
            let file = file.map_err(|e| io(&sub.path(), e))?;
            let hash = format!("{prefix}{}", file.file_name().to_string_lossy());
            out.push((valid_hash(&hash).then_some(hash), file.path()));
        }
    }
    Ok(out)
}

fn remove_incoming(dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(dir).map_err(|e| io(dir, e))? {
        let entry = entry.map_err(|e| io(dir, e))?;
        if entry.file_name().to_string_lossy().starts_with(".incoming-") {
            std::fs::remove_file(entry.path()).map_err(|e| io(&entry.path(), e))?;
        }
    }
    Ok(())
}

/// Copy `from` to `to` through a temporary file in `tmp_dir` (on the same file
/// system), checking the bytes' SHA-256 against `hash` if given. Returns the bytes
/// copied. Blobs go through the top of their blob directory, where dbd (and
/// pruning, on a drive) clears away what a crash leaves.
fn copy(
    from: &Path,
    to: &Path,
    tmp_dir: &Path,
    hash: Option<&str>,
    progress: &mut dyn FnMut(u64),
) -> Result<u64> {
    let dir = to.parent().expect("a file in a directory");
    std::fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
    let tmp = tmp_dir.join(format!(".incoming-{}", uuid::Uuid::now_v7()));
    let result = copy_to(from, &tmp, hash, progress).and_then(|n| {
        std::fs::rename(&tmp, to).map_err(|e| io(to, e))?;
        Ok(n)
    });
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn copy_to(from: &Path, tmp: &Path, hash: Option<&str>, progress: &mut dyn FnMut(u64)) -> Result<u64> {
    let mut src = File::open(from).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => Error::Failed(format!("{} is missing", from.display())),
        _ => io(from, e),
    })?;
    let mut dst = File::create(tmp).map_err(|e| io(tmp, e))?;
    let mut hasher = Sha256::default();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut n = 0u64;
    loop {
        let got = src.read(&mut buf).map_err(|e| io(from, e))?;
        if got == 0 {
            break;
        }
        if hash.is_some() {
            hasher.update(&buf[..got]);
        }
        dst.write_all(&buf[..got]).map_err(|e| io(tmp, e))?;
        n += got as u64;
        progress(n);
    }
    dst.sync_all().map_err(|e| io(tmp, e))?;
    if let Some(want) = hash
        && hasher.hex() != want
    {
        return Err(Error::Failed(format!(
            "{} is damaged: its contents don't match its name",
            from.display()
        )));
    }
    Ok(n)
}

/// Copy a database snapshot, keeping the copy only if SQLite finds it sound.
fn copy_checked_db(from: &Path, to: &Path, progress: &mut dyn FnMut(u64)) -> Result<u64> {
    let dir = to.parent().expect("a file in a directory");
    let tmp = dir.join(format!(".incoming-{}", uuid::Uuid::now_v7()));
    let result = copy_to(from, &tmp, None, progress).and_then(|n| {
        snapshot::describe(&tmp)
            .map_err(|e| Error::Failed(format!("the copy on the drive is damaged: {e}")))?;
        std::fs::rename(&tmp, to).map_err(|e| io(to, e))?;
        Ok(n)
    });
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Copy a backup's database into dbd's snapshot directory `snapshots`, named as a
/// snapshot so dbd's `restore` takes it. Returns that name.
pub fn place_snapshot(backup: &Path, snapshots: &Path) -> Result<String> {
    let file = backup.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let (taken, change_set) =
        parse_backup(file).ok_or_else(|| Error::NotFound(format!("not a backup: {file}")))?;
    let name = snapshot::name(taken, change_set);
    let to = snapshots.join(&name);
    if !to.exists() {
        copy(backup, &to, snapshots, None, &mut |_| {})?;
    }
    Ok(name)
}

#[cfg(unix)]
fn set_read_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o444)).map_err(|e| io(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);

    impl Tmp {
        fn new(name: &str) -> Tmp {
            let p = std::env::temp_dir().join(format!("backupd-set-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A device: a store with one blob per call to `add`, snapshots and blobs.
    struct Device {
        store: db::Store,
        snapshots: db::Snapshots,
        blobs: db::Blobs,
        dir: PathBuf,
    }

    impl Device {
        fn new(dir: PathBuf) -> Device {
            std::fs::create_dir_all(&dir).unwrap();
            Device {
                store: db::Store::open(&dir.join("db")).unwrap(),
                snapshots: db::Snapshots::new(dir.join("snapshots")).unwrap(),
                blobs: db::Blobs::new(dir.join("blobs")).unwrap(),
                dir,
            }
        }

        /// Store a document holding a new blob of `bytes`; return its hash.
        fn add(&mut self, bytes: &[u8]) -> String {
            let (h, n) = self.blobs.write(&mut &bytes[..], db::blobs::MAX_BLOB_BYTES).unwrap();
            self.store.add_blob(&h, n).unwrap();
            let data = serde_json::json!({"file": {"$blob": h}});
            self.store.create("Note", data.as_object().unwrap(), "t").unwrap();
            h
        }

        fn back_up(&self, set: &BackupSet) -> Written {
            let snap = self.snapshots.take(&self.store).unwrap();
            set.back_up(&self.snapshots.dir().join(&snap.name), &self.dir.join("blobs"), &mut |_, _| {})
                .unwrap()
        }

        fn name(&self) -> String {
            device_name(self.store.peer())
        }
    }

    #[test]
    fn pruning_keeps_each_devices_newest_backups_and_the_blobs_they_need() {
        let tmp = Tmp::new("prune");
        let set = BackupSet::create(&tmp.0.join("drive")).unwrap();
        let mut a = Device::new(tmp.0.join("a"));
        let mut b = Device::new(tmp.0.join("b"));
        // A blob a document held for an hour, long ago: in a's first backup, then
        // gone once history compaction thins that hour out.
        let day = db::retention::DAY_MS;
        let then = 20500 * day;
        let doc = a.store.create("Note", &serde_json::Map::new(), "t").unwrap().id;
        let (first, n) =
            a.blobs.write(&mut &b"only in a's first backup"[..], db::blobs::MAX_BLOB_BYTES).unwrap();
        a.store.add_blob(&first, n).unwrap();
        let date = |a: &Device, sql: &str, at: i64| a.store.connection().execute(sql, [at]).unwrap();
        date(&a, "UPDATE blobs SET created = ?1", then);
        let set_field = |a: &mut Device, v: serde_json::Value, at: i64| {
            a.store.update(&doc, serde_json::json!({"file": v}).as_object().unwrap(), "t").unwrap();
            date(a, "UPDATE change_sets SET at = ?1 WHERE id = (SELECT max(id) FROM change_sets)", at);
        };
        set_field(&mut a, serde_json::json!({"$blob": first}), then);
        a.back_up(&set);
        set_field(&mut a, serde_json::Value::Null, then + 3_600_000);
        let done =
            db::compact_history(&mut a.store, &a.blobs, then + 100 * day, &db::Retention::default()).unwrap();
        assert_eq!(done.blobs, 1);
        let later = a.add(b"in a's later backups");
        for _ in 0..3 {
            a.store.create("Note", &serde_json::Map::new(), "t").unwrap();
            a.back_up(&set);
        }
        let shared = b.add(b"in b's backup");
        b.back_up(&set);
        // A copy interrupted by unplugging the drive.
        std::fs::write(set.root.join("blobs").join(".incoming-x"), b"half").unwrap();

        assert_eq!(set.list(&a.name()).unwrap().len(), 5);
        assert_eq!(set.prune(&a.name(), 2).unwrap(), 2);
        let left = set.list(&a.name()).unwrap();
        assert_eq!(left.iter().filter(|x| x.this_device).count(), 2);
        assert_eq!(left.iter().filter(|x| !x.this_device).count(), 1, "b's backup isn't a's to prune");
        let drive_blob = |h: &str| blob_path(&set.root.join("blobs"), h).exists();
        assert!(drive_blob(&later) && drive_blob(&shared));
        assert!(!drive_blob(&first), "no kept backup records it");
        assert!(!set.root.join("blobs/.incoming-x").exists());
    }

    #[test]
    fn a_damaged_blob_fails_the_backup_and_leaves_nothing_behind() {
        let tmp = Tmp::new("damaged");
        let set = BackupSet::create(&tmp.0.join("drive")).unwrap();
        let mut a = Device::new(tmp.0.join("a"));
        let h = a.add(b"good bytes");
        let path = blob_path(&a.dir.join("blobs"), &h);
        set_writable(&path);
        std::fs::write(&path, b"rotten bytes").unwrap();
        let snap = a.snapshots.take(&a.store).unwrap();
        let err = set
            .back_up(&a.snapshots.dir().join(&snap.name), &a.dir.join("blobs"), &mut |_, _| {})
            .unwrap_err();
        assert!(matches!(&err, Error::Failed(m) if m.contains("damaged")), "{err}");
        assert!(!blob_path(&set.root.join("blobs"), &h).exists());
        assert!(set.list(&a.name()).unwrap().is_empty(), "no backup without its blobs");
        let stray = std::fs::read_dir(set.root.join("blobs")).unwrap().flatten();
        assert!(stray.filter(|e| e.file_name().to_string_lossy().starts_with('.')).count() == 0);
    }

    fn set_writable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
}
