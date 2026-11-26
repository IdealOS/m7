//! backupd with dbd on a real broker, a directory standing in for the USB drive:
//! back up, back up again (nothing new is copied), then restore onto a new device
//! whose database is empty, and find every document and blob there.

use std::path::{Path, PathBuf};

use backupd::drives::Drives;
use bus_client::{CallError, Client};
use protocol::backup::{BackedUp, BackupStatus, BackupsResult, code};
use protocol::db::Doc;
use serde_json::{Value, json};
use tokio::net::UnixListener;

/// One device: a broker, dbd and backupd, with the database in `dir/db`.
struct Device {
    socket: PathBuf,
    db_dir: PathBuf,
}

impl Device {
    /// Start a device in `dir`, backing up to `drive`. `before` runs on the store
    /// before dbd opens it (to put blobs in, which the bus would need fds for).
    async fn start(dir: &Path, drive: &Path, before: impl FnOnce(&mut db::Store, &db::Blobs)) -> Device {
        std::fs::create_dir_all(dir).unwrap();
        let socket = dir.join("bus.sock");
        let db_dir = dir.join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let path = db_dir.join("idealos.db");
        {
            let mut store = db::Store::open(&path).unwrap();
            before(&mut store, &db::Blobs::new(db_dir.join("blobs")).unwrap());
        }
        tokio::spawn(busd::serve(UnixListener::bind(&socket).unwrap(), Box::new(busd::AllowAll), None));
        let dbd_bus = Client::connect(&socket).await.unwrap();
        dbd_bus.claim(protocol::db::SERVICE).await.unwrap();
        tokio::spawn(dbd::serve(dbd_bus, db::Store::open(&path).unwrap(), dbd::Config::beside(&path)));
        let backup_bus = Client::connect(&socket).await.unwrap();
        backup_bus.claim(protocol::backup::SERVICE).await.unwrap();
        let config = backupd::Config {
            db_dir: db_dir.clone(),
            drives: Drives::Dir(drive.to_owned()),
            automatic: false,
        };
        tokio::spawn(backupd::serve(backup_bus, config));
        Device { socket, db_dir }
    }

    async fn client(&self) -> Client {
        Client::connect(&self.socket).await.unwrap()
    }
}

async fn call(c: &Client, method: &str, params: Value) -> Result<Value, CallError> {
    c.request(protocol::backup::SERVICE, method, params).await
}

async fn db(c: &Client, method: &str, params: Value) -> Value {
    c.request(protocol::db::SERVICE, method, params).await.unwrap()
}

/// Wait until backupd has looked for drives and found the stand-in.
async fn found_drive(c: &Client) -> BackupStatus {
    for _ in 0..100 {
        let s: BackupStatus = serde_json::from_value(call(c, "status", Value::Null).await.unwrap()).unwrap();
        if !s.drives.is_empty() {
            return s;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("backupd never found the drive");
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn back_up_then_restore_onto_a_new_device() {
    let tmp = TempDir(std::env::temp_dir().join(format!("backupd-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&tmp.0);
    let drive = tmp.0.join("drive");
    std::fs::create_dir_all(&drive).unwrap();
    let photo = b"pretend this is a photo".repeat(1000);
    let mut hash = String::new();
    let old = Device::start(&tmp.0.join("old"), &drive, |store, blobs| {
        let (h, n) = blobs.write(&mut &photo[..], db::blobs::MAX_BLOB_BYTES).unwrap();
        store.add_blob(&h, n).unwrap();
        let data = json!({"title": "Holiday", "photo": {"$blob": h}});
        store.create("Note", data.as_object().unwrap(), "test").unwrap();
        hash = h;
    })
    .await;
    let c = old.client().await;
    let status = found_drive(&c).await;
    assert_eq!(status.drives[0].id, "dir");
    assert!(!status.drives[0].set_up);

    // Not a backup drive until set up.
    let err = call(&c, "back_up", json!({"drive": "dir"})).await.unwrap_err();
    assert_eq!(err.code(), code::NOT_SET_UP);
    call(&c, "setup", json!({"drive": "dir"})).await.unwrap();
    assert!(drive.join("idealos-backup/README.txt").exists());

    let first: BackedUp =
        serde_json::from_value(call(&c, "back_up", json!({"drive": "dir"})).await.unwrap()).unwrap();
    assert_eq!(first.blobs_copied, 1);
    assert!(first.bytes_copied > photo.len() as u64);
    assert!(drive.join("idealos-backup/blobs").join(&hash[..2]).join(&hash[2..]).exists());
    // Nothing changed: the same backup, nothing copied.
    let again: BackedUp =
        serde_json::from_value(call(&c, "back_up", json!({"drive": "dir"})).await.unwrap()).unwrap();
    assert_eq!((again.backup.name.as_str(), again.bytes_copied), (first.backup.name.as_str(), 0));
    // A change: a new backup, but the photo isn't copied again.
    db(&c, "create", json!({"type": "Note", "data": {"title": "Later"}})).await;
    let second: BackedUp =
        serde_json::from_value(call(&c, "back_up", json!({"drive": "dir"})).await.unwrap()).unwrap();
    assert_ne!(second.backup.name, first.backup.name);
    assert_eq!(second.blobs_copied, 0);
    let status: BackupStatus =
        serde_json::from_value(call(&c, "status", Value::Null).await.unwrap()).unwrap();
    assert_eq!(status.drives[0].last.as_ref().unwrap().name, second.backup.name);
    assert_eq!(status.error, None);

    // The SD card dies. A new device, with an empty database and no blobs.
    let new = Device::start(&tmp.0.join("new"), &drive, |_, _| {}).await;
    let n = new.client().await;
    found_drive(&n).await;
    let list: BackupsResult =
        serde_json::from_value(call(&n, "backups", json!({"drive": "dir"})).await.unwrap()).unwrap();
    assert_eq!(list.backups.len(), 2);
    assert_eq!(list.backups[0].name, second.backup.name, "newest first");
    assert!(!list.backups[0].this_device, "the backups are of the old device");
    let bad = call(&n, "restore", json!({"drive": "dir", "name": "../../etc/passwd"})).await.unwrap_err();
    assert_eq!(bad.code(), code::NOT_FOUND);
    call(&n, "restore", json!({"drive": "dir", "name": second.backup.name})).await.unwrap();

    let docs = db(&n, "query", json!({"type": "Note", "order_by": [["title", "asc"]]})).await;
    let docs: Vec<Doc> = serde_json::from_value(docs["docs"].clone()).unwrap();
    let titles: Vec<&Value> = docs.iter().map(|d| &d.data["title"]).collect();
    assert_eq!(titles, [&json!("Holiday"), &json!("Later")]);
    let blob = new.db_dir.join("blobs").join(&hash[..2]).join(&hash[2..]);
    assert_eq!(std::fs::read(&blob).unwrap(), photo);
    assert_eq!(db(&n, "blob_info", json!({"hash": hash})).await["refs"], 1);
    // The new device is now the old one: its backups are this device's.
    let list: BackupsResult =
        serde_json::from_value(call(&n, "backups", json!({"drive": "dir"})).await.unwrap()).unwrap();
    assert!(list.backups.iter().all(|b| b.this_device));
}
