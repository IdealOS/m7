//! permd on a real broker: managers grant and revoke, apps ask and the user
//! answers through the compositor's prompt, and every change is published.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bus_client::{Client, Incoming};
use busd::testing::TestBus;
use protocol::bus::code;
use protocol::perm::{GRANTS_TOPIC, GrantsResult, SERVICE};
use protocol::scene::PromptParams;
use serde_json::{Value, json};

struct Setup {
    dir: PathBuf,
    bus: TestBus,
    /// What the stand-in compositor was asked, and answers.
    prompts: Arc<Mutex<Vec<PromptParams>>>,
    allow: Arc<Mutex<bool>>,
}

impl Setup {
    async fn start(name: &str) -> Setup {
        let dir = std::env::temp_dir().join(format!("permd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bus = TestBus::start(&dir.join("bus.sock"), Box::new(busd::AllowAll)).await;
        let prompts: Arc<Mutex<Vec<PromptParams>>> = Arc::default();
        let allow = Arc::new(Mutex::new(true));
        let compositor = bus.system().await;
        compositor.claim("compositor").await.unwrap();
        let mut requests = compositor.take_requests().unwrap();
        let (seen, answer) = (prompts.clone(), allow.clone());
        tokio::spawn(async move {
            while let Some(Incoming::Request { id, method, params, .. }) = requests.next().await {
                assert_eq!(method, "prompt");
                seen.lock().unwrap().push(serde_json::from_value(params).unwrap());
                let allow = *answer.lock().unwrap();
                let _ = compositor.respond(id, Ok(json!({"allow": allow}))).await;
            }
        });
        let service = bus.system().await;
        service.claim(SERVICE).await.unwrap();
        tokio::spawn(permd::serve(service, permd::Store::open(&dir.join("grants.db")).unwrap()));
        bus.system().await.request(SERVICE, "grants", json!({})).await.unwrap();
        Setup { dir, bus, prompts, allow }
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn call(c: &Client, method: &str, params: Value) -> Result<Value, bus_client::CallError> {
    c.request(SERVICE, method, params).await
}

fn code_of(r: Result<Value, bus_client::CallError>) -> String {
    r.map(|v| format!("ok {v}")).unwrap_or_else(|e| e.code().to_owned())
}

#[tokio::test]
async fn managers_grant_and_revoke_and_every_change_is_published() {
    let s = Setup::start("manage").await;
    let tool = s.bus.system().await;
    let watcher = s.bus.system().await;
    let mut published = watcher.subscribe(GRANTS_TOPIC).await.unwrap();

    let made = call(&tool, "grant", json!({"app": "org.x.notes", "rules": [{"type": "Note", "access": ["read"]}, {"capability": "network"}]}))
        .await
        .unwrap();
    assert_eq!(made["grants"].as_array().unwrap().len(), 2);
    let table: GrantsResult = serde_json::from_value(published.next().await.unwrap().data).unwrap();
    assert_eq!(table.grants.len(), 2);
    let v1 = table.version;

    let id = table.grants[0].id;
    assert_eq!(call(&tool, "revoke", json!({"ids": [id]})).await.unwrap()["revoked"], 1);
    let table: GrantsResult = serde_json::from_value(published.next().await.unwrap().data).unwrap();
    assert_eq!(table.grants.len(), 1);
    assert!(table.version > v1);

    // Bad rules and ids are refused.
    for rules in [
        json!([]),
        json!([{"capability": "root"}]),
        json!([{"type": "Note", "access": ["read"], "filter": {"a": {"$regex": "x"}}}]),
    ] {
        assert_eq!(
            code_of(call(&tool, "grant", json!({"app": "org.x.notes", "rules": rules.clone()})).await),
            code::BAD_REQUEST,
            "{rules}"
        );
    }
    assert_eq!(
        code_of(call(&tool, "grant", json!({"app": "nodots", "rules": [{"capability": "network"}]})).await),
        code::BAD_REQUEST
    );

    let audit = call(&tool, "audit", Value::Null).await.unwrap();
    let actions: Vec<&str> =
        audit["entries"].as_array().unwrap().iter().map(|e| e["action"].as_str().unwrap()).collect();
    assert_eq!(actions, ["revoke", "grant", "grant"]);
}

#[tokio::test]
async fn apps_see_their_own_grants_and_cant_manage() {
    let s = Setup::start("apps").await;
    let tool = s.bus.system().await;
    call(&tool, "grant", json!({"app": "org.x.a", "rules": [{"capability": "network"}]})).await.unwrap();
    call(&tool, "grant", json!({"app": "org.x.b", "rules": [{"capability": "clipboard"}]})).await.unwrap();
    let a = s.bus.app("org.x.a").await;
    let mine = call(&a, "grants", json!({"app": "org.x.b"})).await.unwrap();
    assert_eq!(mine["grants"].as_array().unwrap().len(), 1);
    assert_eq!(mine["grants"][0]["app"], "org.x.a", "only its own, whatever it asks");
    for (method, params) in [
        ("grant", json!({"app": "org.x.a", "rules": [{"capability": "clipboard"}]})),
        ("revoke", json!({"app": "org.x.b"})),
        ("audit", json!({})),
    ] {
        assert_eq!(code_of(call(&a, method, params).await), code::PERMISSION_DENIED, "{method}");
    }
    // Settings, with the permissions capability, may.
    call(&tool, "grant", json!({"app": "org.x.settings", "rules": [{"capability": "permissions"}]}))
        .await
        .unwrap();
    let settings = s.bus.app("org.x.settings").await;
    assert_eq!(call(&settings, "revoke", json!({"app": "org.x.b"})).await.unwrap()["revoked"], 1);
    assert_eq!(
        call(&settings, "audit", json!({"limit": 1})).await.unwrap()["entries"][0]["by"],
        "org.x.settings"
    );
}

#[tokio::test]
async fn a_request_prompts_the_user_and_their_answer_counts() {
    let s = Setup::start("request").await;
    let app = s.bus.app("org.x.photos").await;
    let ask = json!({"rules": [{"type": "Image", "access": ["read"]}], "reason": "to show your pictures"});
    assert_eq!(call(&app, "request", ask.clone()).await.unwrap()["granted"], true);
    {
        let prompts = s.prompts.lock().unwrap();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].app, "org.x.photos");
        assert_eq!(prompts[0].name, "org.x.photos", "no App document, so the id");
        assert_eq!(prompts[0].asks, ["read your Image documents"]);
        assert_eq!(prompts[0].reason.as_deref(), Some("to show your pictures"));
    }
    let mine = call(&app, "grants", json!({})).await.unwrap();
    assert_eq!(mine["grants"][0]["by"], "user");
    // Asking again for what it has doesn't prompt.
    assert_eq!(call(&app, "request", ask).await.unwrap()["granted"], true);
    assert_eq!(s.prompts.lock().unwrap().len(), 1);

    // Turned down: nothing granted, and the audit log says so.
    *s.allow.lock().unwrap() = false;
    let net = json!({"rules": [{"capability": "network"}]});
    assert_eq!(call(&app, "request", net).await.unwrap()["granted"], false);
    assert_eq!(call(&app, "grants", json!({})).await.unwrap()["grants"].as_array().unwrap().len(), 1);
    let tool = s.bus.system().await;
    assert_eq!(call(&tool, "audit", json!({"limit": 1})).await.unwrap()["entries"][0]["action"], "deny");

    // Some things can't be asked for at all.
    let manage = json!({"rules": [{"capability": "permissions"}]});
    assert_eq!(code_of(call(&app, "request", manage).await), code::PERMISSION_DENIED);
    assert_eq!(
        code_of(call(&tool, "request", json!({"rules": [{"capability": "network"}]})).await),
        code::BAD_REQUEST
    );
}

#[tokio::test]
async fn grants_survive_a_restart() {
    let dir = std::env::temp_dir().join(format!("permd-restart-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("grants.db");
    for round in 0..2 {
        let bus = TestBus::start(&dir.join("bus.sock"), Box::new(busd::AllowAll)).await;
        let service = bus.system().await;
        service.claim(SERVICE).await.unwrap();
        let task = tokio::spawn(permd::serve(service, permd::Store::open(&path).unwrap()));
        let tool = bus.system().await;
        if round == 0 {
            call(&tool, "grant", json!({"app": "org.x.a", "rules": [{"capability": "network"}]}))
                .await
                .unwrap();
        } else {
            let all = tokio::time::timeout(Duration::from_secs(2), call(&tool, "grants", json!({})))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(all["grants"][0]["capability"], "network");
        }
        task.abort();
    }
    let _ = std::fs::remove_dir_all(&dir);
}
