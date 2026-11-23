//! `session`: the first process BusyBox init starts.
//!
//! It reads `/etc/idealos/session.conf`, starts each service, and restarts any that
//! exit, with exponential backoff. It also makes services' bus connections (see
//! `bus.rs`); per-app sandboxes come in M6.
//!
//! Config format, one service per line, with an optional `bus=` setting:
//!
//! ```text
//! # name       restart   [bus=ROLE]          command...
//! bus          always    bus=broker          /usr/bin/busd
//! shell        always                        /bin/sh -l
//! compositor   always    bus=system          /usr/bin/compositor --backend kms
//! notes        always    bus=app:org.x.notes /usr/bin/app-host notes
//! ```
//!
//! `bus=broker` marks the broker, which gets session's own connection and is
//! started before the services that need it. `bus=system` and `bus=app:ID` services
//! get a connection session made and handed to the broker, with that trust. If the
//! broker exits, those services are stopped too (their connections died with it)
//! and restart as their `restart` setting says.
//!
//! On SIGTERM it stops every service (SIGTERM, then SIGKILL after a grace period)
//! and exits, so init can respawn a fresh session without leaving old services
//! running. `tools/deploy-to-pi` relies on this.

/// Log one line to stderr as `session: ...`, in a single write. Stderr is
/// unbuffered, so `eprintln!` with arguments can take several writes, and on the
/// shared serial console a service's own output then lands in the middle of the
/// line ("session: started " + a shell's warning + "shell (pid 245)"), which
/// tools/qemu-smoke.ts can't parse.
macro_rules! say {
    ($($arg:tt)*) => {{
        let line = format!("session: {}\n", format_args!($($arg)*));
        let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
    }};
}

mod bus;

use std::os::unix::process::CommandExt;
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
    bus: bus::Role,
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
        let mut words = words.peekable();
        let role = match words.peek().and_then(|w| w.strip_prefix("bus=")) {
            Some(role) => {
                let role = bus::Role::parse(role).map_err(|e| format!("line {}: {e}", n + 1))?;
                words.next();
                role
            }
            None => bus::Role::None,
        };
        let argv: Vec<String> = words.map(str::to_owned).collect();
        if argv.is_empty() {
            return Err(format!("line {}: missing command", n + 1));
        }
        if specs.iter().any(|s: &ServiceSpec| s.name == name) {
            return Err(format!("line {}: duplicate service {name}", n + 1));
        }
        if role == bus::Role::Broker && specs.iter().any(|s: &ServiceSpec| s.bus == bus::Role::Broker) {
            return Err(format!("line {}: only one service can be bus=broker", n + 1));
        }
        specs.push(ServiceSpec { name: name.to_owned(), restart, bus: role, argv });
    }
    Ok(specs)
}

/// Next backoff after a crash: reset if the service had been healthy, else double.
fn next_backoff(current: Duration, ran_for: Duration) -> Duration {
    if ran_for >= HEALTHY_AFTER { MIN_BACKOFF } else { (current * 2).clamp(MIN_BACKOFF, MAX_BACKOFF) }
}

