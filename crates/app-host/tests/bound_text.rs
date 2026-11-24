//! M5's "bound text edits survive a restart": an app binds a text area to a note;
//! typing goes from the compositor straight to dbd; then dbd, the compositor and
//! the app all restart, and the text comes back.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bus_client::Client;
use compositor::bus::{self, Bridge};
use compositor::desktop::Desktop;
use compositor::input::{Input, Key, Mods};
use serde_json::{Value, json};
use tokio::net::UnixListener;

/// Opens a window with a text area bound to note `NOTE_ID`'s `body`, then waits.
const APP: &str = r#"
const note = globalThis.NOTE_ID;
__idealos.stream("compositor", "open_window", { title: "Bound", width: 400, height: 200 }, async (ev) => {
  if (ev.kind !== "opened") return;
  await __idealos.request("compositor", "commit", { window: ev.window, ops: [
    { op: "create", id: 2, type: "vstack", props: { padding: 10 } },
    { op: "create", id: 3, type: "text_area", props: { bind: { doc: note, path: "body" }, h: 120 } },
    { op: "insert", parent: 1, child: 2, index: 0 },
    { op: "insert", parent: 2, child: 3, index: 0 },
  ]});
}, () => {});
"#;

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

type Shared = Arc<Mutex<(Desktop, Bridge)>>;

/// A compositor (its real bus bridge driving a headless desktop) on `socket`.
async fn compositor(socket: &Path) -> (Shared, tokio::task::JoinHandle<()>) {
    let socket = socket.to_path_buf();
    let bridge =
        tokio::task::spawn_blocking(move || bus::connect_with(Some(socket), || {})).await.unwrap().unwrap();
    let mut desktop = Desktop::with_fonts(1280.0, 800.0, compositor::fonts::ui_only());
    desktop.set_screen(1280.0, 800.0);
    let shared: Shared = Arc::new(Mutex::new((desktop, bridge)));
    let pump = shared.clone();
    let task = tokio::spawn(async move {
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
    (shared, task)
}

/// Run the app (bound to `note`) on its own thread until the test ends.
fn app(socket: &Path, note: &str) {
    let socket = socket.to_path_buf();
    let source = format!("globalThis.NOTE_ID = {};\n{APP}", json!(note));
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let local = tokio::task::LocalSet::new();
        let _ = local.block_on(&rt, async move {
            let client = Client::connect(&socket).await.unwrap();
            app_host::runtime::run(client, "bound.js", &source).await
        });
    });
}

/// The text the bound area shows, once it's on screen.
fn shown(shared: &Shared) -> Option<String> {
    let mut s = shared.lock().unwrap();
    let list = s.0.draw_list();
    let area =
        s.0.scene.ids().find(|&id| s.0.scene.node(id).is_some_and(|n| n.props.contains_key("bind")))?;
    let _ = list;
    s.0.scene.node(area)?.props.get("_value").and_then(Value::as_str).map(str::to_owned)
}

async fn until(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn start_db(socket: &Path, db_path: &Path) -> tokio::task::JoinHandle<()> {
    let service = Client::connect(socket).await.unwrap();
    service.claim(protocol::db::SERVICE).await.unwrap();
    let store = db::Store::open(db_path).unwrap();
    let config = dbd::Config::beside(db_path);
    tokio::spawn(async move {
        let _ = dbd::serve(service, store, config).await;
    })
}

#[tokio::test]
async fn bound_text_survives_restarting_everything() {
    let dir = Dir(std::env::temp_dir().join(format!("ah-bound-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&dir.0);
    std::fs::create_dir_all(&dir.0).unwrap();
    let socket = dir.0.join("bus.sock");
    let db_path = dir.0.join("notes.db");
    tokio::spawn(busd::serve(UnixListener::bind(&socket).unwrap(), Box::new(busd::AllowAll), None));
    let db = start_db(&socket, &db_path).await;
    let client = Client::connect(&socket).await.unwrap();
    let note = client.request("db", "create", json!({"type": "Note", "data": {"body": "Hi"}})).await.unwrap();
    let note = note["id"].as_str().unwrap().to_owned();

    // First run: the field loads "Hi"; type " there" at the end.
    let (shared, pump) = compositor(&socket).await;
    app(&socket, &note);
    until("the bound text to load", || shown(&shared).as_deref() == Some("Hi")).await;
    {
        let mut s = shared.lock().unwrap();
        let (desktop, bridge) = &mut *s;
        let area = desktop
            .scene
            .ids()
            .find(|&id| desktop.scene.node(id).is_some_and(|n| n.props.contains_key("bind")))
            .unwrap();
        let (r, _) = desktop.scene.screen_rects()[&area];
        let mut inputs = vec![
            Input::MotionTo { x: r.x + 20.0, y: r.y + 10.0 },
            Input::Button { pressed: true },
            Input::Button { pressed: false },
            Input::Key { key: Key::End, mods: Mods::default() },
        ];
        inputs.extend(" there".chars().map(|c| Input::Key { key: Key::Char(c), mods: Mods::default() }));
        for input in inputs {
            let (_, outs) = desktop.input(input);
            bridge.send(outs);
        }
    }
    until("the edits to reach the database", || {
        let path = db_path.clone();
        let note = note.clone();
        std::thread::spawn(move || db::Store::open(&path).unwrap().get(&note).unwrap().data["body"].clone())
            .join()
            .unwrap()
            == "Hi there"
    })
    .await;

    // Restart everything: the database service (reopening the file), the
    // compositor and the app.
    pump.abort();
    drop(shared);
    db.abort();
    client.release("db").await.ok();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _db = start_db(&socket, &db_path).await;
    let (shared, _pump) = compositor(&socket).await;
    app(&socket, &note);
    until("the text after the restart", || shown(&shared).as_deref() == Some("Hi there")).await;
}
