//! The `backup` service: backs the database and its blobs up to a USB drive, and
//! restores them (see [`protocol::backup`] for the methods and the drive's layout).
//!
//! backupd asks dbd for a fresh snapshot, then copies it and the blobs it records
//! to the drive ([`set`]); blob files and snapshots are read straight from dbd's
//! directories. Restoring copies the blobs back into dbd's blob directory, puts the
//! backup's database in dbd's snapshot directory, and asks dbd to restore it.
//!
//! It looks for drives every [`POLL`] ([`drives`]). When a backup drive appears it
//! backs up if this device's newest backup there is over [`ON_PLUG_IN`] old, and
//! again whenever one is over [`EVERY`] old while it stays plugged in. One backup or
//! restore runs at a time; `watch` streams the status as it changes, progress
//! included.

pub mod drives;
pub mod set;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bus_client::{Client, Incoming};
use protocol::BusError;
use protocol::backup::{
    BackedUp, BackupStatus, BackupsResult, Busy, Doing, Drive, DriveParams, KEEP, RestoreBackupParams, code,
    method,
};
use protocol::bus::code as bus_code;
use protocol::db::{self, RestoreResult, SnapshotInfo, StatsResult};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::Notify;

use drives::Drives;
use set::BackupSet;

/// How often backupd looks for drives.
pub const POLL: Duration = Duration::from_secs(5);
/// A backup drive just plugged in gets a backup if the newest is older than this.
pub const ON_PLUG_IN: Duration = Duration::from_secs(60 * 60);
/// While it stays plugged in, a backup drive gets one whenever the newest is older.
pub const EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// How long dbd gets to snapshot or restore a large database.
const DB_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// What backupd needs to know.
#[derive(Debug, Clone)]
pub struct Config {
    /// Where dbd keeps its snapshots (`snapshots/`) and blobs (`blobs/`):
    /// `/data/db` on the device.
    pub db_dir: PathBuf,
    pub drives: Drives,
    /// Back up on its own when a backup drive is plugged in, and daily.
    pub automatic: bool,
}

/// The shared state: what `status` answers.
#[derive(Default)]
struct State {
    status: BackupStatus,
    /// This device's name for backups, once dbd has said.
    device: Option<String>,
    /// Drives plugged in at the last look: those that were there already don't
    /// count as just plugged in.
    seen: HashMap<String, u64>,
}

struct Service {
    bus: Client,
    config: Config,
    state: Mutex<State>,
    changed: Notify,
    /// Held while a drive is mounted: a drive can't be mounted twice at once.
    drive_lock: tokio::sync::Mutex<()>,
}

/// Why a request failed, as the bus says it.
fn fail(code: &str, message: impl Into<String>) -> BusError {
    BusError { code: code.into(), message: message.into() }
}

impl From<set::Error> for BusError {
    fn from(e: set::Error) -> BusError {
        match e {
            set::Error::NotFound(m) => fail(code::NOT_FOUND, m),
            set::Error::Failed(m) => fail(code::FAILED, m),
        }
    }
}

