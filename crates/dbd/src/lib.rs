//! The `db` service: [`db::Store`] on the bus.
//!
//! The store is synchronous (SQLite), so it runs on its own thread; the bus side
//! sends it one job per request and answers when the job is done. Requests are
//! handled one at a time, in the order they arrive.
//!
//! Each change is recorded with its origin: the requester's app id or service
//! name, as the broker stamped it, so no client can write in another's name.

use bus_client::{Client, Incoming};
use db::{Error, Store};
use protocol::BusError;
use protocol::bus::{Sender, code as bus_code};
use protocol::db::{
    AtParams, AtResult, ChangeKind, ChangeSetInfo, ChangesParams, ChangesResult, CreateParams,
    DEFAULT_QUERY_LIMIT, DocsResult, HistoryResult, IdParams, Query, SearchParams, UndoParams, UndoResult,
    UpdateParams, VersionInfo, code, method,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

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

struct Job {
    origin: String,
    method: String,
    params: Value,
    reply: oneshot::Sender<Result<Value, BusError>>,
}

/// Serve `store` on `bus`, which must already have claimed the `db` name, until
/// the connection closes.
pub async fn serve(bus: Client, mut store: Store) {
    let Some(mut requests) = bus.take_requests() else { return };
    let (jobs, mut queue) = mpsc::channel::<Job>(256);
    let worker = std::thread::spawn(move || {
        while let Some(job) = queue.blocking_recv() {
            let _ = job.reply.send(handle(&mut store, &job.origin, &job.method, job.params));
        }
    });
    while let Some(incoming) = requests.next().await {
        // Cancels need nothing: a request runs to the end, and the broker drops the
        // answer nobody wants.
        let Incoming::Request { id, from, method, params, .. } = incoming else { continue };
        let (tx, rx) = oneshot::channel();
        let job = Job { origin: origin(from.as_ref()), method, params, reply: tx };
        if jobs.send(job).await.is_err() {
            break;
        }
        let bus = bus.clone();
        tokio::spawn(async move {
            if let Ok(answer) = rx.await {
                let _ = bus.respond(id, answer).await;
            }
        });
    }
    drop(jobs);
    let _ = tokio::task::spawn_blocking(move || worker.join()).await;
}
