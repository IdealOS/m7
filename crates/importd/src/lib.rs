//! The `import` service: files from a share or a folder, into the database as
//! typed documents (see [`protocol::import`] for the methods).
//!
//! Browsing opens the source (mounting a share), lists one folder and closes it
//! again. An import opens the source, finds every file under the chosen paths
//! ([`source::walk`]), and then, in the background, makes a document of each one
//! it knows how to: a `TextDocument` for text, an `Image` (its bytes stored as a
//! blob) for a picture, a `Track` for music (its tags read by [`audio`], its
//! cover art stored as a blob of its own, shared by tracks with the same picture). Each file's SHA-256 goes in its document's `source`, so a
//! file imported before is found and skipped, and a blob already stored isn't sent
//! again. A file that can't be read or stored fails alone; the import carries on.
//! Every document one import makes goes in one change set (the first one's), so
//! `undo` takes the whole import back in one step.
//! The share stays mounted until the import ends.

pub mod audio;
pub mod kinds;
pub mod source;

use std::io::Read;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bus_client::{CallError, Client, Incoming};
use protocol::BusError;
use protocol::bus::code as bus_code;
use protocol::db::{self, BlobInfo, Doc, DocsResult, HistoryResult, UndoParams};
use protocol::import::{
    BrowseParams, BrowseResult, ImportParams, ImportStatus, Kind, LoginParams, MAX_ERRORS, MAX_TEXT_BYTES,
    Origin, SOURCE_FIELD, Source, code, method,
};
use protocol::secrets;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use kinds::{HEAD_BYTES, image_size, image_type, stem, text_format};
use source::{File, Opened, Share};

/// How long dbd gets to store a large blob.
const BLOB_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// What importd needs to know.
#[derive(Debug, Clone)]
pub struct Config {
    /// Where shares are mounted while in use: `/run/idealos/import`.
    pub mounts: PathBuf,
    /// The folder on this machine that `dir` sources may be in, if any (for
    /// development and tests; none on the device).
    pub dir_root: Option<PathBuf>,
}

struct Service {
    bus: Client,
    config: Config,
    status: Mutex<ImportStatus>,
    /// Numbers mount points, so a browse and an import can mount at once.
    mounts: AtomicU64,
}

