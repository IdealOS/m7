//! session, busd and osctl as real processes: session starts the broker, hands
//! connections to an app and a system service, and restarts them when the broker
//! dies.
//!
//! Needs the session and busd binaries next to osctl's, which `cargo test
//! --workspace` (and `make test`) builds.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn sibling(name: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_BIN_EXE_osctl")).with_file_name(name);
    assert!(
        path.exists(),
        "{} is missing; run `cargo build --workspace` (or `make test`) first",
        path.display()
    );
    path
}

/// Wait until `path` has at least `lines` lines, and return them.
fn wait_lines(path: &Path, lines: usize) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let got: Vec<String> = text.lines().map(str::to_owned).collect();
        if got.len() >= lines {
            return got;
        }
        assert!(Instant::now() < deadline, "{} has {} of {lines} lines:\n{text}", path.display(), got.len());
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn osctl(socket: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_osctl"))
        .arg("--socket")
        .arg(socket)
        .args(args)
        .env_remove("IDEALOS_BUS_FD")
        .output()
        .unwrap();
    (
        out.status.success(),
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)),
    )
}

struct Session {
    child: Child,
    dir: PathBuf,
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: plain kill(2) on the child we spawned.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn session_gives_services_their_bus_identity() {
    // Short: Unix socket paths are limited to about 100 bytes on macOS.
    let dir = std::env::temp_dir().join(format!("sess-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("bus.sock");
    let bin = env!("CARGO_BIN_EXE_osctl");

    // A handed connection belongs to one client process (when it exits, the
    // connection ends), so each service runs one osctl and then, if it needs to
    // stay up, sleeps. Each records what the broker said, every time it starts.
    let d = dir.display();
    let app = dir.join("app.sh");
    std::fs::write(&app, format!("{bin} bus hello >> {d}/app.out 2>&1\nexec sleep 1000\n")).unwrap();
    let claimer = dir.join("claimer.sh");
    std::fs::write(&claimer, format!("{bin} bus echo app-name >> {d}/claim.out 2>&1\nexec sleep 1000\n"))
        .unwrap();
    let svc = dir.join("svc.sh");
    std::fs::write(&svc, format!("echo started >> {d}/svc.out\nexec {bin} bus echo from-session\n")).unwrap();
    let conf = dir.join("session.conf");
    std::fs::write(
        &conf,
        format!(
            "app     always  bus=app:org.example.notes  /bin/sh {app}\n\
             claimer always  bus=app:org.example.other  /bin/sh {claimer}\n\
             bus     always  bus=broker                 {busd} --socket {socket}\n\
             svc     always  bus=system                 /bin/sh {svc}\n",
            app = app.display(),
            claimer = claimer.display(),
            busd = sibling("busd").display(),
            socket = socket.display(),
            svc = svc.display(),
        ),
    )
    .unwrap();
    let log = std::fs::File::create(dir.join("session.log")).unwrap();
    let child =
        // The "apps" here are shell scripts writing logs beside them, which their
        // sandboxes (on Linux) would rightly refuse; this test is about identity.
        Command::new(sibling("session"))
            .arg(&conf)
            .env("IDEALOS_NO_SANDBOX", "1")
            .stderr(log)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
    let session = Session { child, dir: dir.clone() };

    // The app is stamped with its app id, and can't act as a service.
    let app_hello: serde_json::Value = serde_json::from_str(&wait_lines(&dir.join("app.out"), 1)[0]).unwrap();
    assert_eq!(app_hello["sender"]["app"]["app"], "org.example.notes", "{app_hello}");
    let claim = wait_lines(&dir.join("claim.out"), 1).join("\n");
    assert!(claim.contains("permission_denied"), "{claim}");

    // The system service claimed a name, which anyone on the socket can call.
    let call = || {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (ok, out) = osctl(&socket, &["bus", "call", "from-session", "ping", r#"{"n":1}"#]);
            if ok || Instant::now() > deadline {
                return out;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    assert_eq!(call().trim(), r#"{"n":1}"#);
    let (_, list) = osctl(&socket, &["bus", "list"]);
    assert!(list.contains("session ") && list.contains("from-session"), "{list}");

    // Kill the broker: session restarts it and the services, with new connections.
    let pid =
        Command::new("pgrep").arg("-f").arg(format!("busd --socket {}", socket.display())).output().unwrap();
    let pid: i32 = String::from_utf8_lossy(&pid.stdout).trim().parse().expect("busd's pid");
    // SAFETY: plain kill(2) on the broker this test's session started.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    let lines = wait_lines(&dir.join("app.out"), 2);
    let again: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
    assert_eq!(again["sender"]["app"]["app"], "org.example.notes", "{again}");
    wait_lines(&dir.join("svc.out"), 2);
    assert_eq!(call().trim(), r#"{"n":1}"#, "the system service is back on the new broker");
    let log = std::fs::read_to_string(dir.join("session.log")).unwrap();
    assert!(log.contains("its bus connection closed with busd"), "{log}");
    drop(session);
}
