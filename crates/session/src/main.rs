//! `session`: the first process BusyBox init starts.
//!
//! Phase 0 scope: read `/etc/idealos/session.conf`, start each service, and restart
//! any that exit, with exponential backoff. Later milestones add the bus socket
//! hand-off (M2) and per-app sandboxes (M6).
//!
//! Config format, one service per line:
//!
//! ```text
//! # name       restart   command...
//! shell        always    /bin/sh -l
//! compositor   always    /usr/bin/compositor --backend kms
//! ```
//!
//! On SIGTERM it stops every service (SIGTERM, then SIGKILL after a grace period)
//! and exits, so init can respawn a fresh session without leaving old services
//! running. `tools/deploy-to-pi` relies on this.

use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const DEFAULT_CONFIG: &str = "/etc/idealos/session.conf";
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A service that stays up this long is considered healthy again.
const HEALTHY_AFTER: Duration = Duration::from_secs(60);
/// How long services get to exit after SIGTERM before they are killed.
/// Interactive shells ignore SIGTERM, so the shell always takes this long.
const STOP_GRACE: Duration = Duration::from_secs(2);

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigterm(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

fn install_sigterm_handler() {
    // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
    unsafe { libc::signal(libc::SIGTERM, on_sigterm as *const () as libc::sighandler_t) };
}

#[derive(Debug, Clone, PartialEq)]
enum Restart {
    Always,
    Never,
}

#[derive(Debug, Clone, PartialEq)]
struct ServiceSpec {
    name: String,
    restart: Restart,
    argv: Vec<String>,
}

struct Service {
    spec: ServiceSpec,
    child: Option<Child>,
    /// Whether it has been started at least once (`never` services start once).
    launched: bool,
    started: Instant,
    backoff: Duration,
    next_start: Instant,
}

fn parse_config(text: &str) -> Result<Vec<ServiceSpec>, String> {
    let mut specs = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut words = line.split_whitespace();
        let (Some(name), Some(restart)) = (words.next(), words.next()) else {
            return Err(format!("line {}: expected `name restart command...`", n + 1));
        };
        let restart = match restart {
            "always" => Restart::Always,
            "never" => Restart::Never,
            other => return Err(format!("line {}: restart must be always or never, not {other}", n + 1)),
        };
        let argv: Vec<String> = words.map(str::to_owned).collect();
        if argv.is_empty() {
            return Err(format!("line {}: missing command", n + 1));
        }
        if specs.iter().any(|s: &ServiceSpec| s.name == name) {
            return Err(format!("line {}: duplicate service {name}", n + 1));
        }
        specs.push(ServiceSpec { name: name.to_owned(), restart, argv });
    }
    Ok(specs)
}

/// Next backoff after a crash: reset if the service had been healthy, else double.
fn next_backoff(current: Duration, ran_for: Duration) -> Duration {
    if ran_for >= HEALTHY_AFTER { MIN_BACKOFF } else { (current * 2).clamp(MIN_BACKOFF, MAX_BACKOFF) }
}

impl Service {
    fn start(&mut self) {
        let mut cmd = Command::new(&self.spec.argv[0]);
        cmd.args(&self.spec.argv[1..]);
        match cmd.spawn() {
            Ok(child) => {
                eprintln!("session: started {} (pid {})", self.spec.name, child.id());
                self.child = Some(child);
                self.started = Instant::now();
            }
            Err(e) => {
                eprintln!("session: cannot start {}: {e}", self.spec.name);
                self.schedule_restart(Duration::ZERO);
            }
        }
    }

    fn schedule_restart(&mut self, ran_for: Duration) {
        self.backoff = next_backoff(self.backoff, ran_for);
        self.next_start = Instant::now() + self.backoff;
    }

    fn running(&mut self) -> bool {
        self.child.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None)))
    }
}

/// Ask every running service to exit, wait up to `STOP_GRACE`, then kill the rest.
fn stop_all(services: &mut [Service]) {
    for svc in services.iter_mut() {
        if let Some(child) = &svc.child {
            // SAFETY: plain kill(2) on a pid we spawned and have not yet reaped.
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        }
    }
    let deadline = Instant::now() + STOP_GRACE;
    while Instant::now() < deadline && services.iter_mut().any(Service::running) {
        std::thread::sleep(Duration::from_millis(50));
    }
    for svc in services.iter_mut() {
        if let Some(mut child) = svc.child.take() {
            if matches!(child.try_wait(), Ok(None)) {
                eprintln!("session: killing {}", svc.spec.name);
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| DEFAULT_CONFIG.to_owned());
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        eprintln!("session: cannot read {path}: {e}; falling back to a shell");
        "shell always /bin/sh -l\n".to_owned()
    });
    let specs = parse_config(&text).unwrap_or_else(|e| {
        eprintln!("session: {path}: {e}");
        std::process::exit(1);
    });

    install_sigterm_handler();
    let now = Instant::now();
    let mut services: Vec<Service> = specs
        .into_iter()
        .map(|spec| Service {
            spec,
            child: None,
            launched: false,
            started: now,
            backoff: Duration::ZERO,
            next_start: now,
        })
        .collect();

    // A polling loop is enough for a handful of services. M2 replaces it with an
    // event loop (signalfd + the bus socket).
    loop {
        if STOP.load(Ordering::Relaxed) {
            eprintln!("session: SIGTERM, stopping services");
            stop_all(&mut services);
            return;
        }
        for svc in &mut services {
            if let Some(child) = &mut svc.child {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let ran_for = svc.started.elapsed();
                        eprintln!("session: {} exited ({status}) after {ran_for:?}", svc.spec.name);
                        svc.child = None;
                        if svc.spec.restart == Restart::Always {
                            svc.schedule_restart(ran_for);
                        }
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("session: wait {}: {e}", svc.spec.name),
                }
            } else if Instant::now() >= svc.next_start
                && (svc.spec.restart == Restart::Always || !svc.launched)
            {
                svc.start();
                svc.launched = true;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_services_and_ignores_comments() {
        let specs = parse_config(
            "# comment\n\nshell always /bin/sh -l\ncompositor never /usr/bin/compositor --backend kms # trailing\n",
        )
        .unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].argv, ["/bin/sh", "-l"]);
        assert_eq!(specs[1].restart, Restart::Never);
        assert_eq!(specs[1].argv.last().unwrap(), "kms");
    }

    #[test]
    fn rejects_bad_lines() {
        assert!(parse_config("shell").is_err());
        assert!(parse_config("shell sometimes /bin/sh").is_err());
        assert!(parse_config("shell always").is_err());
        assert!(parse_config("a always x\na always y").is_err());
    }

    #[test]
    fn backoff_doubles_caps_and_resets_when_healthy() {
        let short = Duration::from_secs(2);
        let mut b = next_backoff(Duration::ZERO, short);
        assert_eq!(b, MIN_BACKOFF);
        for _ in 0..10 {
            b = next_backoff(b, short);
        }
        assert_eq!(b, MAX_BACKOFF);
        assert_eq!(next_backoff(b, HEALTHY_AFTER), MIN_BACKOFF);
    }
}
