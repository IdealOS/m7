//! `log PATH`: what services print reaches both the console (session's own stderr)
//! and the file, stamped; a `console` service reaches only the console.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn services_output_goes_to_the_console_and_the_log() {
    let dir = std::env::temp_dir().join(format!("session-test-log-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // A directory that doesn't exist yet: session makes it.
    let log = dir.join("log/session.log");
    let talker = dir.join("talker");
    std::fs::write(&talker, "#!/bin/sh\necho hello from talker\necho oops on stderr >&2\nexec sleep 100\n")
        .unwrap();
    let direct = dir.join("direct");
    std::fs::write(&direct, "#!/bin/sh\necho straight to the console\nexec sleep 100\n").unwrap();
    let conf = dir.join("session.conf");
    std::fs::write(
        &conf,
        format!(
            "log {}\ntalker always /bin/sh {}\ndirect always console /bin/sh {}\n",
            log.display(),
            talker.display(),
            direct.display()
        ),
    )
    .unwrap();

    let mut session =
        Command::new(env!("CARGO_BIN_EXE_session")).arg(&conf).stderr(Stdio::piped()).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !std::fs::read_to_string(&log).is_ok_and(|t| t.contains("oops on stderr\n")) {
        assert!(Instant::now() < deadline, "the log never got the talker's stderr");
        std::thread::sleep(Duration::from_millis(20));
    }
    // Let the console service print too, before stopping.
    std::thread::sleep(Duration::from_millis(200));
    // SAFETY: plain kill(2) on the child we just spawned.
    unsafe { libc::kill(session.id() as i32, libc::SIGTERM) };
    let mut console = String::new();
    session.stderr.take().unwrap().read_to_string(&mut console).unwrap();
    assert!(session.wait().unwrap().success());
    let text = std::fs::read_to_string(&log).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();

    for line in ["hello from talker", "oops on stderr", "session: started talker", "session: started direct"]
    {
        assert!(console.contains(line), "console is missing {line:?}:\n{console}");
        assert!(text.contains(line), "log is missing {line:?}:\n{text}");
    }
    assert!(console.contains("straight to the console"), "console:\n{console}");
    assert!(!text.contains("straight to the console"), "a console service bypasses the log:\n{text}");
    // Written while stopping: Log::finish lets the last lines through.
    assert!(text.contains("session: SIGTERM, stopping services"), "log:\n{text}");
    assert!(!console.contains("] hello"), "the console isn't stamped:\n{console}");
    for line in text.lines() {
        assert!(line.starts_with('[') && line.contains("] "), "unstamped log line {line:?}");
    }
}