impl Service {
    /// Run `f` on a blocking thread while holding the drive lock.
    async fn with_drive<T: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce(&Service) -> Result<T, BusError> + Send + 'static,
    ) -> Result<T, BusError> {
        let _held = self.drive_lock.lock().await;
        let this = self.clone();
        tokio::task::spawn_blocking(move || f(&this)).await.map_err(|e| fail(code::FAILED, e.to_string()))?
    }

    fn status(&self) -> BackupStatus {
        self.state.lock().expect("state lock").status.clone()
    }

    fn update(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.state.lock().expect("state lock"));
        self.changed.notify_one();
    }

    /// This device's name for backups, asking dbd the first time.
    async fn device(&self) -> Result<String, BusError> {
        if let Some(d) = self.state.lock().expect("state lock").device.clone() {
            return Ok(d);
        }
        let stats: StatsResult = call(&self.bus, db::method::STATS, json!({}), None).await?;
        self.update(|s| s.device = Some(stats.device.clone()));
        Ok(stats.device)
    }

    /// Look at the drives plugged in: mount each new one long enough to see whether
    /// it's a backup drive and what's on it. Returns the backup drives that just
    /// appeared.
    async fn look(self: &Arc<Self>) -> Vec<String> {
        let found = self.config.drives.list();
        let device = self.device().await.unwrap_or_default();
        let (known, previously): (HashMap<String, Drive>, HashMap<String, u64>) = {
            let s = self.state.lock().expect("state lock");
            (s.status.drives.iter().map(|d| (d.id.clone(), d.clone())).collect(), s.seen.clone())
        };
        let mut drives = Vec::new();
        let mut appeared = Vec::new();
        for f in &found {
            let same = previously.get(&f.id) == Some(&f.bytes);
            let drive = match known.get(&f.id) {
                Some(d) if same => d.clone(),
                _ => {
                    let (id, dev) = (f.id.clone(), device.clone());
                    match self
                        .with_drive(move |s| s.inspect(&id, &dev).map_err(|e| fail(code::FAILED, e)))
                        .await
                    {
                        Ok(d) => d,
                        Err(e) => {
                            log::warn!("backupd: can't read drive {}: {}", f.id, e.message);
                            continue;
                        }
                    }
                }
            };
            if !same && drive.set_up {
                log::info!("backupd: backup drive {} plugged in", drive.id);
                appeared.push(drive.id.clone());
            }
            drives.push(drive);
        }
        drives.sort_by(|a, b| a.id.cmp(&b.id));
        let seen = found.into_iter().map(|f| (f.id, f.bytes)).collect();
        let changed = drives != self.status().drives;
        if changed {
            self.update(|s| {
                s.status.drives = drives;
                s.seen = seen;
            });
        } else {
            self.state.lock().expect("state lock").seen = seen;
        }
        appeared
    }

    /// Mount drive `id` and see what's on it.
    fn inspect(&self, id: &str, device: &str) -> Result<Drive, String> {
        let bytes = self.config.drives.list().into_iter().find(|d| d.id == id).map_or(0, |d| d.bytes);
        let mount = self.config.drives.mount(id)?;
        let free = drives::space(&mount.path).map_or(0, |(_, free)| free);
        let set = BackupSet::open(&mount.path);
        let last = match &set {
            Some(set) => set.list(device).map_err(|e| e.to_string())?.into_iter().find(|b| b.this_device),
            None => None,
        };
        Ok(Drive { id: id.into(), bytes, free, set_up: set.is_some(), last })
    }

    /// Refresh one drive's entry after writing to it.
    async fn reinspect(self: &Arc<Self>, id: &str) {
        let device = self.device().await.unwrap_or_default();
        let id_owned = id.to_owned();
        let inspected =
            self.with_drive(move |s| s.inspect(&id_owned, &device).map_err(|e| fail(code::FAILED, e)));
        if let Ok(drive) = inspected.await {
            self.update(|s| {
                if let Some(d) = s.status.drives.iter_mut().find(|d| d.id == drive.id) {
                    *d = drive;
                }
            });
        }
    }

    /// Mark the service busy with `doing` on `drive`, or refuse if it already is.
    fn start(&self, drive: &str, doing: Doing) -> Result<(), BusError> {
        let mut s = self.state.lock().expect("state lock");
        if let Some(b) = &s.status.busy {
            let what = match b.doing {
                Doing::BackingUp => "backing up",
                Doing::Restoring => "restoring",
            };
            return Err(fail(code::BUSY, format!("already {what} on {}", b.drive)));
        }
        if !s.status.drives.iter().any(|d| d.id == drive) {
            return Err(fail(code::NOT_FOUND, format!("no drive {drive} is plugged in")));
        }
        s.status.busy = Some(Busy { drive: drive.into(), doing, done: 0, total: 0 });
        drop(s);
        self.changed.notify_one();
        Ok(())
    }

    /// Clear the busy mark, recording how it went.
    fn finish<T>(&self, result: &Result<T, BusError>) {
        self.update(|s| {
            s.status.busy = None;
            s.status.error = result.as_ref().err().map(|e| e.message.clone());
        });
    }

    /// A progress callback that updates the status, at most a few times a second.
    fn progress(self: &Arc<Self>) -> impl FnMut(u64, u64) + Send + 'static {
        let this = self.clone();
        let mut last = std::time::Instant::now();
        move |done, total| {
            if done < total && last.elapsed() < Duration::from_millis(250) {
                return;
            }
            last = std::time::Instant::now();
            this.update(|s| {
                if let Some(b) = &mut s.status.busy {
                    b.done = done;
                    b.total = total;
                }
            });
        }
    }

    async fn setup(self: &Arc<Self>, drive: &str) -> Result<(), BusError> {
        let id = drive.to_owned();
        self.with_drive(move |this| {
            let mount = this.config.drives.mount(&id).map_err(|e| fail(code::NOT_FOUND, e))?;
            BackupSet::create(&mount.path)?;
            Ok(())
        })
        .await?;
        log::info!("backupd: drive {drive} is now a backup drive");
        self.reinspect(drive).await;
        Ok(())
    }

    async fn back_up(self: &Arc<Self>, drive: &str) -> Result<BackedUp, BusError> {
        self.start(drive, Doing::BackingUp)?;
        let result = self.back_up_now(drive).await;
        self.finish(&result);
        self.reinspect(drive).await;
        match &result {
            Ok(b) => log::info!(
                "backupd: backed up to {drive}: {} ({} blobs, {} bytes copied)",
                b.backup.name,
                b.blobs_copied,
                b.bytes_copied
            ),
            Err(e) => log::error!("backupd: backing up to {drive} failed: {}", e.message),
        }
        result
    }

    async fn back_up_now(self: &Arc<Self>, drive: &str) -> Result<BackedUp, BusError> {
        let device = self.device().await?;
        let snap: SnapshotInfo = call(&self.bus, db::method::SNAPSHOT, json!({}), Some(DB_TIMEOUT)).await?;
        let id = drive.to_owned();
        let mut progress = self.progress();
        self.with_drive(move |this| {
            let mount = this.config.drives.mount(&id).map_err(|e| fail(code::NOT_FOUND, e))?;
            let set = BackupSet::open(&mount.path)
                .ok_or_else(|| fail(code::NOT_SET_UP, format!("drive {id} isn't a backup drive yet")))?;
            let snapshot = this.config.db_dir.join("snapshots").join(&snap.name);
            let written = set.back_up(&snapshot, &this.config.db_dir.join("blobs"), &mut progress)?;
            let pruned = set.prune(&device, KEEP)?;
            Ok(BackedUp {
                backup: written.backup,
                blobs_copied: written.blobs_copied,
                bytes_copied: written.bytes_copied,
                pruned,
            })
        })
        .await
    }

    async fn backups(self: &Arc<Self>, drive: &str) -> Result<BackupsResult, BusError> {
        let device = self.device().await?;
        let id = drive.to_owned();
        self.with_drive(move |this| {
            let mount = this.config.drives.mount(&id).map_err(|e| fail(code::NOT_FOUND, e))?;
            let set = BackupSet::open(&mount.path)
                .ok_or_else(|| fail(code::NOT_SET_UP, format!("drive {id} isn't a backup drive")))?;
            Ok(BackupsResult { backups: set.list(&device)? })
        })
        .await
    }

    async fn restore(self: &Arc<Self>, p: RestoreBackupParams) -> Result<RestoreResult, BusError> {
        self.start(&p.drive, Doing::Restoring)?;
        let result = self.restore_now(&p).await;
        self.finish(&result);
        match &result {
            Ok(_) => {
                log::warn!("backupd: restored {} from {}", p.name, p.drive);
                // The device may be another one now.
                self.update(|s| s.device = None);
                let device = self.device().await.unwrap_or_default();
                log::info!("backupd: this device is {device}");
                self.reinspect(&p.drive).await;
            }
            Err(e) => log::error!("backupd: restoring {} failed: {}", p.name, e.message),
        }
        result
    }

    async fn restore_now(self: &Arc<Self>, p: &RestoreBackupParams) -> Result<RestoreResult, BusError> {
        let (id, name) = (p.drive.clone(), p.name.clone());
        let mut progress = self.progress();
        // Blobs first, so the restored database finds every one it refers to.
        let snapshot = self
            .with_drive(move |this| {
                let mount = this.config.drives.mount(&id).map_err(|e| fail(code::NOT_FOUND, e))?;
                let set = BackupSet::open(&mount.path)
                    .ok_or_else(|| fail(code::NOT_SET_UP, format!("drive {id} isn't a backup drive")))?;
                let (backup, _) =
                    set.restore_blobs(&name, &this.config.db_dir.join("blobs"), &mut progress)?;
                Ok(set::place_snapshot(&backup, &this.config.db_dir.join("snapshots"))?)
            })
            .await?;
        call(&self.bus, db::method::RESTORE, json!({"name": snapshot, "adopt": true}), Some(DB_TIMEOUT)).await
    }

    /// Back up to every backup drive that's due.
    async fn automatic(self: &Arc<Self>, appeared: &[String]) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        let due: Vec<String> = self
            .status()
            .drives
            .iter()
            .filter(|d| d.set_up)
            .filter(|d| {
                let age = d.last.as_ref().map_or(i64::MAX, |b| now - b.taken);
                let limit = if appeared.contains(&d.id) { ON_PLUG_IN } else { EVERY };
                age > limit.as_millis() as i64
            })
            .map(|d| d.id.clone())
            .collect();
        for id in due {
            if self.status().busy.is_none() {
                let _ = self.back_up(&id).await;
            }
        }
    }

    async fn call(self: &Arc<Self>, method: &str, params: Value) -> Result<Value, BusError> {
        let drive = || parse::<DriveParams>(params.clone()).map(|p| p.drive);
        match method {
            method::STATUS => Ok(to_value(self.status())),
            method::SETUP => self.setup(&drive()?).await.map(|()| Value::Null),
            method::BACK_UP => self.back_up(&drive()?).await.map(to_value),
            method::BACKUPS => self.backups(&drive()?).await.map(to_value),
            method::RESTORE => self.restore(parse(params)?).await.map(to_value),
            other => Err(fail(bus_code::BAD_REQUEST, format!("backup has no method {other:?}"))),
        }
    }
}

