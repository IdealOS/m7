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
        let config = dbd::Config::beside(&db);
        tokio::spawn(dbd::serve(service, db::Store::open(&db).unwrap(), config));
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
async fn history_and_undo_over_the_bus() {
    let s = Setup::start("undo").await;
    let c = s.client().await;
    let d = doc(call(&c, "create", json!({"type": "Note", "data": {"title": "a"}})).await.unwrap());
    call(&c, "update", json!({"id": d.id, "patch": {"title": "b"}})).await.unwrap();
    let history: protocol::db::HistoryResult =
        serde_json::from_value(call(&c, "history", json!({"id": d.id})).await.unwrap()).unwrap();
    assert_eq!(history.versions.len(), 2);
    let edit = history.versions[1].change_set;
    let old =
        call(&c, "at", json!({"id": d.id, "change_set": history.versions[0].change_set})).await.unwrap();
    assert_eq!(old, json!({"data": {"title": "a"}}));

    let undo = call(&c, "undo", json!({"change_set": edit})).await.unwrap();
    assert!(undo["change_set"].as_i64().unwrap() > edit);
    assert_eq!(doc(call(&c, "get", json!({"id": d.id})).await.unwrap()).data["title"], "a");
    let changes = call(&c, "changes", json!({"limit": 1})).await.unwrap();
    assert_eq!(changes["change_sets"][0]["undoes"], edit);
    assert_eq!(call(&c, "changes", Value::Null).await.unwrap()["change_sets"].as_array().unwrap().len(), 3);
    assert_eq!(call(&c, "undo", json!({"change_set": 999})).await.unwrap_err().code(), "bad_request");
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

/// The next part of a live query, within a few seconds.
async fn next_part(stream: &mut bus_client::Stream) -> protocol::db::WatchEvent {
    let part = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .expect("a part within 5 s")
        .expect("the stream is open")
        .expect("not an error");
    serde_json::from_value(part).unwrap()
}

#[tokio::test]
async fn live_queries_over_the_bus() {
    use protocol::db::WatchEvent;
    let s = Setup::start("watch").await;
    let c = s.client().await;
    let first = doc(call(&c, "create", json!({"type": "Task", "data": {"n": 5}})).await.unwrap());
    let query = json!({"type": "Task", "where": {"done": {"$ne": true}}, "orderBy": [["n", "asc"]]});
    let mut live = c.request_stream(SERVICE, "watch", query.clone()).await.unwrap();
    let WatchEvent::Initial { docs } = next_part(&mut live).await else { panic!("not the initial part") };
    let mut list = docs;
    assert_eq!(list, std::slice::from_ref(&first));

    // Another client's writes show up; a burst of them arrives in fewer parts.
    let writer = s.client().await;
    for n in 0..20 {
        call(&writer, "create", json!({"type": "Task", "data": {"n": n}})).await.unwrap();
    }
    call(&writer, "update", json!({"id": first.id, "patch": {"done": true}})).await.unwrap();
    // Changes to other types don't wake this query.
    call(&writer, "create", json!({"type": "Note", "data": {}})).await.unwrap();
    let mut parts = 0;
    while list.len() != 20 || list.iter().any(|d| d.id == first.id) {
        let WatchEvent::Diff { added, changed, removed, order } = next_part(&mut live).await else {
            panic!()
        };
        parts += 1;
        list.retain(|d| !removed.contains(&d.id));
        for d in changed {
            let slot = list.iter_mut().find(|x| x.id == d.id).unwrap();
            *slot = d;
        }
        list.extend(added);
        if let Some(order) = order {
            list.sort_by_key(|d| order.iter().position(|id| *id == d.id).unwrap());
        }
    }
    assert!(parts < 21, "{parts} parts for 21 writes");
    let fresh: DocsResult = serde_json::from_value(call(&c, "query", query).await.unwrap()).unwrap();
    assert_eq!(list, fresh.docs);

    // Bad queries and unstreamed watches are refused.
    let mut bad = c
        .request_stream(SERVICE, "watch", json!({"type": "Task", "where": {"a": {"$regex": 1}}}))
        .await
        .unwrap();
    assert_eq!(bad.next().await.unwrap().unwrap_err().code(), "bad_request");
    assert!(bad.next().await.is_none());
    assert_eq!(call(&c, "watch", json!({"type": "Task"})).await.unwrap_err().code(), "bad_request");

    // Dropping the stream stops the live query; the service carries on.
    drop(live);
    call(&writer, "create", json!({"type": "Task", "data": {"n": 99}})).await.unwrap();
    assert_eq!(doc(call(&c, "get", json!({"id": first.id})).await.unwrap()).data["done"], true);
}

#[tokio::test]
async fn snapshots_restore_over_the_bus_and_live_queries_follow() {
    use protocol::db::{RestoreResult, SnapshotInfo, SnapshotsResult, WatchEvent};
    let s = Setup::start("snap").await;
    let c = s.client().await;
    let d = doc(call(&c, "create", json!({"type": "Note", "data": {"v": 1}})).await.unwrap());
    let first: SnapshotInfo =
        serde_json::from_value(call(&c, "snapshot", Value::Null).await.unwrap()).unwrap();
    call(&c, "update", json!({"id": d.id, "patch": {"v": 2}})).await.unwrap();
    let mut live = c.request_stream(SERVICE, "watch", json!({"type": "Note"})).await.unwrap();
    let WatchEvent::Initial { docs } = next_part(&mut live).await else { panic!() };
    assert_eq!(docs[0].data["v"], 2);

    let list: SnapshotsResult =
        serde_json::from_value(call(&c, "snapshots", Value::Null).await.unwrap()).unwrap();
    // The startup snapshot (if dbd took one before the first write) and ours.
    assert!(list.snapshots.iter().any(|x| x == &first));
    let restored: RestoreResult =
        serde_json::from_value(call(&c, "restore", json!({"name": first.name})).await.unwrap()).unwrap();
    assert_eq!(doc(call(&c, "get", json!({"id": d.id})).await.unwrap()).data["v"], 1);
    // The live query sees the restored state.
    let WatchEvent::Diff { changed, .. } = next_part(&mut live).await else { panic!() };
    assert_eq!(changed[0].data["v"], 1);
    // And the restore can be undone.
    call(&c, "restore", json!({"name": restored.saved.name})).await.unwrap();
    assert_eq!(doc(call(&c, "get", json!({"id": d.id})).await.unwrap()).data["v"], 2);
    assert_eq!(call(&c, "restore", json!({"name": "../test.db"})).await.unwrap_err().code(), "bad_request");
    assert_eq!(
        call(&c, "restore", json!({"name": "snapshot-1-1.db"})).await.unwrap_err().code(),
        code::NOT_FOUND
    );
}

/// A file with `text` in it, unlinked, as a descriptor.
fn file_with(dir: &std::path::Path, text: &[u8]) -> std::os::fd::OwnedFd {
    use std::io::{Seek, Write};
    let path = dir.join(format!("upload-{}", uuid_like()));
    let mut f =
        std::fs::OpenOptions::new().create(true).truncate(true).read(true).write(true).open(&path).unwrap();
    f.write_all(text).unwrap();
    f.rewind().unwrap();
    std::fs::remove_file(&path).unwrap();
    f.into()
}

fn uuid_like() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

#[tokio::test]
async fn blobs_go_in_and_out_as_descriptors() {
    use protocol::db::BlobInfo;
    use std::io::Read;
    let s = Setup::start("blobs").await;
    let c = s.client().await;
    let bytes = b"song bytes ".repeat(10_000);
    let put = c
        .request_with(SERVICE, "put_blob", Value::Null, vec![file_with(&s.dir, &bytes)], None)
        .await
        .unwrap();
    let info: BlobInfo = serde_json::from_value(put.result).unwrap();
    assert_eq!((info.size, info.refs), (bytes.len() as u64, 0));
    assert_eq!(info.hash.len(), 64);
    // The blob is on disk under its hash.
    assert!(s.dir.join("blobs").join(&info.hash[..2]).join(&info.hash[2..]).is_file());

    call(&c, "create", json!({"type": "Track", "data": {"title": "t", "audio": {"$blob": info.hash}}}))
        .await
        .unwrap();
    let got =
        c.request_with(SERVICE, "get_blob", json!({"hash": info.hash}), Vec::new(), None).await.unwrap();
    assert_eq!(got.result["refs"], 1);
    let [fd] = <[_; 1]>::try_from(got.fds).unwrap();
    let mut back = Vec::new();
    std::fs::File::from(fd).read_to_end(&mut back).unwrap();
    assert_eq!(back, bytes);

    // No descriptor, two descriptors, an unknown hash, a bad hash, and an
    // unrecorded blob in a document are all refused.
    assert_eq!(call(&c, "put_blob", Value::Null).await.unwrap_err().code(), "bad_request");
    let two = vec![file_with(&s.dir, b"a"), file_with(&s.dir, b"b")];
    assert_eq!(
        c.request_with(SERVICE, "put_blob", Value::Null, two, None).await.unwrap_err().code(),
        "bad_request"
    );
    let zeros = "0".repeat(64);
    assert_eq!(call(&c, "get_blob", json!({"hash": zeros})).await.unwrap_err().code(), code::NOT_FOUND);
    assert_eq!(call(&c, "blob_info", json!({"hash": "nope"})).await.unwrap_err().code(), code::NOT_FOUND);
    assert_eq!(call(&c, "get_blob", json!({"hash": "../../etc"})).await.unwrap_err().code(), code::NOT_FOUND);
    let missing = json!({"type": "Note", "data": {"f": {"$blob": zeros}}});
    assert_eq!(call(&c, "create", missing).await.unwrap_err().code(), "bad_request");
}

#[tokio::test]
async fn links_diffs_and_schemas_over_the_bus() {
    use protocol::db::{DiffResult, LinksResult, SchemasResult};
    let s = Setup::start("links").await;
    let c = s.client().await;
    let a = doc(call(&c, "create", json!({"type": "Contact", "data": {"name": "Ada"}})).await.unwrap());
    let b =
        doc(call(&c, "create", json!({"type": "Note", "data": {"about": {"$link": a.id}}})).await.unwrap());
    let links: LinksResult =
        serde_json::from_value(call(&c, "links", json!({"id": a.id})).await.unwrap()).unwrap();
    assert_eq!((links.incoming[0].id.as_str(), links.incoming[0].path.as_str()), (b.id.as_str(), "about"));
    let found: DocsResult = serde_json::from_value(
        call(&c, "query", json!({"type": "Note", "where": {"$linksTo": a.id}})).await.unwrap(),
    )
    .unwrap();
    assert_eq!(found.docs[0].id, b.id);

    call(&c, "update", json!({"id": a.id, "patch": {"organization": "AE"}})).await.unwrap();
    let h: protocol::db::HistoryResult =
        serde_json::from_value(call(&c, "history", json!({"id": a.id})).await.unwrap()).unwrap();
    let diff: DiffResult = serde_json::from_value(
        call(
            &c,
            "diff",
            json!({"id": a.id, "from": h.versions[0].change_set, "to": h.versions[1].change_set}),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(diff.changes.len(), 1);
    assert_eq!(diff.changes[0].path, "organization");

    let schemas: SchemasResult =
        serde_json::from_value(call(&c, "schemas", Value::Null).await.unwrap()).unwrap();
    assert!(schemas.schemas.contains_key("Email"));
    let e = call(&c, "create", json!({"type": "Contact", "data": {}})).await.unwrap_err();
    assert_eq!(e.code(), "bad_request");
    assert!(e.to_string().contains("name is required"), "{e}");
}
