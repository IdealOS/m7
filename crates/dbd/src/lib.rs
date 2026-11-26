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
//! Apps get only what they're allowed ([`db::access`]): what they created, and
//! what their grants cover. dbd follows the grant table permd publishes; when an
//! app's grants change, its live queries are re-run under the new ones at once.
//! A system service acting for an app (the compositor, for a text field bound in
//! the app's window) adds `"as": "<app id>"` to the params, and is then treated,
//! and recorded, as that app. Apps may not use `as`.
//!
//! dbd also takes a snapshot every hour (none when nothing changed), keeping the
//! newest [`db::snapshot::KEEP`]. Blobs are read from the descriptor a `put_blob`
//! request carries off the store's thread, so a slow sender can't hold up other
//! requests; only recording the finished blob waits its turn.

use bus_client::{Client, Incoming};
use std::collections::HashMap;
use std::fs::File;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use db::access::Scope;
use db::{Blobs, Error, Snapshots, Store, Watches};
use protocol::BusError;
use protocol::bus::{Sender, code as bus_code, valid_app_id};
use protocol::db::{
    AtParams, AtResult, BacklinkInfo, BlobInfo, ChangeKind, ChangeSetInfo, ChangesParams, ChangesResult,
    CompactResult, CreateParams, DEFAULT_QUERY_LIMIT, DiffParams, DiffResult, DocsResult, FieldChangeInfo,
    HashParams, HistoryResult, IdParams, LinkInfo, LinksResult, MAX_BLOB_BYTES, MAX_WATCHES, PinParams,
    Query, RestoreParams, RestoreResult, SchemasResult, SearchParams, SnapshotInfo, SnapshotsResult,
    SpliceParams, StatsResult, TypesResult, UndoParams, UndoResult, UpdateParams, VersionInfo, WatchEvent,
    code, method,
};
use protocol::perm::{self, AppGrants, GrantsResult};
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

/// The latest grant table, by app.
pub type Grants = HashMap<String, AppGrants>;

/// Who a request is for, as far as access goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Caller {
    /// A system service or tool, recorded under this origin. Anything goes.
    System(String),
    /// An app: only what it may.
    App(String),
}

/// Who `params` from `from` is for. A system service acting for an app sends
/// `"as": "<app id>"`, which is taken out of the params.
pub fn caller(from: Option<&Sender>, params: &mut Value) -> Result<Caller, BusError> {
    let on_behalf = params.as_object_mut().and_then(|p| p.remove("as"));
    match (from, on_behalf) {
        (Some(Sender::App { app, .. }), None) => Ok(Caller::App(app.clone())),
        (Some(Sender::App { .. }), Some(_)) => {
            Err(error(bus_code::PERMISSION_DENIED, "apps can't act for other apps (`as`)"))
        }
        (_, Some(Value::String(app))) if valid_app_id(&app) => Ok(Caller::App(app)),
        (_, Some(_)) => Err(error(bus_code::BAD_REQUEST, "`as` takes an app id")),
        (from, None) => Ok(Caller::System(origin(from))),
    }
}

/// Run one request for `caller` against the store.
pub fn handle(
    store: &mut Store,
    caller: &Caller,
    grants: &Grants,
    method: &str,
    params: Value,
) -> Result<Value, BusError> {
    match caller {
        Caller::System(origin) => handle_system(store, origin, method, params),
        Caller::App(app) => handle_app(store, &scope(app, grants), method, params),
    }
}

fn scope(app: &str, grants: &Grants) -> Scope {
    Scope::new(app, grants.get(app).cloned().unwrap_or_default())
}

fn denied(why: impl Into<String>) -> BusError {
    error(bus_code::PERMISSION_DENIED, why)
}

/// Refuse unless the app may read (or, with `write`, change) live document `id`.
fn check(store: &Store, scope: &Scope, id: &str, write: bool) -> Result<(), BusError> {
    let doc_type = store.get(id).map_err(err)?.doc_type;
    let filter = if write { scope.write(&doc_type) } else { scope.read(&doc_type) };
    if store.matches(id, &filter).map_err(err)? {
        Ok(())
    } else {
        let verb = if write { "change" } else { "read" };
        Err(denied(format!("{} may not {verb} {doc_type} {id}", scope.app)))
    }
}