/// Call dbd.
async fn call<T: DeserializeOwned>(
    bus: &Client,
    method: &str,
    params: Value,
    timeout: Option<Duration>,
) -> Result<T, BusError> {
    let answer = match timeout {
        Some(t) => bus.request_timeout(db::SERVICE, method, params, t).await,
        None => bus.request(db::SERVICE, method, params).await,
    };
    let value = answer.map_err(|e| match e {
        bus_client::CallError::Bus(e) => fail(code::FAILED, format!("db: {}", e.message)),
        bus_client::CallError::Disconnected => fail(code::FAILED, "the bus closed the connection"),
    })?;
    serde_json::from_value(value).map_err(|e| fail(code::FAILED, format!("db answered oddly: {e}")))
}

/// Serve `config` on `bus`, which must already have claimed `backup`.
pub async fn serve(bus: Client, config: Config) -> Result<(), String> {
    let Some(mut requests) = bus.take_requests() else { return Ok(()) };
    config.drives.release_stale();
    let service = Arc::new(Service {
        bus: bus.clone(),
        config,
        state: Mutex::new(State::default()),
        changed: Notify::new(),
        drive_lock: tokio::sync::Mutex::new(()),
    });
    let watchers: Arc<tokio::sync::Mutex<Vec<u64>>> = Arc::default();

    // Look for drives, and back up to the ones that are due.
    let looking = {
        let service = service.clone();
        tokio::spawn(async move {
            loop {
                let appeared = service.look().await;
                if service.config.automatic {
                    service.automatic(&appeared).await;
                }
                tokio::time::sleep(POLL).await;
            }
        })
    };
    // Send watchers the status whenever it changes.
    let pushing = {
        let (service, watchers) = (service.clone(), watchers.clone());
        tokio::spawn(async move {
            let mut last = None;
            loop {
                service.changed.notified().await;
                let now = service.status();
                if last.as_ref() != Some(&now) {
                    let v = to_value(&now);
                    for &w in watchers.lock().await.iter() {
                        let _ = service.bus.stream(w, v.clone()).await;
                    }
                    last = Some(now);
                }
            }
        })
    };
    while let Some(incoming) = requests.next().await {
        match incoming {
            Incoming::Request { id, method, stream: true, .. } if method == method::WATCH => {
                let _ = bus.stream(id, to_value(service.status())).await;
                watchers.lock().await.push(id);
            }
            Incoming::Request { id, method, .. } if method == method::WATCH => {
                let e = fail(bus_code::BAD_REQUEST, "watch must be sent as a streamed request");
                let _ = bus.respond(id, Err(e)).await;
            }
            Incoming::Request { id, method, params, .. } => {
                let (bus, service) = (bus.clone(), service.clone());
                tokio::spawn(async move {
                    let answer = service.call(&method, params).await;
                    let _ = bus.respond(id, answer).await;
                });
            }
            Incoming::Cancel { id } => watchers.lock().await.retain(|w| *w != id),
        }
    }
    looking.abort();
    pushing.abort();
    Ok(())
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, BusError> {
    serde_json::from_value(params).map_err(|e| fail(bus_code::BAD_REQUEST, format!("bad params: {e}")))
}

fn to_value(v: impl Serialize) -> Value {
    serde_json::to_value(v).expect("results serialize")
}
