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

use bus_client::{Client, Incoming};
use std::sync::Arc;
use std::time::Duration;

use db::{Error, Store, Watches};
use protocol::BusError;
use protocol::bus::{Sender, code as bus_code};
use protocol::db::{
    AtParams, AtResult, ChangeKind, ChangeSetInfo, ChangesParams, ChangesResult, CreateParams,
    DEFAULT_QUERY_LIMIT, DocsResult, HistoryResult, IdParams, MAX_WATCHES, Query, SearchParams, UndoParams,
    UndoResult, UpdateParams, VersionInfo, WatchEvent, code, method,
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

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, BusError> {
    serde_json::from_value(params).map_err(|e| error(bus_code::BAD_REQUEST, format!("bad params: {e}")))
}

fn reply<T: Serialize>(result: db::Result<T>) -> Result<Value, BusError> {
    match result {
        Ok(v) => Ok(serde_json::to_value(v).expect("db results serialize")),
        Err(Error::NotFound(id)) => Err(error(code::NOT_FOUND, format!("no document {id}"))),
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
}

/// What the store's thread sends back for a live query.
enum Part {
    /// One part of its streamed answer.
    Frame(Value),
    /// The live query ended with this error.
    End(BusError),
}

/// Serve `store` on `bus`, which must already have claimed the `db` name, until
/// the connection closes.
pub async fn serve(bus: Client, store: Store) {
    let Some(mut requests) = bus.take_requests() else { return };
    let (jobs, queue) = mpsc::channel::<Job>(256);
    let (parts_tx, mut parts) = mpsc::unbounded_channel::<(u64, Part)>();
    let flush = Arc::new(Notify::new());
    let worker = {
        let flush = flush.clone();
        std::thread::spawn(move || work(store, queue, parts_tx, &flush))
    };

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
            Incoming::Request { id, from, method, params, .. } => {
                let (reply, answer) = oneshot::channel();
                let bus = bus.clone();
                tokio::spawn(async move {
                    if let Ok(answer) = answer.await
                        && let Err(e) = bus.respond(id, answer).await
                    {
                        // Most likely too big to send; say so rather than leave the
                        // caller waiting for a timeout.
                        let _ = bus
                            .respond(id, Err(BusError { code: e.code().into(), message: e.to_string() }))
                            .await;
                    }
                });
                Job::Call { origin: origin(from.as_ref()), method, params, reply }
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
}

/// The store's thread: runs jobs in order until the queue closes.
fn work(
    mut store: Store,
    mut queue: mpsc::Receiver<Job>,
    parts: mpsc::UnboundedSender<(u64, Part)>,
    flush: &Notify,
) {
    let mut watches = match Watches::new(&store) {
        Ok(w) => w,
        Err(e) => {
            log::error!("dbd: cannot start live queries: {e}");
            return;
        }
    };
    let frame = |event: WatchEvent| Part::Frame(serde_json::to_value(event).expect("db results serialize"));
    while let Some(job) = queue.blocking_recv() {
        match job {
            Job::Call { origin, method, params, reply } => {
                let _ = reply.send(handle(&mut store, &origin, &method, params));
                if !watches.is_empty() {
                    flush.notify_one();
                }
            }
            Job::Watch { id, params } => {
                let part = if watches.len() >= MAX_WATCHES {
                    Part::End(error(
                        bus_code::LIMIT_EXCEEDED,
                        format!("more than {MAX_WATCHES} live queries"),
                    ))
                } else {
                    match parse::<Query>(params).and_then(|q| reply(watches.add(&store, id, q))) {
                        Ok(first) => Part::Frame(first),
                        Err(e) => Part::End(e),
                    }
                };
                let _ = parts.send((id, part));
            }
            Job::Unwatch { id } => {
                watches.remove(id);
            }
            Job::Flush => match watches.refresh(&store) {
                Ok(changed) => {
                    for (id, result) in changed {
                        let part = match result {
                            Ok(event) => frame(event),
                            Err(e) => Part::End(reply::<()>(Err(e)).expect_err("an error")),
                        };
                        let _ = parts.send((id, part));
                    }
                }
                Err(e) => log::error!("dbd: refreshing live queries: {e}"),
            },
        }
    }
}