/// Refuse unless some document the app may read refers to blob `hash`.
fn check_blob(store: &Store, scope: &Scope, hash: &str) -> Result<(), BusError> {
    for id in store.blob_docs(hash).map_err(err)? {
        if check(store, scope, &id, false).is_ok() {
            return Ok(());
        }
    }
    store.blob_info(hash).map_err(err)?; // a missing blob is not_found
    Err(denied(format!("{} may not read blob {hash}", scope.app)))
}

/// A request from an app, limited to its scope.
fn handle_app(store: &mut Store, scope: &Scope, method: &str, params: Value) -> Result<Value, BusError> {
    let app = scope.app.as_str();
    match method {
        method::CREATE => {
            let p: CreateParams = parse(params)?;
            let Some(filter) = scope.create(&p.doc_type) else {
                return Err(denied(format!("{app} may not create {} documents", p.doc_type)));
            };
            reply(store.guarded(filter, |s| s.create(&p.doc_type, &p.data, app)))
        }
        method::UPDATE => {
            let p: UpdateParams = parse(params)?;
            check(store, scope, &p.id, true)?;
            let filter = scope.write(&store.get(&p.id).map_err(err)?.doc_type);
            reply(store.guarded(filter, |s| s.update(&p.id, &p.patch, app)))
        }
        method::SPLICE => {
            let p: SpliceParams = parse(params)?;
            check(store, scope, &p.id, true)?;
            let filter = scope.write(&store.get(&p.id).map_err(err)?.doc_type);
            let set = store.guarded(filter, |s| {
                s.splice(&p.id, &p.path, p.at as usize, p.delete as usize, &p.insert, app)
            });
            reply(set.map(|change_set| UndoResult { change_set }))
        }
        method::DELETE => {
            let id = parse::<IdParams>(params)?.id;
            check(store, scope, &id, true)?;
            reply(store.delete(&id, app))
        }
        method::GET | method::HISTORY | method::AT | method::DIFF => {
            let id = params.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
            check(store, scope, &id, false)?;
            handle_system(store, app, method, params)
        }
        method::QUERY => {
            let q = scope.query(&parse::<Query>(params)?);
            reply(store.query(&q).map(|docs| DocsResult { docs }))
        }
        method::SEARCH => {
            // Found, then filtered, so an app may get fewer than `limit`.
            let p: SearchParams = parse(params)?;
            let found = store
                .search(&p.text, p.doc_type.as_deref(), p.limit.unwrap_or(DEFAULT_QUERY_LIMIT))
                .map_err(err)?;
            let docs = found.into_iter().filter(|d| check(store, scope, &d.id, false).is_ok()).collect();
            reply(Ok(DocsResult { docs }))
        }
        method::LINKS => {
            let id = parse::<IdParams>(params.clone())?.id;
            check(store, scope, &id, false)?;
            let mut links: LinksResult = from_value(handle_system(store, app, method, params)?);
            links.incoming.retain(|b| check(store, scope, &b.id, false).is_ok());
            Ok(to_value(links))
        }
        method::CHANGES => {
            // Only the app's own.
            let mut sets: ChangesResult = from_value(handle_system(store, app, method, params)?);
            sets.change_sets.retain(|c| c.origin == app);
            Ok(to_value(sets))
        }
        method::UNDO => {
            let p: UndoParams = parse(params.clone())?;
            let set = store.change_set(p.change_set).map_err(err)?;
            if set.origin != app {
                return Err(denied(format!("{app} may undo only its own changes")));
            }
            for id in &set.docs {
                let own = store.creator(id).map_err(err)?.as_deref() == Some(app);
                if !own && check(store, scope, id, true).is_err() {
                    return Err(denied(format!("{app} may no longer change {id}")));
                }
            }
            handle_system(store, app, method, params)
        }
        method::BLOB_INFO => {
            let hash = parse::<HashParams>(params.clone())?.hash;
            check_blob(store, scope, &hash)?;
            handle_system(store, app, method, params)
        }
        method::SCHEMAS => handle_system(store, app, method, params),
        method::TYPES => {
            // Only types it can read some of, counting only those it can.
            let mut types = std::collections::BTreeMap::new();
            for t in store.types().map_err(err)?.into_keys() {
                let n = store.count(&scope.query(&Query::all(t.clone()))).map_err(err)?;
                if n > 0 {
                    types.insert(t, n);
                }
            }
            Ok(to_value(TypesResult { types }))
        }
        method::SNAPSHOT | method::SNAPSHOTS | method::STATS => {
            Err(denied(format!("{app} needs the storage capability for {method}")))
        }
        method::RESTORE => Err(denied("only system services may restore a snapshot")),
        other => Err(error(bus_code::BAD_REQUEST, format!("db has no method {other:?}"))),
    }
}