fn fail(code: &str, message: impl Into<String>) -> BusError {
    BusError { code: code.into(), message: message.into() }
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// The secret a share's password is kept as.
fn secret_name(host: &str, user: &str) -> String {
    format!("smb:{user}@{host}")
}

impl Service {
    async fn call(self: &Arc<Self>, method: &str, params: Value) -> Result<Value, BusError> {
        match method {
            method::LOGIN => {
                let p: LoginParams = parse(params)?;
                let put = json!({"name": secret_name(&p.host, &p.user), "value": p.password});
                self.ask(secrets::SERVICE, secrets::method::PUT, put).await?;
                Ok(Value::Null)
            }
            method::BROWSE => {
                let p: BrowseParams = parse(params)?;
                let opened = self.open(&p.source).await?;
                let entries = tokio::task::spawn_blocking(move || {
                    let dir = opened.resolve(&p.path).map_err(|e| fail(bus_code::BAD_REQUEST, e))?;
                    source::list(&dir).map_err(|e| fail(code::NOT_FOUND, format!("{}: {e}", p.path)))
                })
                .await
                .map_err(|e| fail(code::UNREACHABLE, e.to_string()))??;
                Ok(to_value(BrowseResult { entries }))
            }
            method::IMPORT => self.start(parse(params)?).await.map(to_value),
            method::UNDO => {
                // Apps may only undo their own change sets; an import's are importd's.
                let p: UndoParams = parse(params)?;
                let v = self.ask(db::SERVICE, db::method::UNDO, json!({"change_set": p.change_set})).await?;
                Ok(v)
            }
            method::STATUS => Ok(to_value(self.status.lock().expect("status").clone())),
            other => Err(fail(bus_code::BAD_REQUEST, format!("import has no method {other:?}"))),
        }
    }

    /// Open `source`: mount a share (with its kept password), or check a folder.
    async fn open(&self, source: &Source) -> Result<Opened, BusError> {
        match source {
            Source::Dir { path } => {
                Opened::dir(path, self.config.dir_root.as_deref()).map_err(|e| fail(code::NOT_FOUND, e))
            }
            Source::Share { host, share, user, port } => {
                let get = json!({"name": secret_name(host, user)});
                let password = match self.ask(secrets::SERVICE, secrets::method::GET, get).await {
                    Ok(v) => v["value"].as_str().unwrap_or_default().to_owned(),
                    Err(e) if e.code == "not_found" => {
                        return Err(fail(
                            code::UNREACHABLE,
                            format!("no password for {user} on {host}: log in first"),
                        ));
                    }
                    Err(e) => return Err(e),
                };
                let n = self.mounts.fetch_add(1, Ordering::Relaxed);
                let at = self.config.mounts.join(format!("{}-{n}", std::process::id()));
                let share = Share {
                    host: host.clone(),
                    share: share.clone(),
                    user: user.clone(),
                    port: port.unwrap_or(445),
                };
                tokio::task::spawn_blocking(move || Opened::share(&share, &password, at))
                    .await
                    .map_err(|e| fail(code::UNREACHABLE, e.to_string()))?
                    .map_err(|e| fail(code::UNREACHABLE, e))
            }
        }
    }

    /// Start importing: find the files now, import them in the background.
    async fn start(self: &Arc<Self>, p: ImportParams) -> Result<ImportStatus, BusError> {
        {
            let mut s = self.status.lock().expect("status");
            if s.running {
                return Err(fail(code::BUSY, "an import is already running"));
            }
            *s = ImportStatus { running: true, source: Some(p.source.name()), ..ImportStatus::default() };
        }
        let found = async {
            let opened = self.open(&p.source).await?;
            let paths = p.paths.clone();
            tokio::task::spawn_blocking(move || source::walk(&opened, &paths).map(|files| (opened, files)))
                .await
                .map_err(|e| fail(code::UNREACHABLE, e.to_string()))?
                .map_err(|e| fail(code::NOT_FOUND, e))
        }
        .await;
        let (opened, files) = match found {
            Ok(f) => f,
            Err(e) => {
                self.status.lock().expect("status").running = false;
                return Err(e);
            }
        };
        let status = {
            let mut s = self.status.lock().expect("status");
            s.total = files.len() as u64;
            s.clone()
        };
        let (this, from) = (self.clone(), p.source.name());
        tokio::spawn(async move {
            log::info!("importd: importing {} files from {from}", files.len());
            let mut set = None;
            for file in files {
                this.update(|s| s.current = Some(file.path.clone()));
                let outcome = this.import_one(&from, &file, &mut set).await;
                if set.is_some() {
                    this.update(|s| s.change_set = set);
                }
                this.update(|s| {
                    s.done += 1;
                    match outcome {
                        Ok(Outcome::Imported) => s.imported += 1,
                        Ok(Outcome::Skipped) => s.skipped += 1,
                        Ok(Outcome::Ignored) => s.ignored += 1,
                        Err(e) => {
                            s.failed += 1;
                            s.errors.push(format!("{}: {e}", file.path));
                            if s.errors.len() > MAX_ERRORS {
                                s.errors.remove(0);
                            }
                        }
                    }
                });
            }
            drop(opened); // unmounts a share
            this.update(|s| {
                s.running = false;
                s.current = None;
                log::info!(
                    "importd: done: {} imported, {} already there, {} failed, {} not imported",
                    s.imported,
                    s.skipped,
                    s.failed,
                    s.ignored
                );
            });
        });
        Ok(status)
    }

    fn update(&self, f: impl FnOnce(&mut ImportStatus)) {
        f(&mut self.status.lock().expect("status"));
    }

    /// Make a document of one file, unless it's there already.
    /// `set` is the import's change set, once its first document has made one.
    async fn import_one(&self, from: &str, file: &File, set: &mut Option<i64>) -> Result<Outcome, String> {
        let doc_type = match file.kind {
            Kind::Text => "TextDocument",
            Kind::Image => "Image",
            Kind::Audio => "Track",
            _ => return Ok(Outcome::Ignored),
        };
        if file.kind == Kind::Text && file.size > MAX_TEXT_BYTES {
            return Err(format!(
                "too long for a document ({} KB, at most {} KB)",
                file.size / 1024,
                MAX_TEXT_BYTES / 1024
            ));
        }
        let real = file.real.clone();
        let (hash, head) = tokio::task::spawn_blocking(move || hash_file(&real))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("reading it: {e}"))?;
        let existing: DocsResult = self
            .db(
                db::method::QUERY,
                json!({"type": doc_type, "where": {format!("{SOURCE_FIELD}.hash"): hash}, "limit": 1}),
            )
            .await?;
        if !existing.docs.is_empty() {
            return Ok(Outcome::Skipped);
        }
        let name = file.path.rsplit('/').next().unwrap_or(&file.path);
        let origin =
            Origin { from: from.into(), path: file.path.clone(), hash: hash.clone(), imported: now_ms() };
        let data = match file.kind {
            Kind::Text => {
                // The whole file was read for its hash; it's small.
                let body = String::from_utf8(head).map_err(|_| "not UTF-8 text".to_owned())?;
                json!({"title": stem(name), "body": body, "format": text_format(name), SOURCE_FIELD: origin})
            }
            Kind::Audio => {
                let real = file.real.clone();
                let tags = match tokio::task::spawn_blocking(move || audio::read(&real)).await {
                    Ok(Ok(tags)) => tags,
                    Ok(Err(e)) => {
                        log::warn!("importd: {}: no tags read ({e}); importing it by its name", file.path);
                        audio::Tags::default()
                    }
                    Err(e) => return Err(e.to_string()),
                };
                self.store_blob(&file.real, &hash).await?;
                // A cover that can't be stored leaves the track without one.
                let artwork = match &tags.artwork {
                    Some(art) => self
                        .store_bytes(&art.bytes)
                        .await
                        .map_err(|e| log::warn!("importd: {}: its cover: {e}", file.path))
                        .ok(),
                    None => None,
                };
                let mut track = audio::track(&tags, stem(name), &hash, artwork.as_deref());
                track.insert(SOURCE_FIELD.into(), json!(origin));
                Value::Object(track)
            }
            _ => {
                self.store_blob(&file.real, &hash).await?;
                let mut image = json!({"blob": {"$blob": hash}, "caption": stem(name), SOURCE_FIELD: origin});
                if let Some(t) = image_type(name) {
                    image["type"] = json!(t);
                }
                if let Some((w, h)) = image_size(&head) {
                    image["width"] = json!(w);
                    image["height"] = json!(h);
                }
                image
            }
        };
        let mut create = json!({"type": doc_type, "data": data});
        if let Some(s) = *set {
            create["change_set"] = json!(s);
        }
        let doc: Doc = self.db(db::method::CREATE, create).await?;
        if set.is_none() {
            let versions: HistoryResult = self.db(db::method::HISTORY, json!({"id": doc.id})).await?;
            *set = versions.versions.first().map(|v| v.change_set);
        }
        Ok(Outcome::Imported)
    }

    /// Store `bytes` (a cover picture) as a blob, unless dbd has it already: its hash.
    async fn store_bytes(&self, bytes: &[u8]) -> Result<String, String> {
        let mut h = ::db::sha256::Sha256::default();
        h.update(bytes);
        let hash = h.hex();
        if self.db::<BlobInfo>(db::method::BLOB_INFO, json!({"hash": hash})).await.is_ok() {
            return Ok(hash);
        }
        // put_blob takes a descriptor: write the bytes to a file nobody else sees,
        // open it, and unlink it before sending.
        let n = self.mounts.fetch_add(1, Ordering::Relaxed);
        std::fs::create_dir_all(&self.config.mounts).map_err(|e| e.to_string())?;
        let path = self.config.mounts.join(format!(".bytes-{}-{n}", std::process::id()));
        std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
        let file = std::fs::File::open(&path);
        let _ = std::fs::remove_file(&path);
        self.put_blob(file.map_err(|e| e.to_string())?.into(), &hash).await?;
        Ok(hash)
    }

    /// Store file `real` as a blob, unless dbd has it already.
    async fn store_blob(&self, real: &std::path::Path, hash: &str) -> Result<(), String> {
        if self.db::<BlobInfo>(db::method::BLOB_INFO, json!({"hash": hash})).await.is_ok() {
            return Ok(());
        }
        let fd: OwnedFd = std::fs::File::open(real).map_err(|e| format!("opening it: {e}"))?.into();
        self.put_blob(fd, hash).await
    }

    /// Send `fd` to dbd as a blob, which should have hash `hash`.
    async fn put_blob(&self, fd: OwnedFd, hash: &str) -> Result<(), String> {
        let reply = self
            .bus
            .request_with(db::SERVICE, db::method::PUT_BLOB, Value::Null, vec![fd], Some(BLOB_TIMEOUT))
            .await
            .map_err(|e| format!("storing it: {}", message(e)))?;
        let info: BlobInfo = serde_json::from_value(reply.result).map_err(|e| e.to_string())?;
        if info.hash != hash {
            return Err("it changed while it was read".into());
        }
        Ok(())
    }

    async fn db<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T, String> {
        let v =
            self.bus.request(db::SERVICE, method, params).await.map_err(|e| format!("db: {}", message(e)))?;
        serde_json::from_value(v).map_err(|e| format!("db answered oddly: {e}"))
    }

    async fn ask(&self, to: &str, method: &str, params: Value) -> Result<Value, BusError> {
        self.bus.request(to, method, params).await.map_err(|e| match e {
            CallError::Bus(e) => e,
            CallError::Disconnected => fail(code::UNREACHABLE, "the bus closed the connection"),
        })
    }
}

