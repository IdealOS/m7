//! app-host's runtime against a real broker: timers, requests, streams, cancels,
//! errors and how a run ends.

use std::path::PathBuf;
use std::time::Duration;

use app_host::runtime::{self, Exit};
use bus_client::{Client, Incoming};
use serde_json::{Value, json};
use tokio::net::UnixListener;

struct Bus {
    dir: PathBuf,
    socket: PathBuf,
}

impl Bus {
    fn start(name: &str) -> Bus {
        let dir = std::env::temp_dir().join(format!("ah-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("bus.sock");
        tokio::spawn(busd::serve(UnixListener::bind(&socket).unwrap(), Box::new(busd::AllowAll), None));
        Bus { dir, socket }
    }

    async fn client(&self) -> Client {
        Client::connect(&self.socket).await.unwrap()
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A service `svc`: `echo` answers its params, `fail` an error, `count` streams
/// 0..n then ends, `forever` streams until cancelled and then reports it on
/// `cancelled` requests. Every request is also published to `seen`.
async fn service(bus: &Bus) {
    let svc = bus.client().await;
    svc.claim("svc").await.unwrap();
    let mut requests = svc.take_requests().unwrap();
    tokio::spawn(async move {
        let mut cancels = 0;
        while let Some(incoming) = requests.next().await {
            match incoming {
                Incoming::Request { id, method, params, .. } => match method.as_str() {
                    "echo" => svc.respond(id, Ok(params)).await.unwrap(),
                    "fail" => {
                        let e = protocol::BusError { code: "nope".into(), message: "no".into() };
                        svc.respond(id, Err(e)).await.unwrap();
                    }
                    "count" => {
                        for n in 0..params.as_u64().unwrap() {
                            svc.stream(id, json!(n)).await.unwrap();
                        }
                        svc.respond(id, Ok(Value::Null)).await.unwrap();
                    }
                    "forever" => svc.stream(id, json!("tick")).await.unwrap(),
                    "cancels" => svc.respond(id, Ok(json!(cancels))).await.unwrap(),
                    _ => {}
                },
                Incoming::Cancel { .. } => cancels += 1,
            }
        }
    });
}

async fn run(bus: &Bus, source: &str) -> Exit {
    let client = bus.client().await;
    let local = tokio::task::LocalSet::new();
    let source = source.to_owned();
    tokio::time::timeout(Duration::from_secs(10), local.run_until(runtime::run(client, "test.js", &source)))
        .await
        .expect("finished in time")
        .unwrap()
}

/// Scripts report by exiting with a code: 0 when every check held, else which one failed.
#[tokio::test]
async fn timers_requests_and_streams() {
    let bus = Bus::start("basics");
    service(&bus).await;
    let script = r#"
        const fail = (n) => __idealos.exit(n);
        const order = [];
        setTimeout(() => order.push("b"), 20);
        setTimeout(() => order.push("a"), 5);
        const cancelled = setTimeout(() => fail(1), 10);
        clearTimeout(cancelled);
        let ticks = 0;
        const iv = setInterval(() => { if (++ticks === 3) clearInterval(iv); }, 1);
        await new Promise((r) => setTimeout(r, 40));
        if (order.join("") !== "ab") fail(2);
        if (ticks !== 3) fail(3);

        const echoed = await __idealos.request("svc", "echo", {x: [1, "two"]});
        if (JSON.stringify(echoed) !== '{"x":[1,"two"]}') fail(4);
        try { await __idealos.request("svc", "fail", null); fail(5); }
        catch (e) { if (e.code !== "nope" || e.message !== "no") fail(6); }
        try { await __idealos.request("nobody", "x", null); fail(7); }
        catch (e) { if (e.code !== "no_such_service") fail(8); }

        const parts = [];
        await new Promise((resolve) => __idealos.stream("svc", "count", 3, (p) => parts.push(p), (err) => err ? fail(9) : resolve()));
        if (parts.join() !== "0,1,2") fail(10);

        // Cancelling a stream tells the service.
        let cancel;
        await new Promise((resolve) => { cancel = __idealos.stream("svc", "forever", null, resolve, () => fail(11)); });
        cancel();
        cancel(); // harmless
        await new Promise((r) => setTimeout(r, 30));
        if (await __idealos.request("svc", "cancels", null) !== 1) fail(12);
        console.log("all good", {n: 1});
        __idealos.exit(0);
    "#;
    assert_eq!(run(&bus, script).await, Exit::Code(0));
}

#[tokio::test]
async fn a_run_ends_when_nothing_is_left_to_wait_for() {
    let bus = Bus::start("idle");
    assert_eq!(run(&bus, "setTimeout(() => {}, 10);").await, Exit::Idle);
}

#[tokio::test]
async fn load_failures_and_uncaught_errors() {
    let bus = Bus::start("errors");
    let Exit::Failed(why) = run(&bus, "throw new Error('broken at load');").await else { panic!() };
    assert!(why.contains("broken at load"), "{why}");
    let Exit::Failed(why) = run(&bus, "await Promise.reject(new Error('async load'));").await else {
        panic!()
    };
    assert!(why.contains("async load"), "{why}");
    let Exit::Failed(why) = run(&bus, "this is not javascript").await else { panic!() };
    assert!(why.to_lowercase().contains("syntax"), "{why}");
    // An error in a callback is logged, and the app carries on.
    let script =
        "setTimeout(() => { throw new Error('in a timer'); }, 1); setTimeout(() => __idealos.exit(7), 20);";
    assert_eq!(run(&bus, script).await, Exit::Code(7));
}
