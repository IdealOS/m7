//! secretsd on a real broker: each app and service sees only its own secrets.

use busd::testing::TestBus;
use serde_json::json;

#[tokio::test]
async fn each_owner_has_its_own_secrets() {
    let dir = std::env::temp_dir().join(format!("secretsd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bus = TestBus::start(&dir.join("bus.sock"), Box::new(busd::policy::Grants::new())).await;
    let svc = bus.system().await;
    svc.claim("secrets").await.unwrap();
    let key = secretsd::store::load_key(&dir.join("key")).unwrap();
    tokio::spawn(secretsd::serve(svc, secretsd::Store::open(&dir.join("secrets.db"), &key).unwrap()));

    let mail = bus.app("org.example.mail").await;
    let other = bus.app("org.example.other").await;
    let sync = bus.system().await;
    sync.claim("mailsync").await.unwrap();
    mail.request("secrets", "put", json!({"name": "token", "value": "app-secret"})).await.unwrap();
    sync.request("secrets", "put", json!({"name": "token", "value": "service-secret"})).await.unwrap();

    assert_eq!(
        mail.request("secrets", "get", json!({"name": "token"})).await.unwrap()["value"],
        "app-secret"
    );
    assert_eq!(
        sync.request("secrets", "get", json!({"name": "token"})).await.unwrap()["value"],
        "service-secret"
    );
    let e = other.request("secrets", "get", json!({"name": "token"})).await.unwrap_err();
    assert_eq!(e.code(), "not_found", "another app sees nothing");
    assert_eq!(other.request("secrets", "list", json!({})).await.unwrap()["names"], json!([]));
    // A tool has no identity to keep secrets under.
    let tool = bus.system().await;
    assert_eq!(tool.request("secrets", "list", json!({})).await.unwrap_err().code(), "permission_denied");

    mail.request("secrets", "delete", json!({"name": "token"})).await.unwrap();
    assert_eq!(mail.request("secrets", "list", json!({})).await.unwrap()["names"], json!([]));
    assert_eq!(sync.request("secrets", "list", json!({})).await.unwrap()["names"], json!(["token"]));
    let _ = std::fs::remove_dir_all(&dir);
}
