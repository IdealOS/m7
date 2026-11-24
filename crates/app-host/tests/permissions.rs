//! Apps against the real permission stack: the broker's grants policy, permd and
//! dbd, with the app on an app connection as session makes them. An app starts
//! with nothing, asks, the user answers in the (stand-in) compositor's prompt,
//! and revoking takes effect at once.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use app_host::runtime::{self, Exit};
use bus_client::{Client, Incoming};
use busd::testing::TestBus;
use serde_json::{Value, json};

struct Stack {
    dir: PathBuf,
    bus: TestBus,
    /// The answer the stand-in compositor gives prompts, and how many it got.
    allow: Arc<Mutex<bool>>,
    prompts: Arc<Mutex<u32>>,
}

impl Stack {
    async fn start(name: &str) -> Stack {
        let dir = std::env::temp_dir().join(format!("ah-perm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bus = TestBus::start(&dir.join("bus.sock"), Box::new(busd::policy::Grants::new())).await;
        let perm = bus.system().await;
        perm.claim("perm").await.unwrap();
        tokio::spawn(permd::serve(perm, permd::Store::open(&dir.join("grants.db")).unwrap()));
        let db = bus.system().await;
        db.claim("db").await.unwrap();
        let db_path = dir.join("db.sqlite");
        tokio::spawn(dbd::serve(db, db::Store::open(&db_path).unwrap(), dbd::Config::beside(&db_path)));
        let (allow, prompts) = (Arc::new(Mutex::new(true)), Arc::new(Mutex::new(0)));
        let compositor = bus.system().await;
        compositor.claim("compositor").await.unwrap();
        let mut requests = compositor.take_requests().unwrap();
        let (answer, count) = (allow.clone(), prompts.clone());
        tokio::spawn(async move {
            while let Some(Incoming::Request { id, method, .. }) = requests.next().await {
                assert_eq!(method, "prompt");
                *count.lock().unwrap() += 1;
                let allow = *answer.lock().unwrap();
                let _ = compositor.respond(id, Ok(json!({"allow": allow}))).await;
            }
        });
        let tool = bus.system().await;
        tool.request("db", "schemas", Value::Null).await.unwrap();
        tool.request("perm", "grants", json!({})).await.unwrap();
        Stack { dir, bus, allow, prompts }
    }

    /// Run `source` as app `id` and return what it passed to `__idealos.exit`.
    async fn run(&self, id: &str, source: &str) -> i32 {
        let client = self.bus.app(id).await;
        let local = tokio::task::LocalSet::new();
        let exit = tokio::time::timeout(
            Duration::from_secs(10),
            local.run_until(runtime::run(client, "test.js", source)),
        )
        .await
        .expect("finished in time")
        .unwrap();
        match exit {
            Exit::Code(c) => c,
            other => panic!("{other:?}"),
        }
    }

    async fn tool(&self) -> Client {
        self.bus.system().await
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// JS that tries to create a Note: exits 0 if it could, 1 if refused with
/// permission_denied, 2 otherwise.
const CREATE: &str = r#"
__idealos.request("db", "create", {type: "Note", data: {title: "x"}}).then(
  () => __idealos.exit(0),
  (e) => __idealos.exit(e.code === "permission_denied" ? 1 : 2));
"#;

#[tokio::test]
async fn an_app_without_grants_is_refused_and_a_prompt_grants_access() {
    let s = Stack::start("prompt").await;
    assert_eq!(s.run("org.example.notes", CREATE).await, 1, "no grant: permission_denied");

    // The user turns it down, then allows it.
    let ask = r#"
      __idealos.request("perm", "request", {rules: [{type: "Note", access: ["create"]}], reason: "to save notes"}, {timeoutMs: 600000})
        .then((r) => __idealos.exit(r.granted ? 0 : 1), () => __idealos.exit(2));
    "#;
    *s.allow.lock().unwrap() = false;
    assert_eq!(s.run("org.example.notes", ask).await, 1);
    assert_eq!(s.run("org.example.notes", CREATE).await, 1);
    *s.allow.lock().unwrap() = true;
    assert_eq!(s.run("org.example.notes", ask).await, 0);
    assert_eq!(*s.prompts.lock().unwrap(), 2);
    // The broker and dbd follow permd's publish; give it a moment.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(s.run("org.example.notes", CREATE).await, 0, "granted");
    assert_eq!(s.run("org.example.other", CREATE).await, 1, "only for that app");
}

#[tokio::test]
async fn revoking_database_access_takes_effect_immediately() {
    let s = Stack::start("revoke").await;
    let tool = s.tool().await;
    tool.request(
        "perm",
        "grant",
        json!({"app": "org.example.notes", "rules": [{"type": "Note", "access": ["read", "create"]}]}),
    )
    .await
    .unwrap();
    tool.request("db", "create", json!({"type": "Note", "data": {"title": "theirs"}})).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let count = r#"__idealos.request("db", "query", {type: "Note"}).then((r) => __idealos.exit(r.docs.length), () => __idealos.exit(99));"#;
    assert_eq!(s.run("org.example.notes", count).await, 1);
    assert_eq!(s.run("org.example.notes", CREATE).await, 0);
    assert_eq!(s.run("org.example.notes", count).await, 2);

    tool.request("perm", "revoke", json!({"app": "org.example.notes"})).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(s.run("org.example.notes", count).await, 1, "only the one it made");
    assert_eq!(s.run("org.example.notes", CREATE).await, 1, "and it can't make more");
}

#[tokio::test]
async fn service_grants_open_other_services_to_an_app() {
    let s = Stack::start("service").await;
    let svc = s.tool().await;
    svc.claim("weather").await.unwrap();
    let mut requests = svc.take_requests().unwrap();
    tokio::spawn(async move {
        while let Some(Incoming::Request { id, .. }) = requests.next().await {
            let _ = svc.respond(id, Ok(json!("sunny"))).await;
        }
    });
    let ask = r#"__idealos.request("weather", "now", null).then(() => __idealos.exit(0), (e) => __idealos.exit(e.code === "permission_denied" ? 1 : 2));"#;
    assert_eq!(s.run("org.example.app", ask).await, 1);
    s.tool()
        .await
        .request(
            "perm",
            "grant",
            json!({"app": "org.example.app", "rules": [{"capability": "service:weather"}]}),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(s.run("org.example.app", ask).await, 0);
}