fn message(e: CallError) -> String {
    match e {
        CallError::Bus(e) => e.message,
        CallError::Disconnected => "the bus closed the connection".into(),
    }
}

/// What importing one file did.
enum Outcome {
    Imported,
    Skipped,
    Ignored,
}

/// A file's SHA-256 in hex, and its first bytes ([`HEAD_BYTES`], or all of a
/// smaller file).
fn hash_file(path: &std::path::Path) -> std::io::Result<(String, Vec<u8>)> {
    let mut f = std::fs::File::open(path)?;
    let mut hash = ::db::sha256::Sha256::default();
    let mut head = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
        let room = (HEAD_BYTES.max(MAX_TEXT_BYTES as usize)).saturating_sub(head.len());
        head.extend_from_slice(&buf[..n.min(room)]);
    }
    Ok((hash.hex(), head))
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, BusError> {
    serde_json::from_value(params).map_err(|e| fail(bus_code::BAD_REQUEST, format!("bad params: {e}")))
}

fn to_value(v: impl serde::Serialize) -> Value {
    serde_json::to_value(v).expect("results serialize")
}

/// Serve `config` on `bus`, which must already have claimed `import`.
pub async fn serve(bus: Client, config: Config) -> Result<(), String> {
    let Some(mut requests) = bus.take_requests() else { return Ok(()) };
    source::release_stale(&config.mounts);
    let service =
        Arc::new(Service { bus: bus.clone(), config, status: Mutex::default(), mounts: AtomicU64::new(0) });
    while let Some(incoming) = requests.next().await {
        if let Incoming::Request { id, method, params, .. } = incoming {
            let (bus, service) = (bus.clone(), service.clone());
            tokio::spawn(async move {
                let answer = service.call(&method, params).await;
                let _ = bus.respond(id, answer).await;
            });
        }
    }
    Ok(())
}
