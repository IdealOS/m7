//! Apps get only what they may: the documents they created and what their grants
//! cover, followed live from the table permd publishes.

use std::path::PathBuf;
use std::time::Duration;

use bus_client::{CallError, Client, Incoming};
use busd::testing::TestBus;
use protocol::bus::code::PERMISSION_DENIED;
use protocol::db::{Doc, DocsResult, SERVICE, WatchEvent};
use protocol::perm::{self, Access, Grant, GrantsResult, Rule};
use serde_json::{Value, json};
use tokio::sync::watch;

struct Setup {
    dir: PathBuf,
    bus: TestBus,
    /// The grant table the stand-in permd serves and publishes.
    table: watch::Sender<GrantsResult>,
}

impl Setup {
    async fn start(name: &str) -> Setup {
        let dir = std::env::temp_dir().join(format!("dbd-access-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bus = TestBus::start(&dir.join("bus.sock"), Box::new(busd::AllowAll)).await;
        let (table, mut changes) = watch::channel(GrantsResult::default());
        // A stand-in permd: answers `grants`, publishes each change.
        let perm = bus.system().await;
        perm.claim(perm::SERVICE).await.unwrap();
        let mut requests = perm.take_requests().unwrap();
        let current = changes.clone();
        let answers = perm.clone();
        tokio::spawn(async move {
            while let Some(Incoming::Request { id, .. }) = requests.next().await {
                let t = serde_json::to_value(&*current.borrow()).unwrap();
                let _ = answers.respond(id, Ok(t)).await;
            }
        });
        tokio::spawn(async move {
            while changes.changed().await.is_ok() {
                let t = serde_json::to_value(&*changes.borrow_and_update()).unwrap();
                perm.publish(perm::GRANTS_TOPIC, t).await.unwrap();
            }
        });
        let service = bus.system().await;
        service.claim(SERVICE).await.unwrap();
        let db = dir.join("test.db");
        tokio::spawn(dbd::serve(service, db::Store::open(&db).unwrap(), dbd::Config::beside(&db)));
        // dbd is serving once it answers.
        bus.system().await.request(SERVICE, "schemas", Value::Null).await.unwrap();
        Setup { dir, bus, table }
    }

    /// Replace the grants, and give dbd a moment to take them.
    async fn grant(&self, grants: &[(&str, Rule)]) {
        self.table.send_modify(|t| {
            t.version += 1;
            t.grants = grants
                .iter()
                .enumerate()
                .map(|(i, (app, rule))| Grant {
                    id: i as i64 + 1,
                    app: app.to_string(),
                    rule: rule.clone(),
                    granted: 0,
                    by: "test".into(),
                })
                .collect();
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
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

fn denied(r: Result<Value, CallError>) -> bool {
    matches!(&r, Err(e) if e.code() == PERMISSION_DENIED)
}

async fn query(c: &Client, q: Value) -> Vec<String> {
    let found: DocsResult = serde_json::from_value(call(c, "query", q).await.unwrap()).unwrap();
    found.docs.into_iter().map(|d| d.id).collect()
}

async fn create(c: &Client, doc_type: &str, data: Value) -> Result<Doc, CallError> {
    call(c, "create", json!({"type": doc_type, "data": data}))
        .await
        .map(|v| serde_json::from_value(v).unwrap())
}

#[tokio::test]
async fn an_app_without_grants_gets_permission_denied() {
    let s = Setup::start("none").await;
    let tool = s.bus.system().await;
    let app = s.bus.app("org.example.nosy").await;
    let note = create(&tool, "Note", json!({"title": "private"})).await.unwrap();

    assert!(denied(create(&app, "Note", json!({})).await.map(|_| Value::Null)));
    assert!(denied(call(&app, "get", json!({"id": note.id})).await));
    assert!(denied(call(&app, "update", json!({"id": note.id, "patch": {"x": 1}})).await));
    assert!(denied(call(&app, "delete", json!({"id": note.id})).await));
    assert!(denied(call(&app, "history", json!({"id": note.id})).await));
    assert!(denied(
        call(&app, "splice", json!({"id": note.id, "path": "title", "at": 0, "delete": 0, "insert": "x"}))
            .await
    ));
    assert!(denied(call(&app, "snapshots", Value::Null).await));
    assert!(call(&app, "compact", Value::Null).await.is_err(), "only the system compacts");
    assert!(denied(call(&app, "pin", json!({"id": note.id, "change_set": 1})).await));
    assert!(query(&app, json!({"type": "Note"})).await.is_empty(), "queries see nothing");
    let hits: DocsResult =
        serde_json::from_value(call(&app, "search", json!({"text": "private"})).await.unwrap()).unwrap();
    assert!(hits.docs.is_empty(), "nor does search");
    // An app can't pretend to be another.
    assert!(denied(call(&app, "get", json!({"id": note.id, "as": "org.example.other"})).await));
    assert_eq!(call(&app, "schemas", Value::Null).await.map(|_| ()), Ok(()));
}

#[tokio::test]
async fn grants_cover_types_and_filters_and_apps_own_what_they_create() {
    let s = Setup::start("grants").await;
    let tool = s.bus.system().await;
    let app = s.bus.app("org.example.frame").await;
    let family = create(&tool, "Photo", json!({"album": "Family"})).await.unwrap();
    let work = create(&tool, "Photo", json!({"album": "Work"})).await.unwrap();
    let read_family = Rule::Data(perm::DataRule {
        doc_type: "Photo".into(),
        access: vec![Access::Read, Access::Create],
        filter: json!({"album": "Family"}).as_object().unwrap().clone(),
    });
    s.grant(&[("org.example.frame", read_family)]).await;

    assert_eq!(query(&app, json!({"type": "Photo"})).await, std::slice::from_ref(&family.id));
    assert!(call(&app, "get", json!({"id": family.id})).await.is_ok());
    assert!(denied(call(&app, "get", json!({"id": work.id})).await));
    assert!(
        denied(call(&app, "update", json!({"id": family.id, "patch": {"x": 1}})).await),
        "read isn't write"
    );
    // It may create only what its filter covers, and owns what it creates.
    assert!(denied(create(&app, "Photo", json!({"album": "Work"})).await.map(|_| Value::Null)));
    let mine = create(&app, "Photo", json!({"album": "Family", "by": "me"})).await.unwrap();
    call(&app, "update", json!({"id": mine.id, "patch": {"album": "Holiday"}})).await.unwrap();
    // It may pin versions of what it may change, and only those.
    let first =
        call(&app, "history", json!({"id": mine.id})).await.unwrap()["versions"][0]["change_set"].clone();
    call(&app, "pin", json!({"id": mine.id, "change_set": first})).await.unwrap();
    assert_eq!(call(&app, "history", json!({"id": mine.id})).await.unwrap()["versions"][0]["pinned"], true);
    assert!(
        denied(call(&app, "pin", json!({"id": family.id, "change_set": first})).await),
        "read isn't write"
    );
    assert_eq!(query(&app, json!({"type": "Photo", "orderBy": [["by", "asc"]]})).await.len(), 2);
    // Another app sees none of it.
    let other = s.bus.app("org.example.other").await;
    assert!(query(&other, json!({"type": "Photo"})).await.is_empty());
    // Its own change sets, and only those, are listed and undoable.
    let sets = call(&app, "changes", json!({})).await.unwrap();
    assert!(sets["change_sets"].as_array().unwrap().iter().all(|c| c["origin"] == "org.example.frame"));
    let tools_first =
        call(&tool, "history", json!({"id": work.id})).await.unwrap()["versions"][0]["change_set"].clone();
    assert!(denied(call(&app, "undo", json!({"change_set": tools_first})).await));
}

#[tokio::test]
async fn revoking_takes_effect_at_once_even_for_live_queries() {
    let s = Setup::start("revoke").await;
    let tool = s.bus.system().await;
    let app = s.bus.app("org.example.notes").await;
    let note = create(&tool, "Note", json!({"title": "shared"})).await.unwrap();
    s.grant(&[("org.example.notes", Rule::data("Note", &[Access::Read]))]).await;

    let mut live = app.request_stream(SERVICE, "watch", json!({"type": "Note"})).await.unwrap();
    let first: WatchEvent = serde_json::from_value(live.next().await.unwrap().unwrap()).unwrap();
    assert!(matches!(&first, WatchEvent::Initial { docs } if docs.len() == 1), "{first:?}");

    s.grant(&[]).await;
    assert!(denied(call(&app, "get", json!({"id": note.id})).await));
    let part = tokio::time::timeout(Duration::from_secs(2), live.next()).await.unwrap().unwrap().unwrap();
    let diff: WatchEvent = serde_json::from_value(part).unwrap();
    assert!(
        matches!(&diff, WatchEvent::Diff { removed, .. } if removed == std::slice::from_ref(&note.id)),
        "{diff:?}"
    );

    // Granting it back brings it back.
    s.grant(&[("org.example.notes", Rule::data("Note", &[Access::Read]))]).await;
    let part = tokio::time::timeout(Duration::from_secs(2), live.next()).await.unwrap().unwrap().unwrap();
    let diff: WatchEvent = serde_json::from_value(part).unwrap();
    assert!(matches!(&diff, WatchEvent::Diff { added, .. } if added.len() == 1), "{diff:?}");
}

#[tokio::test]
async fn services_acting_for_an_app_get_only_what_it_may() {
    let s = Setup::start("as").await;
    let tool = s.bus.system().await;
    let compositor = s.bus.system().await;
    let note = create(&tool, "Note", json!({"body": "hello"})).await.unwrap();
    let splice =
        |app: &str| json!({"id": note.id, "path": "body", "at": 5, "delete": 0, "insert": "!", "as": app});
    assert!(denied(call(&compositor, "splice", splice("org.example.app")).await));
    s.grant(&[("org.example.app", Rule::data("Note", &[Access::Write]))]).await;
    call(&compositor, "splice", splice("org.example.app")).await.unwrap();
    let history = call(&tool, "history", json!({"id": note.id})).await.unwrap();
    assert_eq!(history["versions"][1]["origin"], "org.example.app", "recorded as the app");
    let e = call(&compositor, "get", json!({"id": note.id, "as": "not an app"})).await.unwrap_err();
    assert_eq!(e.code(), "bad_request");
}

#[tokio::test]
async fn blobs_are_readable_through_a_readable_document() {
    use std::io::Write;
    let s = Setup::start("blobs").await;
    let tool = s.bus.system().await;
    let app = s.bus.app("org.example.viewer").await;
    let path = s.dir.join("blob");
    std::fs::File::create(&path).unwrap().write_all(b"picture").unwrap();
    let file = std::fs::File::open(&path).unwrap();
    let put = tool.request_with(SERVICE, "put_blob", Value::Null, vec![file.into()], None).await.unwrap();
    let hash = put.result["hash"].as_str().unwrap().to_owned();
    create(&tool, "Image", json!({"blob": {"$blob": hash}})).await.unwrap();
    let get = |c: Client| {
        let hash = hash.clone();
        async move {
            c.request_with(SERVICE, "get_blob", json!({"hash": hash}), Vec::new(), None)
                .await
                .map(|r| r.result)
        }
    };
    assert!(denied(get(app.clone()).await));
    assert!(denied(call(&app, "blob_info", json!({"hash": hash})).await));
    s.grant(&[("org.example.viewer", Rule::data("Image", &[Access::Read]))]).await;
    assert!(get(app.clone()).await.is_ok());
}

#[tokio::test]
async fn types_counts_only_what_an_app_may_read_and_stats_need_storage() {
    let s = Setup::start("types").await;
    let tool = s.bus.system().await;
    for t in ["Note", "Note", "Photo"] {
        create(&tool, t, json!({})).await.unwrap();
    }
    let all = tool.request(SERVICE, "types", Value::Null).await.unwrap();
    assert_eq!(all["types"], json!({"Note": 2, "Photo": 1}));

    let browser = s.bus.app("org.example.browser").await;
    assert_eq!(browser.request(SERVICE, "types", Value::Null).await.unwrap()["types"], json!({}));
    s.grant(&[("org.example.browser", Rule::data("*", &[Access::Read]))]).await;
    assert_eq!(
        browser.request(SERVICE, "types", Value::Null).await.unwrap()["types"],
        json!({"Note": 2, "Photo": 1})
    );
    assert_eq!(query(&browser, json!({"type": "Photo"})).await.len(), 1, "* reads every type");
    assert!(denied(create(&browser, "Note", json!({})).await.map(|_| Value::Null)), "but writes none");

    assert!(denied(browser.request(SERVICE, "stats", Value::Null).await));
    assert!(denied(browser.request(SERVICE, "snapshots", Value::Null).await));
    s.grant(&[("org.example.browser", Rule::capability("storage"))]).await;
    let stats = browser.request(SERVICE, "stats", Value::Null).await.unwrap();
    assert_eq!((stats["docs"].as_u64(), stats["types"].as_u64()), (Some(3), Some(2)));
    assert!(stats["db_bytes"].as_u64().unwrap() > 0);
    browser.request(SERVICE, "snapshot", Value::Null).await.unwrap();
    let snapshots = browser.request(SERVICE, "snapshots", Value::Null).await.unwrap();
    assert!(
        !snapshots["snapshots"].as_array().unwrap().is_empty(),
        "the one it took (dbd takes one at startup too)"
    );
    assert!(denied(browser.request(SERVICE, "restore", json!({"name": "x"})).await), "never restore");
}
