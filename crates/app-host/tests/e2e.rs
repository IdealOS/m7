//! The whole M4 path with real parts and no display: a broker, the db service, the
//! compositor's bus bridge driving a desktop, and app-host running an app. The test
//! clicks the app's button through the desktop's input path, the app writes a Note
//! and updates its label, and the test reads the label back from the scene.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bus_client::Client;
use compositor::bus::{self, Bridge};
use compositor::desktop::Desktop;
use compositor::input::Input;
use tokio::net::UnixListener;

/// The app, written against app-host's raw `__idealos` API (the SDK, which wraps
/// it, has its own tests): a label counting Notes and a button that adds one. It
/// closes its window after two clicks, which ends it.
const APP: &str = r#"
let windowId;
let clicks = 0;
const commit = (ops) => __idealos.request("compositor", "commit", { window: windowId, ops });
const cancel = __idealos.stream("compositor", "open_window", { title: "E2E", width: 300, height: 160 }, async (ev) => {
  if (ev.kind === "opened") {
    windowId = ev.window;
    await commit([
      { op: "create", id: 2, type: "vstack", props: { padding: 10, gap: 8 } },
      { op: "create", id: 3, type: "label", props: { text: "Notes: 0" } },
      { op: "create", id: 4, type: "button", props: { label: "Add" } },
      { op: "insert", parent: 1, child: 2, index: 0 },
      { op: "insert", parent: 2, child: 3, index: 0 },
      { op: "insert", parent: 2, child: 4, index: 1 },
      { op: "listen", id: 4, events: ["click"] },
    ]);
  } else if (ev.kind === "event" && ev.node === 4) {
    clicks += 1;
    await __idealos.request("db", "create", { type: "Note", data: { n: clicks } });
    const { docs } = await __idealos.request("db", "query", { type: "Note" });
    await commit([{ op: "set", id: 3, props: { text: `Notes: ${docs.length}` } }]);
  } else if (ev.kind === "close") {
    cancel();
  }
}, (err) => err && console.error(err.message));
"#;

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

type Shared = Arc<Mutex<(Desktop, Bridge)>>;

/// Whether some text item on screen says exactly `text`.
fn shows(shared: &Shared, text: &str) -> bool {
    // As a backend draws: laid out first.
    let mut s = shared.lock().unwrap();
    s.0.draw_list().layers.iter().flat_map(|l| &l.texts).any(|t| t.text == text)
}

/// Click the middle of the button labelled `text`, as a pointer would.
fn click(shared: &Shared, text: &str) {
    let mut s = shared.lock().unwrap();
    let (desktop, bridge) = &mut *s;
    let list = desktop.draw_list();
    let t =
        list.layers.iter().flat_map(|l| &l.texts).find(|t| t.text == text).expect("the button is on screen");
    let (x, y) = (t.rect.x + t.rect.w / 2.0, t.rect.y + t.rect.h / 2.0);
    for input in [Input::MotionTo { x, y }, Input::Button { pressed: true }, Input::Button { pressed: false }]
    {
        let (_, outs) = desktop.input(input);
        bridge.send(outs);
    }
}

async fn until(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn an_app_window_round_trips_through_the_compositor_and_the_database() {
    let dir = Dir(std::env::temp_dir().join(format!("ah-e2e-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&dir.0);
    std::fs::create_dir_all(&dir.0).unwrap();
    let socket = dir.0.join("bus.sock");
    tokio::spawn(busd::serve(UnixListener::bind(&socket).unwrap(), Box::new(busd::AllowAll), None));

    let db_path = dir.0.join("test.db");
    let service = Client::connect(&socket).await.unwrap();
    service.claim(protocol::db::SERVICE).await.unwrap();
    let store = db::Store::open(&db_path).unwrap();
    tokio::spawn(dbd::serve(service, store, dbd::Config::beside(&db_path)));

    // The compositor: its real bus bridge, pumped into a headless desktop.
    let bridge = tokio::task::spawn_blocking({
        let socket = socket.clone();
        move || bus::connect_with(Some(socket), || {})
    })
    .await
    .unwrap()
    .expect("the compositor reached the bus");
    let shared: Shared = Arc::new(Mutex::new((Desktop::new(1280.0, 800.0), bridge)));
    let pump = shared.clone();
    tokio::spawn(async move {
        loop {
            {
                let mut s = pump.lock().unwrap();
                let (desktop, bridge) = &mut *s;
                let calls: Vec<_> = bridge.calls().collect();
                for call in calls {
                    let outs = desktop.call(call);
                    bridge.send(outs);
                }
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    });

    // The app, in app-host's runtime on its own thread.
    let app_socket = socket.clone();
    let app = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async move {
            let client = Client::connect(&app_socket).await.unwrap();
            app_host::runtime::run(client, "e2e.js", APP).await
        })
    });

    until("the app's window", || shows(&shared, "Notes: 0")).await;
    assert_eq!(shared.lock().unwrap().0.apps.len(), 1);
    assert!(shows(&shared, "E2E"), "the compositor drew the title");

    click(&shared, "Add");
    until("the first note", || shows(&shared, "Notes: 1")).await;
    click(&shared, "Add");
    until("the second note", || shows(&shared, "Notes: 2")).await;

    // The close button: the app closes its window and, with nothing left to do, ends.
    click(&shared, "×");
    until("the window to go", || shared.lock().unwrap().0.apps.is_empty()).await;
    assert_eq!(app.join().unwrap().unwrap(), app_host::runtime::Exit::Idle);
    let check = Client::connect(&socket).await.unwrap();
    let notes = check.request("db", "query", serde_json::json!({"type": "Note"})).await.unwrap();
    assert_eq!(notes["docs"].as_array().unwrap().len(), 2);
}