impl Service {
    /// Start the service, with its bus connection if it has one. Starting the
    /// broker replaces `link`.
    fn start(&mut self, link: &mut Option<bus::Link>) {
        let mut new_link = None;
        let inherit = match &self.spec.bus {
            bus::Role::None => None,
            bus::Role::Broker => match bus::Link::new() {
                Ok((l, inherit)) => {
                    new_link = Some(l);
                    Some(inherit)
                }
                Err(e) => {
                    say!("cannot make busd's connection: {e}");
                    None
                }
            },
            role => match link.as_mut().map(|l| l.connection_for(&self.spec.name, role)) {
                Some(Ok(inherit)) => Some(inherit),
                Some(Err(e)) => {
                    say!("cannot make {}'s bus connection: {e}", self.spec.name);
                    None
                }
                None => {
                    say!("starting {} without a bus connection: busd isn't running", self.spec.name);
                    None
                }
            },
        };
        let mut cmd = Command::new(&self.spec.argv[0]);
        cmd.args(&self.spec.argv[1..]);
        if let Some(inherit) = &inherit {
            let fd = inherit.raw();
            cmd.env(inherit.env, fd.to_string());
            // SAFETY: the closure runs in the child between fork and exec and only
            // calls fcntl, which is async-signal-safe.
            unsafe {
                cmd.pre_exec(move || {
                    // Descriptors are created close-on-exec; let this one through.
                    if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        // Our copy of the child's end closes when `inherit` drops after the spawn.
        match cmd.spawn() {
            Ok(child) => {
                say!("started {} (pid {})", self.spec.name, child.id());
                self.child = Some(child);
                self.started = Instant::now();
                if new_link.is_some() {
                    *link = new_link;
                }
            }
            Err(e) => {
                say!("cannot start {}: {e}", self.spec.name);
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
                say!("killing {}", svc.spec.name);
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

/// The broker exited, and every connection it held died with it: stop the services
/// that had one, so they restart (with new connections) as their config says.
fn stop_bus_services(services: &mut [Service]) {
    for svc in services.iter_mut().filter(|s| s.spec.bus.adopted()) {
        if let Some(child) = &svc.child {
            say!("stopping {}: its bus connection closed with busd", svc.spec.name);
            // SAFETY: plain kill(2) on a pid we spawned and have not yet reaped.
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        }
    }
}

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| DEFAULT_CONFIG.to_owned());
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        say!("cannot read {path}: {e}; falling back to a shell");
        "shell always /bin/sh -l\n".to_owned()
    });
    let specs = parse_config(&text).unwrap_or_else(|e| {
        say!("{path}: {e}");
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

    // session's connection to the broker, while the broker runs.
    let mut link: Option<bus::Link> = None;
    // The broker comes first, so the services after it can get connections.
    services.sort_by_key(|s| s.spec.bus != bus::Role::Broker);
    let has_broker = services.iter().any(|s| s.spec.bus == bus::Role::Broker);

    // A polling loop is enough for a handful of services.
    loop {
        if STOP.load(Ordering::Relaxed) {
            say!("SIGTERM, stopping services");
            stop_all(&mut services);
            return;
        }
        if link.as_mut().is_some_and(|l| !l.poll()) {
            link = None;
        }
        let mut broker_gone = false;
        for svc in &mut services {
            if let Some(child) = &mut svc.child {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let ran_for = svc.started.elapsed();
                        say!("{} exited ({status}) after {ran_for:?}", svc.spec.name);
                        svc.child = None;
                        if svc.spec.restart == Restart::Always {
                            svc.schedule_restart(ran_for);
                        }
                        if svc.spec.bus == bus::Role::Broker {
                            broker_gone = true;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => say!("wait {}: {e}", svc.spec.name),
                }
            } else if Instant::now() >= svc.next_start
                && (svc.spec.restart == Restart::Always || !svc.launched)
                // A bus service waits while the broker is down, rather than start
                // without the connection it needs.
                && !(svc.spec.bus.adopted() && has_broker && link.is_none())
            {
                svc.start(&mut link);
                svc.launched = true;
            }
        }
        if broker_gone {
            link = None;
            stop_bus_services(&mut services);
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
    fn parses_bus_roles() {
        let specs = parse_config(
            "bus always bus=broker /usr/bin/busd\nnotes always bus=app:org.x.notes app-host notes\nshell always /bin/sh\n",
        )
        .unwrap();
        assert_eq!(specs[0].bus, bus::Role::Broker);
        assert_eq!(specs[0].argv, ["/usr/bin/busd"]);
        assert_eq!(specs[1].bus, bus::Role::App("org.x.notes".into()));
        assert_eq!(specs[1].argv, ["app-host", "notes"]);
        assert_eq!(specs[2].bus, bus::Role::None);
        assert!(parse_config("x always bus=root /bin/x").is_err());
        assert!(parse_config("x always bus=system").is_err(), "a role is not a command");
        assert!(parse_config("a always bus=broker a\nb always bus=broker b").is_err());
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