fn to_value(v: impl Serialize) -> Value {
    serde_json::to_value(v).expect("db results serialize")
}

fn from_value<T: DeserializeOwned>(v: Value) -> T {
    serde_json::from_value(v).expect("db results parse")
}

/// A store error as the bus error it's answered with.
fn err(e: Error) -> BusError {
    reply_err(e).expect_err("an error")
}

/// Run one request against the store, unrestricted, recorded as `origin`.
fn handle_system(store: &mut Store, origin: &str, method: &str, params: Value) -> Result<Value, BusError> {
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
                        pinned: v.pinned,
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
        method::TYPES => reply(store.types().map(|types| TypesResult { types })),
        method::SCHEMAS => reply(Ok(SchemasResult {
            schemas: db::schemas::all().iter().map(|(name, s)| (name.clone(), s.json.clone())).collect(),
        })),
        method::BLOB_INFO => reply(store.blob_info(&parse::<HashParams>(params)?.hash).map(blob_info)),
        method::SPLICE => {
            let p: SpliceParams = parse(params)?;
            let set = store.splice(&p.id, &p.path, p.at as usize, p.delete as usize, &p.insert, origin);
            reply(set.map(|change_set| UndoResult { change_set }))
        }
        method::PIN => {
            let p: PinParams = parse(params)?;
            reply(store.pin(&p.id, p.change_set, p.pinned))
        }
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
        Err(Error::Denied(why)) => Err(denied(why)),
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

/// How often dbd compacts history, and how long after starting it first does, so
/// it isn't part of booting.
pub const COMPACT_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
pub const COMPACT_FIRST: Duration = Duration::from_secs(10 * 60);

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
    Call { caller: Caller, method: String, params: Value, reply: oneshot::Sender<Result<Value, BusError>> },
    /// Start a live query for streamed request `id`.
    Watch { id: u64, caller: Caller, params: Value },
    /// permd published a new grant table.
    Grants(Grants),
    /// The request `id` was cancelled (or its caller went away): stop its live
    /// query, if it has one.
    Unwatch { id: u64 },
    /// Re-run the live queries that changes since the last flush could affect.
    Flush,
    /// Refresh query statistics, take the hourly snapshot and prune old ones.
    Snapshot,
    /// The daily history compaction.
    Compact,
    /// Record a blob whose file was just written.
    AddBlob { hash: String, size: u64, reply: Reply },
    /// Look up a blob and open it, to answer `get_blob`.
    GetBlob { hash: String, caller: Caller, reply: oneshot::Sender<Result<(Value, OwnedFd), BusError>> },
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

    // The daily compaction, first a while after starting.
    let weak_jobs = jobs.downgrade();
    tokio::spawn(async move {
        let mut every = tokio::time::interval_at(tokio::time::Instant::now() + COMPACT_FIRST, COMPACT_EVERY);
        loop {
            every.tick().await;
            let Some(jobs) = weak_jobs.upgrade() else { break };
            if jobs.send(Job::Compact).await.is_err() {
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

    // It holds a connection handle, so it must end with serve (dropped or not),
    // or the connection (and the `db` name) would outlive it.
    let _grants = AbortOnDrop(tokio::spawn(follow_grants(bus.clone(), jobs.downgrade())));

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
        // Who it's for, with any `as` taken out of the params.
        let incoming = match incoming {
            Incoming::Request { id, from, method, mut params, stream, fds } => {
                match caller(from.as_ref(), &mut params) {
                    Ok(who) => (Incoming::Request { id, from, method, params, stream, fds }, who),
                    Err(e) => {
                        let _ = bus.respond(id, Err(e)).await;
                        continue;
                    }
                }
            }
            cancel => (cancel, Caller::System(String::new())),
        };
        let job = match incoming {
            (Incoming::Request { id, method, params, stream: true, .. }, caller)
                if method == method::WATCH =>
            {
                Job::Watch { id, caller, params }
            }
            (Incoming::Request { id, method, .. }, _) if method == method::WATCH => {
                let e = error(bus_code::BAD_REQUEST, "watch must be sent as a streamed request");
                let _ = bus.respond(id, Err(e)).await;
                continue;
            }
            (Incoming::Request { id, method, fds, .. }, _) if method == method::PUT_BLOB => {
                tokio::spawn(put_blob(bus.clone(), jobs.clone(), blobs.clone(), id, fds));
                continue;
            }
            (Incoming::Request { id, method, params, .. }, caller) if method == method::GET_BLOB => {
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
                Job::GetBlob { hash, caller, reply }
            }
            (Incoming::Request { id, method, params, .. }, caller) => {
                Job::Call { caller, method, params, reply: answer_later(&bus, id) }
            }
            // An ordinary request runs to the end, and the broker drops the answer
            // nobody wants; a live query stops.
            (Incoming::Cancel { id }, _) => Job::Unwatch { id },
        };
        if jobs.send(job).await.is_err() {
            break;
        }
    }
    drop(jobs);
    let _ = tokio::task::spawn_blocking(move || worker.join()).await;
    Ok(())
}

/// Whether `caller` may see storage stats and list and take snapshots.
fn may_store(caller: &Caller, grants: &Grants) -> bool {
    match caller {
        Caller::System(_) => true,
        Caller::App(app) => scope(app, grants).has(perm::capability::STORAGE),
    }
}

/// Aborts a task when dropped.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Pass permd's grant tables to the store's thread: the current one, then each
/// published change. Older tables than one already passed are skipped.
async fn follow_grants(bus: Client, jobs: mpsc::WeakSender<Job>) {
    let mut sub = match bus.subscribe(perm::GRANTS_TOPIC).await {
        Ok(s) => s,
        Err(e) => {
            log::error!("dbd: cannot follow grants ({e}); apps get only their own documents");
            return;
        }
    };
    let mut newest = -1;
    let mut pass = async |table: GrantsResult| {
        if table.version < newest {
            return true;
        }
        newest = table.version;
        let Some(jobs) = jobs.upgrade() else { return false };
        jobs.send(Job::Grants(perm::by_app(&table.grants))).await.is_ok()
    };
    // permd may have published before the subscription started.
    if let Ok(v) = bus.request(perm::SERVICE, perm::method::GRANTS, serde_json::json!({})).await
        && let Ok(table) = serde_json::from_value(v)
        && !pass(table).await
    {
        return;
    }
    while let Some(p) = sub.next().await {
        if p.from != Some(Sender::Service(perm::SERVICE.into())) {
            log::warn!("dbd: ignored grants published by {:?}", p.from);
            continue;
        }
        match serde_json::from_value::<GrantsResult>(p.data) {
            Ok(table) => {
                if !pass(table).await {
                    return;
                }
            }
            Err(e) => log::warn!("dbd: ignored a grant table it couldn't read: {e}"),
        }
    }
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
            method::STATS => {
                let (docs, types, change_sets, blobs, blob_bytes) = self.store.counts().map_err(err)?;
                let db_bytes = self.store.file_bytes().map_err(err)?;
                let snapshot_bytes = self.snapshots.list().map_err(err)?.iter().map(|s| s.bytes).sum();
                Ok(to_value(StatsResult {
                    db_bytes,
                    docs,
                    types,
                    change_sets,
                    blobs,
                    blob_bytes,
                    snapshot_bytes,
                    device: format!("{:016x}", self.store.peer()),
                }))
            }
            method::SNAPSHOT => reply(self.snapshots.take(&self.store).map(snapshot_info)),
            method::SNAPSHOTS => reply(
                self.snapshots
                    .list()
                    .map(|list| SnapshotsResult { snapshots: list.into_iter().map(snapshot_info).collect() }),
            ),
            _ => {
                let p: RestoreParams = parse(params)?;
                let saved = self.snapshots.restore_as(&mut self.store, &p.name, p.adopt);
                if saved.is_ok() {
                    log::warn!("dbd: restored snapshot {}", p.name);
                }
                reply(saved.map(|s| RestoreResult { saved: snapshot_info(s) }))
            }
        }
    }

    /// Thin history out and delete the blobs nothing needs, logging what went.
    fn compact(&mut self) -> db::Result<db::Compacted> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let done = db::compact_history(&mut self.store, &self.blobs, now, &db::Retention::default())?;
        if done != db::Compacted::default() {
            log::info!(
                "dbd: compacted history: {} versions, {} change sets, {} blobs ({} bytes)",
                done.versions,
                done.change_sets,
                done.blobs,
                done.blob_bytes
            );
        }
        Ok(done)
    }

    fn get_blob(&self, hash: &str, caller: &Caller, grants: &Grants) -> Result<(Value, OwnedFd), BusError> {
        if let Caller::App(app) = caller {
            check_blob(&self.store, &scope(app, grants), hash)?;
        }
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
    let mut grants = Grants::new();
    // The app behind each of its live queries, and the query it asked for.
    let mut app_watches: HashMap<u64, (String, Query)> = HashMap::new();
    let mut watches = match Watches::new(&w.store) {
        Ok(w) => w,
        Err(e) => {
            log::error!("dbd: cannot start live queries: {e}");
            return;
        }
    };
    while let Some(job) = queue.blocking_recv() {
        match job {
            Job::Call { caller, method, params, reply } => {
                let answer = match method.as_str() {
                    method::SNAPSHOT | method::SNAPSHOTS | method::STATS if may_store(&caller, &grants) => {
                        w.snapshot_method(&method, params)
                    }
                    method::COMPACT if matches!(caller, Caller::System(_)) => {
                        crate::reply(w.compact().map(|c| CompactResult {
                            versions: c.versions,
                            change_sets: c.change_sets,
                            blobs: c.blobs,
                            blob_bytes: c.blob_bytes,
                        }))
                    }
                    method::RESTORE if matches!(caller, Caller::System(_)) => {
                        let restoring = method == method::RESTORE;
                        let answer = w.snapshot_method(&method, params);
                        // The database changed under every live query.
                        if restoring && answer.is_ok() {
                            send_parts(&parts, watches.reset(&w.store));
                        }
                        answer
                    }
                    _ => handle(&mut w.store, &caller, &grants, &method, params),
                };
                let _ = reply.send(answer);
                if !watches.is_empty() {
                    flush.notify_one();
                }
            }
            Job::Snapshot => match w.store.optimize().and_then(|()| w.snapshots.take(&w.store)) {
                Ok(s) => {
                    log::info!("dbd: snapshot {} ({} bytes)", s.name, s.bytes);
                    if let Err(e) = w.snapshots.prune(db::snapshot::KEEP) {
                        log::error!("dbd: pruning snapshots: {e}");
                    }
                }
                Err(e) => log::error!("dbd: snapshot failed: {e}"),
            },
            Job::Compact => {
                if let Err(e) = w.compact() {
                    log::error!("dbd: compacting history: {e}");
                }
            }
            Job::AddBlob { hash, size, reply: r } => {
                let _ = r.send(reply(w.store.add_blob(&hash, size).map(blob_info)));
            }
            Job::GetBlob { hash, caller, reply } => {
                let _ = reply.send(w.get_blob(&hash, &caller, &grants));
            }
            Job::Grants(table) => {
                // Re-run the live queries of every app whose grants changed.
                let changed: Vec<(u64, Query)> = app_watches
                    .iter()
                    .filter(|(_, (app, _))| grants.get(app) != table.get(app))
                    .map(|(&id, (app, q))| (id, scope(app, &table).query(q)))
                    .collect();
                grants = table;
                if !changed.is_empty() {
                    send_parts(&parts, Ok(watches.requery(&w.store, changed)));
                }
                app_watches.retain(|id, _| watches.contains(*id));
            }
            Job::Watch { id, caller, params } => {
                let part = if watches.len() >= MAX_WATCHES {
                    Part::End(error(
                        bus_code::LIMIT_EXCEEDED,
                        format!("more than {MAX_WATCHES} live queries"),
                    ))
                } else {
                    let added = parse::<Query>(params).and_then(|q| match &caller {
                        Caller::System(_) => reply(watches.add(&w.store, id, q)),
                        Caller::App(app) => {
                            let first = reply(watches.add(&w.store, id, scope(app, &grants).query(&q)));
                            if first.is_ok() {
                                app_watches.insert(id, (app.clone(), q));
                            }
                            first
                        }
                    });
                    match added {
                        Ok(first) => Part::Frame(first),
                        Err(e) => Part::End(e),
                    }
                };
                let _ = parts.send((id, part));
            }
            Job::Unwatch { id } => {
                watches.remove(id);
                app_watches.remove(&id);
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
