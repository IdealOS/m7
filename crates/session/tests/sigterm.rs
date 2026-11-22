//! SIGTERM must stop session's services, not orphan them. Before this was handled,
//! killing session (as `tools/deploy-to-pi` does) left the old shell running on the
//! console next to the one the respawned session started.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the pid exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn wait_for_file(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && text.ends_with('\n')
        {
            return text;
        }
        assert!(Instant::now() < deadline, "service never wrote {}", path.display());
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Start session with one service that runs `script` after writing its pid, send
/// session SIGTERM, and return (the service's pid, how long session took to exit).
fn stop_session_running(name: &str, script: &str) -> (i32, Duration) {
    let dir = std::env::temp_dir().join(format!("session-test-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let pidfile = dir.join("pid");
    let _ = std::fs::remove_file(&pidfile);
    // session splits commands on whitespace, so put the script in a file.
    let script_path = dir.join("echo_pid_then_run");
    std::fs::write(&script_path, format!("#!/bin/sh\necho $$ > {}\n{script}\n", pidfile.display())).unwrap();
    let conf = dir.join("session.conf");
    std::fs::write(&conf, format!("svc always /bin/sh {}\n", script_path.display())).unwrap();

    let mut session =
        Command::new(env!("CARGO_BIN_EXE_session")).arg(&conf).stderr(Stdio::null()).spawn().unwrap();
    let pid: i32 = wait_for_file(&pidfile).trim().parse().unwrap();
    assert!(alive(pid));

    let sent = Instant::now();
    // SAFETY: plain kill(2) on the child we just spawned.
    unsafe { libc::kill(session.id() as i32, libc::SIGTERM) };
    let status = session.wait().unwrap();
    let took = sent.elapsed();
    assert!(status.success(), "session exited with {status}");
    std::fs::remove_dir_all(&dir).unwrap();
    (pid, took)
}

#[test]
fn sigterm_stops_services() {
    let (pid, took) = stop_session_running("plain", "exec sleep 100");
    assert!(!alive(pid), "service {pid} outlived session");
    assert!(took < Duration::from_secs(1), "a service that obeys SIGTERM took {took:?}");
}

#[test]
fn sigterm_kills_services_that_ignore_it() {
    // Like an interactive shell, which ignores SIGTERM.
    let (pid, took) = stop_session_running("stubborn", "trap '' TERM\nexec sleep 100");
    assert!(!alive(pid), "service {pid} outlived session");
    assert!(took >= Duration::from_secs(2), "killed before the grace period: {took:?}");
}
