//! The broker and client together over a real Unix socket.

use std::path::PathBuf;
use std::time::Duration;

use bus_client::{CallError, Client, Incoming};
use protocol::bus::{Sender, TAP_TOPIC, code};
use serde_json::{Value, json};
use tokio::net::UnixListener;

/// A broker on a fresh socket, served on this test's runtime.
struct Bus {
    path: PathBuf,
    _dir: TempDir,
}

impl Bus {
    fn start(name: &str) -> Bus {
        let dir = TempDir::new(name);
        let path = dir.0.join("bus.sock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(busd::serve(listener, Box::new(busd::AllowAll)));
        Bus { path, _dir: dir }
    }

    async fn client(&self) -> Client {
        Client::connect(&self.path).await.unwrap()
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> TempDir {
        // Short: Unix socket paths are limited to about 100 bytes on macOS.
        let dir = std::env::temp_dir().join(format!("busd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A service that answers `echo` with its params and `whoami` with the sender,
/// and never answers `hang`.
async fn echo_service(bus: &Bus, name: &str) -> Client {
    let client = bus.client().await;
    client.claim(name).await.unwrap();
    let mut requests = client.take_requests().unwrap();
    let responder = client.clone();
    tokio::spawn(async move {
        while let Some(incoming) = requests.next().await {
            if let Incoming::Request { id, from, method, params } = incoming {
                let answer = match method.as_str() {
                    "echo" => params,
                    "whoami" => serde_json::to_value(from).unwrap(),
                    _ => continue,
                };
                responder.respond(id, Ok(answer)).await.unwrap();
            }
        }
    });
    client
}

#[tokio::test]
async fn a_request_reaches_a_named_service_and_comes_back() {
    let bus = Bus::start("roundtrip");
    let _svc = echo_service(&bus, "echo").await;
    let client = bus.client().await;
    let answer = client.request("echo", "echo", json!({"n": 1})).await.unwrap();
    assert_eq!(answer, json!({"n": 1}));

    // The service sees the broker's stamp, not anything the client claims.
    let (conn, me) = client.hello().await.unwrap();
    assert_eq!(me, Sender::Conn(conn));
    let seen = client.request("echo", "whoami", Value::Null).await.unwrap();
    assert_eq!(seen, json!({"conn": conn}));
}

#[tokio::test]
async fn errors_come_back_as_codes() {
    let bus = Bus::start("errors");
    let _svc = echo_service(&bus, "echo").await;
    let client = bus.client().await;
    let err = client.request("nobody", "x", Value::Null).await.unwrap_err();
    assert_eq!(err.code(), code::NO_SUCH_SERVICE);
    let err =
        client.request_timeout("echo", "hang", Value::Null, Duration::from_millis(50)).await.unwrap_err();
    assert_eq!(err.code(), code::TIMEOUT);
    let other = bus.client().await;
    assert_eq!(other.claim("echo").await.unwrap_err().code(), code::NAME_TAKEN);
}

#[tokio::test]
async fn many_requests_in_flight_each_get_their_own_answer() {
    let bus = Bus::start("inflight");
    let _svc = echo_service(&bus, "echo").await;
    let a = bus.client().await;
    let b = bus.client().await;
    let mut tasks = Vec::new();
    for i in 0..200 {
        let c = if i % 2 == 0 { a.clone() } else { b.clone() };
        tasks.push(tokio::spawn(async move { (i, c.request("echo", "echo", json!(i)).await.unwrap()) }));
    }
    for t in tasks {
        let (i, answer) = t.await.unwrap();
        assert_eq!(answer, json!(i));
    }
}

#[tokio::test]
async fn subscribers_get_publications_with_the_publishers_name() {
    let bus = Bus::start("pubsub");
    let publisher = bus.client().await;
    publisher.claim("db").await.unwrap();
    let client = bus.client().await;
    let mut live = client.subscribe("db.live.*").await.unwrap();
    let mut exact = client.subscribe("db.live.7").await.unwrap();

    publisher.publish("db.live.7", json!({"added": [1]})).await.unwrap();
    publisher.publish("db.other", json!("not for us")).await.unwrap();
    publisher.publish("db.live.8", json!({"added": [2]})).await.unwrap();

    let p = live.next().await.unwrap();
    assert_eq!((p.topic.as_str(), &p.data), ("db.live.7", &json!({"added": [1]})));
    assert_eq!(p.from, Some(Sender::Service("db".into())));
    assert_eq!(live.next().await.unwrap().topic, "db.live.8");
    assert_eq!(exact.next().await.unwrap().topic, "db.live.7");

    // Dropping the last subscription on a pattern unsubscribes at the broker.
    drop(exact);
    publisher.publish("db.live.7", Value::Null).await.unwrap();
    assert_eq!(live.next().await.unwrap().topic, "db.live.7");
}

#[tokio::test]
async fn giving_up_on_a_request_cancels_it_at_the_service() {
    let bus = Bus::start("cancel");
    let svc = bus.client().await;
    svc.claim("slow").await.unwrap();
    let mut requests = svc.take_requests().unwrap();
    let client = bus.client().await;

    let gave_up =
        tokio::time::timeout(Duration::from_millis(50), client.request("slow", "work", Value::Null)).await;
    assert!(gave_up.is_err(), "the service never answers");
    let Some(Incoming::Request { id, .. }) = requests.next().await else { panic!() };
    assert_eq!(requests.next().await, Some(Incoming::Cancel { id }));
    // Answering anyway is harmless.
    svc.respond(id, Ok(Value::Null)).await.unwrap();
}

#[tokio::test]
async fn a_service_going_away_fails_its_pending_requests() {
    let bus = Bus::start("gone");
    let svc = bus.client().await;
    svc.claim("flaky").await.unwrap();
    let mut requests = svc.take_requests().unwrap();
    let client = bus.client().await;
    let call = tokio::spawn({
        let client = client.clone();
        async move { client.request("flaky", "x", Value::Null).await }
    });
    assert!(matches!(requests.next().await, Some(Incoming::Request { .. })));
    drop(requests);
    drop(svc);
    assert_eq!(call.await.unwrap().unwrap_err().code(), code::SERVICE_GONE);
    // The name is free for a restarted service.
    let again = bus.client().await;
    again.claim("flaky").await.unwrap();
}

#[tokio::test]
async fn the_tap_shows_traffic() {
    let bus = Bus::start("tap");
    let _svc = echo_service(&bus, "echo").await;
    let tapper = bus.client().await;
    let mut tap = tapper.subscribe(TAP_TOPIC).await.unwrap();
    let client = bus.client().await;
    client.request("echo", "echo", json!("hi")).await.unwrap();
    let mut methods = Vec::new();
    while methods.len() < 2 {
        let p = tap.next().await.unwrap();
        methods.push(p.data["msg"]["kind"].as_str().unwrap().to_owned());
    }
    assert_eq!(methods, ["request", "response"]);
}

/// A connection that stops reading is dropped instead of queueing forever, and
/// everyone else carries on.
#[tokio::test]
async fn a_subscriber_that_stops_reading_is_dropped() {
    use tokio::io::AsyncWriteExt;
    let bus = Bus::start("slow");
    // A raw connection that subscribes and then never reads.
    let mut raw = tokio::net::UnixStream::connect(&bus.path).await.unwrap();
    let sub =
        protocol::bus::encode_frame(&protocol::Message::Subscribe { id: 1, topic: "flood".into() }).unwrap();
    raw.write_all(&sub).await.unwrap();
    let healthy = bus.client().await;
    let mut flood = healthy.subscribe("flood").await.unwrap();

    let publisher = bus.client().await;
    let big = json!("x".repeat(1024));
    let total = busd::server::OUTBOX_FRAMES * 2;
    let reader = tokio::spawn(async move {
        for _ in 0..total {
            flood.next().await.unwrap();
        }
    });
    for _ in 0..total {
        publisher.publish("flood", big.clone()).await.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(20), reader)
        .await
        .expect("the healthy subscriber kept up")
        .unwrap();
    let services = publisher.request("bus", "list", Value::Null).await.unwrap();
    assert_eq!(services["connections"], json!(2), "the stuck connection is gone: {services}");
    drop(raw);
}

/// A tap that can't keep up misses trace messages but stays connected: it's a
/// debugging tool, and the busiest moments are when it's wanted most.
#[tokio::test]
async fn a_tap_that_falls_behind_stays_connected() {
    use tokio::io::AsyncWriteExt;
    let bus = Bus::start("taplag");
    let mut raw = tokio::net::UnixStream::connect(&bus.path).await.unwrap();
    let sub = protocol::bus::encode_frame(&protocol::Message::Subscribe { id: 1, topic: TAP_TOPIC.into() })
        .unwrap();
    raw.write_all(&sub).await.unwrap();
    let publisher = bus.client().await;
    publisher.hello().await.unwrap(); // the subscribe has been routed by now
    let big = json!("x".repeat(1024));
    for _ in 0..busd::server::OUTBOX_FRAMES * 2 {
        publisher.publish("nobody.listens", big.clone()).await.unwrap();
    }
    let list = publisher.request("bus", "list", Value::Null).await.unwrap();
    assert_eq!(list["connections"], json!(2), "the tap is still connected: {list}");
    drop(raw);
}

/// Big frames fill a stuck connection's queue by size long before by count, so it
/// can't make the broker hold gigabytes.
#[tokio::test]
async fn a_stuck_subscriber_is_dropped_by_queued_bytes() {
    use tokio::io::AsyncWriteExt;
    let bus = Bus::start("bytes");
    let mut raw = tokio::net::UnixStream::connect(&bus.path).await.unwrap();
    let sub =
        protocol::bus::encode_frame(&protocol::Message::Subscribe { id: 1, topic: "big".into() }).unwrap();
    raw.write_all(&sub).await.unwrap();
    let publisher = bus.client().await;
    publisher.hello().await.unwrap(); // the subscribe has been routed by now
    let frame = json!("x".repeat(1024 * 1024));
    // 32 MiB in 32 frames: far under the frame limit, twice the byte limit.
    for _ in 0..32 {
        publisher.publish("big", frame.clone()).await.unwrap();
    }
    let list = publisher.request("bus", "list", Value::Null).await.unwrap();
    assert_eq!(list["connections"], json!(1), "the stuck subscriber is gone: {list}");
    drop(raw);
}

#[tokio::test]
async fn requests_after_the_broker_stops_fail_cleanly() {
    let dir = TempDir::new("stopped");
    let path = dir.0.join("bus.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server = tokio::spawn(busd::serve(listener, Box::new(busd::AllowAll)));
    let client = Client::connect(&path).await.unwrap();
    client.hello().await.unwrap();
    server.abort();
    // The router task holds the connections; give it a moment to be dropped.
    let err = loop {
        match client.request_timeout("bus", "hello", Value::Null, Duration::from_millis(200)).await {
            Ok(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            Err(e) => break e,
        }
    };
    assert_eq!(err, CallError::Disconnected);
}
