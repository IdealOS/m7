//! dbd on a real broker: clients create, read, change, query, search and delete
//! documents over the bus, and each change is recorded under the caller's identity.

use std::path::PathBuf;

use bus_client::{CallError, Client};
use protocol::db::{Doc, DocsResult, SERVICE, code};
use serde_json::{Value, json};
use tokio::net::UnixListener;

struct Setup {
    dir: PathBuf,
    socket: PathBuf,
    db: PathBuf,
}

impl Setup {
    async fn start(name: &str) -> Setup {
        let dir = std::env::temp_dir().join(format!("dbd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("bus.sock");
        let db = dir.join("test.db");
        tokio::spawn(busd::serve(UnixListener::bind(&socket).unwrap(), Box::new(busd::AllowAll), None));
        let service = Client::connect(&socket).await.unwrap();
        service.claim(SERVICE).await.unwrap();
        tokio::spawn(dbd::serve(service, db::Store::open(&db).unwrap()));
        Setup { dir, socket, db }
    }

    async fn client(&self) -> Client {
        Client::connect(&self.socket).await.unwrap()
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn call(c: &Client, method: &str, params: Value) -> Result<Value, CallError> {
    c.request(SERVICE, method, params).await
}

fn doc(v: Value) -> Doc {
    serde_json::from_value(v).unwrap()
}

#[tokio::test]
async fn documents_round_trip_over_the_bus() {
    let s = Setup::start("crud").await;
    let c = s.client().await;
    let created =
        doc(call(&c, "create", json!({"type": "Note", "data": {"title": "hi", "n": 1}})).await.unwrap());
    assert_eq!(created.doc_type, "Note");
    assert_eq!(doc(call(&c, "get", json!({"id": created.id})).await.unwrap()), created);

    let updated = doc(call(&c, "update", json!({"id": created.id, "patch": {"n": 2}})).await.unwrap());
    assert_eq!(Value::Object(updated.data), json!({"title": "hi", "n": 2}));

    let found: DocsResult = serde_json::from_value(
        call(&c, "query", json!({"type": "Note", "where": {"n": {"$gte": 2}}})).await.unwrap(),
    )
    .unwrap();
    assert_eq!(found.docs.len(), 1);
    let hits: DocsResult =
        serde_json::from_value(call(&c, "search", json!({"text": "hi", "type": "Note"})).await.unwrap())
            .unwrap();
    assert_eq!(hits.docs[0].id, created.id);

    assert_eq!(call(&c, "delete", json!({"id": created.id})).await.unwrap(), Value::Null);
    let gone = call(&c, "get", json!({"id": created.id})).await.unwrap_err();
    assert_eq!(gone.code(), code::NOT_FOUND);
}

#[tokio::test]
async fn bad_requests_get_error_codes() {
    let s = Setup::start("errors").await;
    let c = s.client().await;
    for (method, params) in [
        ("create", json!({"type": "not a type"})),
        ("create", json!({"data": {}})),
        ("query", json!({"type": "Note", "where": {"a": {"$regex": "x"}}})),
        ("update", json!({"id": "x"})),
        ("drop", json!({})),
    ] {
        let e = call(&c, method, params.clone()).await.unwrap_err();
        assert_eq!(e.code(), "bad_request", "{method} {params}");
    }
    assert_eq!(
        call(&c, "update", json!({"id": "nope", "patch": {}})).await.unwrap_err().code(),
        code::NOT_FOUND
    );
}

/// The origin comes from the broker's stamp, so a service is recorded by its name.
#[tokio::test]
async fn changes_are_recorded_under_the_callers_identity() {
    let s = Setup::start("origin").await;
    let c = s.client().await;
    c.claim("notes-sync").await.unwrap();
    call(&c, "create", json!({"type": "Note", "data": {}})).await.unwrap();
    // Read the change log from a second connection to the same file (WAL).
    assert_eq!(recorded_origins(&s.db), ["notes-sync"]);
}

/// The origins recorded so far, read through a second `Store` on the same file.
fn recorded_origins(path: &std::path::Path) -> Vec<String> {
    let store = db::Store::open(path).unwrap();
    let mut stmt = store.connection().prepare("SELECT origin FROM change_sets ORDER BY id").unwrap();
    stmt.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
}

#[tokio::test]
async fn many_clients_at_once() {
    let s = Setup::start("many").await;
    let mut tasks = Vec::new();
    for i in 0..8 {
        let c = s.client().await;
        tasks.push(tokio::spawn(async move {
            for j in 0..25 {
                call(&c, "create", json!({"type": "Item", "data": {"client": i, "n": j}})).await.unwrap();
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let c = s.client().await;
    let all: DocsResult =
        serde_json::from_value(call(&c, "query", json!({"type": "Item", "limit": 1000})).await.unwrap())
            .unwrap();
    assert_eq!(all.docs.len(), 200);
    let one: DocsResult = serde_json::from_value(
        call(&c, "query", json!({"type": "Item", "where": {"client": 3}})).await.unwrap(),
    )
    .unwrap();
    assert_eq!(one.docs.len(), 25);
}
