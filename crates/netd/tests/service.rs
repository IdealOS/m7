//! netd on a real broker with the pretend network: status, scanning, joining
//! (with a wrong password first), watching for changes, forgetting.

use std::time::Duration;

use busd::testing::TestBus;
use protocol::net::{NetStatus, NetworksResult};
use serde_json::json;

#[tokio::test]
async fn settings_can_join_wifi_and_hear_the_change() {
    let dir = std::env::temp_dir().join(format!("netd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bus = TestBus::start(&dir.join("bus.sock"), Box::new(busd::AllowAll)).await;
    let svc = bus.system().await;
    svc.claim("net").await.unwrap();
    tokio::spawn(netd::serve(svc, netd::Backend::Fake(netd::fake::Fake::default())));
    let settings = bus.app("org.idealos.settings").await;

    let status: NetStatus =
        serde_json::from_value(settings.request("net", "status", json!(null)).await.unwrap()).unwrap();
    assert_eq!(status.connections.len(), 1, "wired");
    let mut watch = settings.request_stream("net", "watch", json!(null)).await.unwrap();
    let first: NetStatus = serde_json::from_value(watch.next().await.unwrap().unwrap()).unwrap();
    assert_eq!(first, status);

    let nets: NetworksResult =
        serde_json::from_value(settings.request("net", "networks", json!({"rescan": true})).await.unwrap())
            .unwrap();
    assert_eq!(nets.networks[0].ssid, "Home");
    assert!(nets.networks[0].secure && !nets.networks[0].known);

    let e =
        settings.request("net", "connect", json!({"ssid": "Home", "password": "nope"})).await.unwrap_err();
    assert_eq!(e.code(), "connect_failed");
    let e = settings.request("net", "connect", json!({"ssid": "Nowhere"})).await.unwrap_err();
    assert_eq!(e.code(), "not_found");
    settings.request("net", "connect", json!({"ssid": "Home", "password": "password"})).await.unwrap();

    // The watcher hears it at once (not at the next poll).
    let changed = tokio::time::timeout(Duration::from_secs(1), watch.next()).await.expect("a prompt update");
    let changed: NetStatus = serde_json::from_value(changed.unwrap().unwrap()).unwrap();
    assert!(changed.connections.iter().any(|c| c.name == "Home" && c.kind == "wifi"));

    let nets: NetworksResult =
        serde_json::from_value(settings.request("net", "networks", json!(null)).await.unwrap()).unwrap();
    assert!(nets.networks[0].active && nets.networks[0].known);
    settings.request("net", "forget", json!({"ssid": "Home"})).await.unwrap();
    settings.request("net", "set_wifi", json!({"enabled": false})).await.unwrap();
    let off: NetStatus =
        serde_json::from_value(settings.request("net", "status", json!(null)).await.unwrap()).unwrap();
    assert!(!off.wifi_enabled && off.connections.len() == 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn without_network_manager_it_says_so() {
    let b = netd::Backend::None;
    let s = b.status().await.unwrap();
    assert_eq!(s.state, protocol::net::NetState::Unavailable);
}
