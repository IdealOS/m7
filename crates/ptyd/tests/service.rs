//! ptyd on a real broker, with a real shell on a real pty: an app opens a
//! session, a stand-in compositor attaches as the app and types, and the screen
//! comes back; nobody else may touch it; hanging up ends it.

use std::time::Duration;

use busd::testing::TestBus;
use protocol::pty::{ScreenFrame, SessionEvent};
use serde_json::json;

/// All the text on a screen, kept up to date from frames.
#[derive(Default)]
struct Screen(Vec<String>);

impl Screen {
    fn apply(&mut self, f: &ScreenFrame) {
        self.0.resize(f.rows as usize, String::new());
        for (row, runs) in &f.lines {
            self.0[*row as usize] = runs.iter().map(|r| r.text.as_str()).collect();
        }
    }
    fn has(&self, text: &str) -> bool {
        self.0.iter().any(|l| l.contains(text))
    }
}

#[tokio::test]
async fn a_shell_runs_and_its_screen_streams_to_its_owner() {
    let dir = std::env::temp_dir().join(format!("ptyd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bus = TestBus::start(&dir.join("bus.sock"), Box::new(busd::AllowAll)).await;
    let svc = bus.system().await;
    svc.claim("pty").await.unwrap();
    let config = ptyd::Config { shell: vec!["/bin/sh".into()], dir: dir.clone() };
    tokio::spawn(ptyd::serve(svc, config));

    let app = bus.app("org.example.terminal").await;
    let mut open = app.request_stream("pty", "open", json!({"cols": 40, "rows": 10})).await.unwrap();
    let first: SessionEvent = serde_json::from_value(open.next().await.unwrap().unwrap()).unwrap();
    let SessionEvent::Opened { id } = first else { panic!("{first:?}") };

    // Another app can't see or type into it; the compositor, acting for the owner, can.
    let other = bus.app("org.example.other").await;
    let e = other.request("pty", "write", json!({"id": id, "data": "x"})).await.unwrap_err();
    assert_eq!(e.code(), "permission_denied");
    let compositor = bus.system().await;
    let e = compositor.request_stream("pty", "attach", json!({"id": id})).await;
    let refused = match e {
        Err(e) => e.code() == "permission_denied",
        Ok(mut s) => matches!(s.next().await, Some(Err(e)) if e.code() == "permission_denied"),
    };
    assert!(refused, "without as, the compositor is just another caller");
    let mut screen_frames = compositor
        .request_stream("pty", "attach", json!({"id": id, "as": "org.example.terminal"}))
        .await
        .unwrap();
    let mut screen = Screen::default();
    let snap: ScreenFrame = serde_json::from_value(screen_frames.next().await.unwrap().unwrap()).unwrap();
    assert_eq!((snap.cols, snap.rows), (40, 10));
    screen.apply(&snap);

    compositor
        .request("pty", "write", json!({"id": id, "data": "echo hel''lo; printf '\\033]0;T\\007'\n", "as": "org.example.terminal"}))
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !screen.has("hello") {
        let part = tokio::time::timeout_at(deadline, screen_frames.next())
            .await
            .expect("hello in time")
            .unwrap()
            .unwrap();
        screen.apply(&serde_json::from_value(part).unwrap());
    }
    let title: SessionEvent = serde_json::from_value(open.next().await.unwrap().unwrap()).unwrap();
    assert_eq!(title, SessionEvent::Title { title: "T".into() });

    compositor
        .request("pty", "resize", json!({"id": id, "cols": 20, "rows": 5, "as": "org.example.terminal"}))
        .await
        .unwrap();
    let resized = loop {
        let f: ScreenFrame = serde_json::from_value(screen_frames.next().await.unwrap().unwrap()).unwrap();
        if f.cols == 20 {
            break f;
        }
    };
    assert_eq!(resized.rows, 5);

    // Hanging up (dropping the open stream cancels it) ends the shell and both streams.
    drop(open);
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(Ok(_)) = screen_frames.next().await {}
    })
    .await;
    assert!(ended.is_ok(), "the attach stream ended with the session");
    let e = app.request("pty", "write", json!({"id": id, "data": "x"})).await.unwrap_err();
    assert_eq!(e.code(), "not_found");
    let _ = std::fs::remove_dir_all(&dir);
}
