//! The `db` service: [`db::Store`] on the bus.
//!
//! The store is synchronous (SQLite), so it runs on its own thread; the bus side
//! sends it one job per request and answers when the job is done. Requests are
//! handled one at a time, in the order they arrive.
//!
//! Live queries (`watch`) are streamed requests. The store's thread keeps them in
//! [`db::Watches`]; a write asks for a flush, which runs [`BATCH`] later, so every
//! write in between is covered by one re-run and one part per live query. A cancel
//! (which the broker also sends when the caller disconnects) stops it.
//!
//! Each change is recorded with its origin: the requester's app id or service
//! name, as the broker stamped it, so no client can write in another's name.
//!
//! dbd also takes a snapshot every hour (none when nothing changed), keeping the
//! newest [`db::snapshot::KEEP`]. Blobs are read from the descriptor a `put_blob`
//! request carries off the store's thread, so a slow sender can't hold up other
//! requests; only recording the finished blob waits its turn.

use bus_client::{Client, Incoming};
use std::fs::File;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use db::{Blobs, Error, Snapshots, Store, Watches};
use protocol::BusError;
use protocol::bus::{Sender, code as bus_code};
use protocol::db::{
    AtParams, AtResult, BacklinkInfo, BlobInfo, ChangeKind, ChangeSetInfo, ChangesParams, ChangesResult,
    CreateParams, DEFAULT_QUERY_LIMIT, DiffParams, DiffResult, DocsResult, FieldChangeInfo, HashParams,
    HistoryResult, IdParams, LinkInfo, LinksResult, MAX_BLOB_BYTES, MAX_WATCHES, Query, RestoreParams,
    RestoreResult, SchemasResult, SearchParams, SnapshotInfo, SnapshotsResult, UndoParams, UndoResult,
    UpdateParams, VersionInfo, WatchEvent, code, method,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::{Notify, mpsc, oneshot};

/// Who a change is recorded as coming from.
pub fn origin(from: Option<&Sender>) -> String {
    match from {
        Some(Sender::Service(name)) => name.clone(),
        Some(Sender::App { app, .. }) => app.clone(),
        Some(Sender::Conn(n)) => format!("conn {n}"),
        None => "unknown".into(),
    }
}

/// Run one request against the store.
pub fn handle(store: &mut Store, origin: &str, method: &str, params: Value) -> Result<Value, BusError> {
    match method {
        method::CREATE => {
            let p: CreateParams = parse(params)?;
            reply(store.create(&p.doc_type, &p.data, origin))
        }
        method::GET => reply(store.get(&parse::<IdParams>(params)?.id)),
        method::UPDATE => {
            let p: UpdateParams = parse(params)?;
            reply(store.update(&p.id, &p.patch, origin))
        }
        method::DELETE => reply(store.delete(&parse::<IdParams>(params)?.id, origin)),
        method::QUERY => reply(store.query(&parse::<Query>(params)?).map(|docs| DocsResult { docs })),
        method::SEARCH => {
            let p: SearchParams = parse(params)?;
            let docs = store.search(&p.text, p.doc_type.as_deref(), p.limit.unwrap_or(DEFAULT_QUERY_LIMIT));
            reply(docs.map(|docs| DocsResult { docs }))
        }
        method::HISTORY => {
            let versions = store.history(&parse::<IdParams>(params)?.id).map(|versions| HistoryResult {
                versions: versions
                    .into_iter()
                    .map(|v| VersionInfo {
                        change_set: v.change_set,
                        at: v.at,
                        origin: v.origin,
                        kind: kind(v.kind),
                        undoes: v.undoes,
                    })
                    .collect(),
            });
            reply(versions)
        }
        method::AT => {
            let p: AtParams = parse(params)?;
            reply(store.at(&p.id, p.change_set).map(|data| AtResult { data }))
        }
        method::CHANGES => {
            let p: ChangesParams = parse(if params.is_null() { serde_json::json!({}) } else { params })?;
            let sets = store.change_sets(p.limit.unwrap_or(DEFAULT_QUERY_LIMIT)).map(|sets| ChangesResult {
                change_sets: sets
                    .into_iter()
                    .map(|c| ChangeSetInfo {
                        id: c.id,
                        at: c.at,
                        origin: c.origin,
                        undoes: c.undoes,
                        docs: c.docs,
                    })
                    .collect(),
            });
            reply(sets)
        }
        method::UNDO => {
            let p: UndoParams = parse(params)?;
            reply(store.undo(p.change_set, origin).map(|change_set| UndoResult { change_set }))
        }
        method::LINKS => reply(store.links(&parse::<IdParams>(params)?.id).map(|(out, back)| {
            LinksResult {
                outgoing: out
                    .into_iter()
                    .map(|l| LinkInfo { path: l.path, id: l.id, exists: l.exists })
                    .collect(),
                incoming: back
                    .into_iter()
                    .map(|b| BacklinkInfo { id: b.id, doc_type: b.doc_type, path: b.path })
                    .collect(),
            }
        })),
        method::DIFF => {
            let p: DiffParams = parse(params)?;
            reply(store.diff(&p.id, p.from, p.to).map(|changes| {
                DiffResult {
                    changes: changes
                        .into_iter()
                        .map(|c| FieldChangeInfo { path: c.path, before: c.before, after: c.after })
                        .collect(),
                }
            }))
        }
        method::SCHEMAS => reply(Ok(SchemasResult {
            schemas: db::schemas::all().iter().map(|(name, s)| (name.clone(), s.json.clone())).collect(),
        })),
        method::BLOB_INFO => reply(store.blob_info(&parse::<HashParams>(params)?.hash).map(blob_info)),
        other => Err(error(bus_code::BAD_REQUEST, format!("db has no method {other:?}"))),
    }
}

fn kind(k: db::Kind) -> ChangeKind {
    match k {
        db::Kind::Create => ChangeKind::Create,
        db::Kind::Update => ChangeKind::Update,
        db::Kind::Delete => ChangeKind::Delete,
        db::Kind::Restore => ChangeKind::Restore,
    }
}

fn blob_info(b: db::BlobInfo) -> BlobInfo {
    BlobInfo { hash: b.hash, size: b.size, created: b.created, refs: b.refs }
}

fn snapshot_info(s: db::SnapshotInfo) -> SnapshotInfo {
    SnapshotInfo { name: s.name, taken: s.taken, change_set: s.change_set, bytes: s.bytes }
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, BusError> {
    serde_json::from_value(params).map_err(|e| error(bus_code::BAD_REQUEST, format!("bad params: {e}")))
}

fn reply<T: Serialize>(result: db::Result<T>) -> Result<Value, BusError> {
    match result {
        Ok(v) => Ok(serde_json::to_value(v).expect("db results serialize")),
        Err(e @ Error::NotFound(_)) => Err(error(code::NOT_FOUND, e.to_string())),
        Err(Error::Invalid(why)) => Err(error(bus_code::BAD_REQUEST, why)),
        Err(e) => {
            log::error!("dbd: {e}");
            Err(error(code::INTERNAL, e.to_string()))
        }
    }
}

fn error(code: &str, message: impl Into<String>) -> BusError {
    BusError { code: code.into(), message: message.into() }
}

/// How long changes collect before live queries are re-run, so a burst of writes
/// becomes one part per live query rather than one per write.
pub const BATCH: Duration = Duration::from_millis(16);

/// How often dbd takes a snapshot.
pub const SNAPSHOT_EVERY: Duration = Duration::from_secs(60 * 60);

/// Where dbd keeps the files beside its database.
#[derive(Debug, Clone)]
pub struct Config {
    pub snapshots: PathBuf,
    pub blobs: PathBuf,
}

impl Config {
    /// `snapshots/` and `blobs/` next to the database at `db`.
    pub fn beside(db: &std::path::Path) -> Config {
        let dir = db.parent().unwrap_or(std::path::Path::new("."));
        Config { snapshots: dir.join("snapshots"), blobs: dir.join("blobs") }
    }
}

type Reply = oneshot::Sender<Result<Value, BusError>>;

/// Work for the store's thread.
enum Job {
    /// An ordinary request.
    Call { origin: String, method: String, params: Value, reply: oneshot::Sender<Result<Value, BusError>> },
    /// Start a live query for streamed request `id`.
    Watch { id: u64, params: Value },
    /// The request `id` was cancelled (or its caller went away): stop its live
    /// query, if it has one.
    Unwatch { id: u64 },
    /// Re-run the live queries that changes since the last flush could affect.
    Flush,
    /// Take the hourly snapshot and prune old ones.
    Snapshot,
    /// Record a blob whose file was just written.
    AddBlob { hash: String, size: u64, reply: Reply },
    /// Look up a blob and open it, to answer `get_blob`.
    GetBlob { hash: String, reply: oneshot::Sender<Result<(Value, OwnedFd), BusError>> },
}

/// What the store's thread sends back for a live query.
enum Part {
    /// One part of its streamed answer.
    Frame(Value),
    /// The live query ended with this error.
    End(BusError),
}

/// Serve `store` on `bus`, which must already have claimed the `db` name, until
/// the connection closes. Fails only if the snapshot or blob directory can't be
/// made.
pub async fn serve(bus: Client, store: Store, config: Config) -> Result<(), String> {
    let snapshots = Snapshots::new(&config.snapshots).map_err(|e| e.to_string())?;
    let blobs = Arc::new(Blobs::new(&config.blobs).map_err(|e| e.to_string())?);
    let Some(mut requests) = bus.take_requests() else { return Ok(()) };
    let (jobs, queue) = mpsc::channel::<Job>(256);
    let (parts_tx, mut parts) = mpsc::unbounded_channel::<(u64, Part)>();
    let flush = Arc::new(Notify::new());
    let worker = {
        let flush = flush.clone();
        let blobs = blobs.clone();
        std::thread::spawn(move || work(Worker { store, snapshots, blobs }, queue, parts_tx, &flush))
    };

    // The hourly snapshot (and one at startup, if anything changed since the last).
    let weak_jobs = jobs.downgrade();
    tokio::spawn(async move {
        let mut every = tokio::time::interval(SNAPSHOT_EVERY);
        loop {
            every.tick().await;
            let Some(jobs) = weak_jobs.upgrade() else { break };
            if jobs.send(Job::Snapshot).await.is_err() {
                break;
            }
        }
    });

    // Writes ask for a flush; one runs BATCH later, covering every write since.
    // Holds only a weak sender, so it doesn't keep the store's thread running.
    let weak_jobs = jobs.downgrade();
    tokio::spawn(async move {
        loop {
            flush.notified().await;
            tokio::time::sleep(BATCH).await;
            let Some(jobs) = weak_jobs.upgrade() else { break };
            if jobs.send(Job::Flush).await.is_err() {
                break;
            }
        }
    });

    // Live query parts go out in the order the store's thread made them.
    let sender = bus.clone();
    let weak_jobs = jobs.downgrade();
    tokio::spawn(async move {
        while let Some((id, part)) = parts.recv().await {
            let sent = match part {
                Part::Frame(data) => sender.stream(id, data).await,
                Part::End(e) => sender.respond(id, Err(e)).await,
            };
            if let Err(e) = sent {
                // Too big to send: end the live query with the reason.
                let _ =
                    sender.respond(id, Err(BusError { code: e.code().into(), message: e.to_string() })).await;
                if let Some(jobs) = weak_jobs.upgrade() {
                    let _ = jobs.send(Job::Unwatch { id }).await;
                }
            }
        }
    });

    while let Some(incoming) = requests.next().await {
        let job = match incoming {
            Incoming::Request { id, method, params, stream: true, .. } if method == method::WATCH => {
                Job::Watch { id, params }
            }
            Incoming::Request { id, method, .. } if method == method::WATCH => {
                let e = error(bus_code::BAD_REQUEST, "watch must be sent as a streamed request");
                let _ = bus.respond(id, Err(e)).await;
                continue;
            }
            Incoming::Request { id, method, fds, .. } if method == method::PUT_BLOB => {
                tokio::spawn(put_blob(bus.clone(), jobs.clone(), blobs.clone(), id, fds));
                continue;
            }
            Incoming::Request { id, method, params, .. } if method == method::GET_BLOB => {
                let hash = match parse::<HashParams>(params) {
                    Ok(p) => p.hash,
                    Err(e) => {
                        let _ = bus.respond(id, Err(e)).await;
                        continue;
                    }
                };
                let (reply, answer) = oneshot::channel();
                let bus = bus.clone();
                tokio::spawn(async move {
                    match answer.await {
                        Ok(Ok((info, fd))) => respond(&bus, id, Ok(info), vec![fd]).await,
                        Ok(Err(e)) => respond(&bus, id, Err(e), Vec::new()).await,
                        Err(_) => {}
                    }
                });
                Job::GetBlob { hash, reply }
            }
            Incoming::Request { id, from, method, params, .. } => {
                Job::Call { origin: origin(from.as_ref()), method, params, reply: answer_later(&bus, id) }
            }
            // An ordinary request runs to the end, and the broker drops the answer
            // nobody wants; a live query stops.
            Incoming::Cancel { id } => Job::Unwatch { id },
        };
        if jobs.send(job).await.is_err() {
            break;
        }
    }
    drop(jobs);
    let _ = tokio::task::spawn_blocking(move || worker.join()).await;
    Ok(())
}

/// A channel whose answer is sent as request `id`'s response.
fn answer_later(bus: &Client, id: u64) -> Reply {
    let (reply, answer) = oneshot::channel();
    let bus = bus.clone();
    tokio::spawn(async move {
        if let Ok(answer) = answer.await {
            respond(&bus, id, answer, Vec::new()).await;
        }
    });
    reply
}

/// Answer request `id`. If the answer can't be sent (most likely it's too big),
/// say so rather than leave the caller waiting for a timeout.
async fn respond(bus: &Client, id: u64, answer: Result<Value, BusError>, fds: Vec<OwnedFd>) {
    if let Err(e) = bus.respond_with(id, answer, fds).await {
        let _ = bus.respond(id, Err(BusError { code: e.code().into(), message: e.to_string() })).await;
    }
}

/// Store the file passed with `put_blob` request `id`, then record it.
async fn put_blob(bus: Client, jobs: mpsc::Sender<Job>, blobs: Arc<Blobs>, id: u64, fds: Vec<OwnedFd>) {
    let reply = answer_later(&bus, id);
    let mut fds = fds.into_iter();
    let (Some(fd), None) = (fds.next(), fds.next()) else {
        let _ = reply.send(Err(error(bus_code::BAD_REQUEST, "put_blob takes exactly one descriptor")));
        return;
    };
    let written = tokio::task::spawn_blocking(move || {
        let mut file = File::from(fd);
        // A pipe or socket could stall forever, so only regular files (memfds
        // included), whose end is known.
        match file.metadata() {
            Ok(m) if m.is_file() => {}
            Ok(_) => return Err(Error::Invalid("put_blob takes a regular file or memfd".into())),
            Err(e) => return Err(Error::Io(e.to_string())),
        }
        blobs.write(&mut file, MAX_BLOB_BYTES)
    })
    .await;
    match written {
        Ok(Ok((hash, size))) => {
            let _ = jobs.send(Job::AddBlob { hash, size, reply }).await;
        }
        Ok(Err(e)) => {
            let _ = reply.send(reply_err(e));
        }
        Err(e) => {
            let _ = reply.send(Err(error(code::INTERNAL, format!("storing the blob: {e}"))));
        }
    }
}

fn reply_err(e: Error) -> Result<Value, BusError> {
    reply::<()>(Err(e))
}

/// What the store's thread owns.
struct Worker {
    store: Store,
    snapshots: Snapshots,
    blobs: Arc<Blobs>,
}

impl Worker {
    /// The snapshot methods, which need the snapshot directory as well as the store.
    fn snapshot_method(&mut self, method: &str, params: Value) -> Result<Value, BusError> {
        match method {
            method::SNAPSHOT => reply(self.snapshots.take(&self.store).map(snapshot_info)),
            method::SNAPSHOTS => reply(
                self.snapshots
                    .list()
                    .map(|list| SnapshotsResult { snapshots: list.into_iter().map(snapshot_info).collect() }),
            ),
            _ => {
                let p: RestoreParams = parse(params)?;
                let saved = self.snapshots.restore(&mut self.store, &p.name);
                if saved.is_ok() {
                    log::warn!("dbd: restored snapshot {}", p.name);
                }
                reply(saved.map(|s| RestoreResult { saved: snapshot_info(s) }))
            }
        }
    }

    fn get_blob(&self, hash: &str) -> Result<(Value, OwnedFd), BusError> {
        let info = reply(self.store.blob_info(hash).map(blob_info))?;
        let file = self.blobs.open(hash).map_err(|e| reply_err(e).expect_err("an error"))?;
        Ok((info, OwnedFd::from(file)))
    }
}

/// The store's thread: runs jobs in order until the queue closes.
fn work(
    mut w: Worker,
    mut queue: mpsc::Receiver<Job>,
    parts: mpsc::UnboundedSender<(u64, Part)>,
    flush: &Notify,
) {
    let mut watches = match Watches::new(&w.store) {
        Ok(w) => w,
        Err(e) => {
            log::error!("dbd: cannot start live queries: {e}");
            return;
        }
    };
    while let Some(job) = queue.blocking_recv() {
        match job {
            Job::Call { origin, method, params, reply } => {
                let answer = match method.as_str() {
                    method::SNAPSHOT | method::SNAPSHOTS | method::RESTORE => {
                        let restoring = method == method::RESTORE;
                        let answer = w.snapshot_method(&method, params);
                        // The database changed under every live query.
                        if restoring && answer.is_ok() {
                            send_parts(&parts, watches.reset(&w.store));
                        }
                        answer
                    }
                    _ => handle(&mut w.store, &origin, &method, params),
                };
                let _ = reply.send(answer);
                if !watches.is_empty() {
                    flush.notify_one();
                }
            }
            Job::Snapshot => match w.snapshots.take(&w.store) {
                Ok(s) => {
                    log::info!("dbd: snapshot {} ({} bytes)", s.name, s.bytes);
                    if let Err(e) = w.snapshots.prune(db::snapshot::KEEP) {
                        log::error!("dbd: pruning snapshots: {e}");
                    }
                }
                Err(e) => log::error!("dbd: snapshot failed: {e}"),
            },
            Job::AddBlob { hash, size, reply: r } => {
                let _ = r.send(reply(w.store.add_blob(&hash, size).map(blob_info)));
            }
            Job::GetBlob { hash, reply } => {
                let _ = reply.send(w.get_blob(&hash));
            }
            Job::Watch { id, params } => {
                let part = if watches.len() >= MAX_WATCHES {
                    Part::End(error(
                        bus_code::LIMIT_EXCEEDED,
                        format!("more than {MAX_WATCHES} live queries"),
                    ))
                } else {
                    match parse::<Query>(params).and_then(|q| reply(watches.add(&w.store, id, q))) {
                        Ok(first) => Part::Frame(first),
                        Err(e) => Part::End(e),
                    }
                };
                let _ = parts.send((id, part));
            }
            Job::Unwatch { id } => {
                watches.remove(id);
            }
            Job::Flush => send_parts(&parts, watches.refresh(&w.store)),
        }
    }
}

/// Send live query parts (or the errors that ended them) back to the bus side.
fn send_parts(
    parts: &mpsc::UnboundedSender<(u64, Part)>,
    changed: db::Result<Vec<(u64, db::Result<WatchEvent>)>>,
) {
    match changed {
        Ok(changed) => {
            for (id, result) in changed {
                let part = match result {
                    Ok(event) => Part::Frame(serde_json::to_value(event).expect("db results serialize")),
                    Err(e) => Part::End(reply_err(e).expect_err("an error")),
                };
                let _ = parts.send((id, part));
            }
        }
        Err(e) => log::error!("dbd: refreshing live queries: {e}"),
    }
}
