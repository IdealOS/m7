//! The `busd` binary: socket setup and shutdown.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("busd-bin-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn start(socket: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_busd"))
        .args(["--socket".as_ref(), socket.as_os_str()])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// Wait until something answers on `socket`.
fn wait_listening(socket: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::os::unix::net::UnixStream::connect(socket).is_err() {
        assert!(Instant::now() < deadline, "busd never listened on {}", socket.display());
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn sigterm(child: &Child) {
    // SAFETY: plain kill(2) on a child we spawned and haven't reaped.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
}

#[test]
fn creates_a_private_directory_and_cleans_up_on_sigterm() {
    let dir = dir("term");
    let socket = dir.join("nested").join("bus.sock");
    let mut busd = start(&socket);
    wait_listening(&socket);
    let mode = std::fs::metadata(socket.parent().unwrap()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700, "the socket's directory is owner-only");

    sigterm(&busd);
    let status = busd.wait().unwrap();
    assert!(status.success(), "{status}");
    assert!(!socket.exists(), "busd removed its socket");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn replaces_a_stale_socket_but_not_a_live_broker() {
    let dir = dir("stale");
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("bus.sock");
    // A socket file nobody listens on, as a crashed broker leaves behind.
    drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
    assert!(socket.exists());

    let mut first = start(&socket);
    wait_listening(&socket);

    let second = start(&socket).wait_with_output().unwrap();
    assert_eq!(second.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(stderr.contains("already listening"), "{stderr}");
    // The first broker still owns the socket.
    std::os::unix::net::UnixStream::connect(&socket).expect("first broker still answers");

    sigterm(&first);
    first.wait().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}
