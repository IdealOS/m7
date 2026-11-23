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
        // As busd does: these tests run the broker and many clients in one process.
        busd::raise_fd_limit().unwrap();
        let dir = TempDir::new(name);
        let path = dir.0.join("bus.sock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(busd::serve(listener, Box::new(busd::AllowAll), None));
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
            if let Incoming::Request { id, from, method, params, .. } = incoming {
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
    assert!(matches!(requests.next().await, Some(Incoming::Cancel { id: cancelled }) if cancelled == id));
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

/// A file with `text` in it, already unlinked, as a descriptor.
fn file_with(text: &str) -> std::os::fd::OwnedFd {
    use std::io::Write;
    // Unique per call: tests run in parallel and each unlinks its file at once.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("busd-fd-{}-{n}", std::process::id()));
    let mut f =
        std::fs::File::options().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    f.write_all(text.as_bytes()).unwrap();
    f.into()
}

fn read_all(fd: std::os::fd::OwnedFd) -> String {
    use std::io::{Read, Seek};
    let mut f = std::fs::File::from(fd);
    f.rewind().unwrap();
    let mut text = String::new();
    f.read_to_string(&mut text).unwrap();
    text
}

/// Descriptors cross the broker in both directions and still work on the far side.
#[tokio::test]
async fn descriptors_travel_with_requests_responses_and_publishes() {
    let bus = Bus::start("fds");
    let svc = bus.client().await;
    svc.claim("files").await.unwrap();
    let mut requests = svc.take_requests().unwrap();
    let responder = svc.clone();
    tokio::spawn(async move {
        while let Some(Incoming::Request { id, fds, .. }) = requests.next().await {
            // Read what came in; answer with a new file holding it reversed.
            let text: String = fds.into_iter().map(read_all).collect::<Vec<_>>().join("+");
            let reply = file_with(&text.chars().rev().collect::<String>());
            responder.respond_with(id, Ok(json!(text.len())), vec![reply]).await.unwrap();
        }
    });
    let client = bus.client().await;
    let reply = client
        .request_with("files", "put", Value::Null, vec![file_with("ab"), file_with("cd")], None)
        .await
        .unwrap();
    assert_eq!(reply.result, json!(5));
    assert_eq!(read_all(reply.fds.into_iter().next().unwrap()), "dc+ba");

    // Two subscribers, one with two matching patterns: each copy works.
    let other = bus.client().await;
    let mut a = client.subscribe("frames.*").await.unwrap();
    let mut b = client.subscribe("frames.new").await.unwrap();
    let mut c = other.subscribe("frames.new").await.unwrap();
    svc.publish_with("frames.new", Value::Null, vec![file_with("pixels")]).await.unwrap();
    for sub in [&mut a, &mut b, &mut c] {
        let p = sub.next().await.unwrap();
        assert_eq!(read_all(p.fds.into_iter().next().unwrap()), "pixels");
    }
}

/// A connection that declares descriptors it didn't send is broken, not trusted
/// to line up later.
#[tokio::test]
async fn a_message_missing_its_descriptors_drops_the_connection() {
    use tokio::io::AsyncWriteExt;
    let bus = Bus::start("liar");
    let mut raw = tokio::net::UnixStream::connect(&bus.path).await.unwrap();
    let lie = protocol::bus::encode_frame(&protocol::Message::publish("x", Value::Null).with_fds(1)).unwrap();
    raw.write_all(&lie).await.unwrap();
    let mut buf = [0u8; 16];
    use tokio::io::AsyncReadExt;
    let n = tokio::time::timeout(Duration::from_secs(5), raw.read(&mut buf)).await.unwrap().unwrap();
    assert_eq!(n, 0, "the broker closed the connection");
}

/// Descriptors waiting for a connection that stopped reading count against its
/// queue, so it can't make the broker hold thousands of them open.
#[tokio::test]
async fn a_stuck_subscriber_is_dropped_by_queued_descriptors() {
    use tokio::io::AsyncWriteExt;
    let bus = Bus::start("fdcap");
    let mut raw = tokio::net::UnixStream::connect(&bus.path).await.unwrap();
    let sub =
        protocol::bus::encode_frame(&protocol::Message::Subscribe { id: 1, topic: "fds".into() }).unwrap();
    raw.write_all(&sub).await.unwrap();
    let publisher = bus.client().await;
    publisher.hello().await.unwrap(); // the subscribe has been routed by now
    // Small frames, far under the frame and byte limits; 16 descriptors each.
    for _ in 0..(busd::server::OUTBOX_FDS / 16) * 4 {
        let fds = (0..16).map(|_| file_with("x")).collect();
        publisher.publish_with("fds", Value::Null, fds).await.unwrap();
    }
    let list = publisher.request("bus", "list", Value::Null).await.unwrap();
    assert_eq!(list["connections"], json!(1), "the stuck subscriber is gone: {list}");
    drop(raw);
}

/// Session's connection hands the broker connections for apps, which are stamped
/// with their app id and can't act as services.
#[tokio::test]
async fn session_hands_over_app_connections() {
    let dir = TempDir::new("adopt");
    let path = dir.0.join("bus.sock");
    let (session_end, broker_end) = tokio::net::UnixStream::pair().unwrap();
    let listener = UnixListener::bind(&path).unwrap();
    tokio::spawn(busd::serve(listener, Box::new(busd::AllowAll), Some(broker_end)));
    let session = Client::from_stream(session_end);

    let (app_end, for_broker) = std::os::unix::net::UnixStream::pair().unwrap();
    let reply = session
        .request_with("bus", "adopt", json!({"app": "org.example.notes"}), vec![for_broker.into()], None)
        .await
        .unwrap();
    let conn = reply.result["conn"].as_u64().unwrap();
    app_end.set_nonblocking(true).unwrap();
    let app = Client::from_stream(tokio::net::UnixStream::from_std(app_end).unwrap());
    let (seen, sender) = app.hello().await.unwrap();
    assert_eq!(seen, conn);
    assert_eq!(sender, Sender::App { app: "org.example.notes".into(), conn });
    assert_eq!(app.claim("db").await.unwrap_err().code(), code::PERMISSION_DENIED);
    let Err(err) = app.subscribe(TAP_TOPIC).await else { panic!("an app tapped the bus") };
    assert_eq!(err.code(), code::PERMISSION_DENIED);

    // A system service's connection, handed over the same way, may claim names.
    let (svc_end, for_broker) = std::os::unix::net::UnixStream::pair().unwrap();
    session
        .request_with("bus", "adopt", json!({"trust": "system"}), vec![for_broker.into()], None)
        .await
        .unwrap();
    svc_end.set_nonblocking(true).unwrap();
    let svc = Client::from_stream(tokio::net::UnixStream::from_std(svc_end).unwrap());
    svc.claim("db").await.unwrap();

    // Anyone else asking to adopt is refused, and a non-socket is refused too.
    let outsider = Client::connect(&path).await.unwrap();
    let (_keep, for_broker) = std::os::unix::net::UnixStream::pair().unwrap();
    let err = outsider
        .request_with("bus", "adopt", json!({"trust": "system"}), vec![for_broker.into()], None)
        .await
        .unwrap_err();
    assert_eq!(err.code(), code::PERMISSION_DENIED);
    let err = session
        .request_with("bus", "adopt", json!({"trust": "system"}), vec![file_with("x")], None)
        .await
        .unwrap_err();
    assert_eq!(err.code(), code::BAD_REQUEST);
}

#[tokio::test]
async fn requests_after_the_broker_stops_fail_cleanly() {
    let dir = TempDir::new("stopped");
    let path = dir.0.join("bus.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server = tokio::spawn(busd::serve(listener, Box::new(busd::AllowAll), None));
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

/// A request with the largest possible `timeout_ms` leaves the broker's clock
/// (the router's only deadline, which the shell sleeps until) working.
#[tokio::test]
async fn a_huge_timeout_leaves_the_broker_working() {
    use tokio::io::AsyncWriteExt;
    let bus = Bus::start("huge-timeout");
    let _svc = echo_service(&bus, "svc").await;
    let mut raw = tokio::net::UnixStream::connect(&bus.path).await.unwrap();
    let mut hang = protocol::Message::request(1, "svc", "hang", Value::Null);
    if let protocol::Message::Request { timeout_ms, .. } = &mut hang {
        *timeout_ms = Some(u64::MAX);
    }
    raw.write_all(&protocol::bus::encode_frame(&hang).unwrap()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let client = bus.client().await;
    let short = client.request_with("svc", "hang", Value::Null, Vec::new(), Some(Duration::from_millis(100)));
    let err = tokio::time::timeout(Duration::from_secs(5), short).await.expect("the broker still keeps time");
    assert!(matches!(&err, Err(CallError::Bus(e)) if e.code == code::TIMEOUT), "{err:?}");
    assert_eq!(client.request("svc", "echo", json!(3)).await.unwrap(), json!(3));
}
