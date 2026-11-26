//! `session`: the first process BusyBox init starts.
//!
//! It reads `/etc/idealos/session.conf`, starts each service, and restarts any that
//! exit, with exponential backoff. It also makes services' bus connections (see
//! `bus.rs`), and starts each app in a sandbox (see `sandbox.rs`): its own network
//! namespace unless it holds the `network` grant, Landlock and seccomp.
//! `IDEALOS_NO_SANDBOX=1` turns sandboxes off, for debugging only.
//!
//! Config format, one service per line, with optional `console` and `bus=` settings:
//!
//! ```text
//! log          /data/log/session.log
//! # name       restart   [console] [bus=ROLE]  command...
//! bus          always    bus=broker            /usr/bin/busd
//! shell        always    console               /bin/sh -l
//! compositor   always    bus=system            /usr/bin/compositor --backend kms
//! notes        always    bus=app:org.x.notes   /usr/bin/app-host notes
//! ```
//!
//! `log PATH` keeps everything session and its services print in PATH as well as
//! on the console (see `log.rs`). A `console` service writes to the console
//! directly instead, as an interactive shell must. Without `log`, every service
//! writes to the console directly.
//!
//! `bus=broker` marks the broker, which gets session's own connection and is
//! started before the services that need it. `bus=system` and `bus=app:ID` services
//! get a connection session made and handed to the broker, with that trust. If the
//! broker exits, those services are stopped too (their connections died with it)
//! and restart as their `restart` setting says.
//!
//! Apps aren't in the config: `launch` requests on the bus (from trusted
//! connections, such as `osctl app install` or `app-host --install` at boot) start
//! `app-host --app ID` as a service with an `app:ID` connection, restarted like any
//! `always` service; `stop` ends one and `apps` lists them (see
//! `protocol::session`). `IDEALOS_APP_HOST` overrides the app-host command.
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
mod log;
mod sandbox;

use std::os::unix::process::CommandExt;
use std::process::{Child, Command};

use protocol::bus::{BusError, Sender, code};
use protocol::session::{AppParams, AppsResult, InfoResult, method};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const DEFAULT_CONFIG: &str = "/etc/idealos/session.conf";
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A service that stays up this long is considered healthy again.
const HEALTHY_AFTER: Duration = Duration::from_secs(60);
/// Stop restarting a service that fails this many times in a row without becoming
/// healthy: a missing driver or a bad binary won't fix itself, and the restarts'
/// output would bury the console shell needed to find out why.
const GIVE_UP_AFTER: u32 = 5;
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
    /// Writes to the console itself, not through the log (an interactive shell).
    console: bool,
    bus: bus::Role,
    argv: Vec<String>,
}

#[derive(Debug, PartialEq)]
struct Config {
    /// Where to keep a copy of everything printed, if anywhere.
    log: Option<std::path::PathBuf>,
    services: Vec<ServiceSpec>,
}

struct Service {
    spec: ServiceSpec,
    child: Option<Child>,
    /// Whether it has been started at least once (`never` services start once).
    launched: bool,
    started: Instant,
    backoff: Duration,
    next_start: Instant,
    /// Start again as soon as it exits, without backoff (a relaunched app).
    relaunch: bool,
    /// Forget it once it exits (a stopped app).
    removed: bool,
    /// Failures in a row since it was last healthy.
    failures: u32,
    /// Failed `GIVE_UP_AFTER` times in a row: not restarted until a `launch`
    /// request or a new session.
    given_up: bool,
}

impl Service {
    fn new(spec: ServiceSpec, now: Instant) -> Service {
        Service {
            spec,
            child: None,
            launched: false,
            started: now,
            backoff: Duration::ZERO,
            next_start: now,
            relaunch: false,
            removed: false,
            failures: 0,
            given_up: false,
        }
    }
}

/// The command that runs one app, before `--app ID`.
fn app_host_command() -> Vec<String> {
    if let Ok(cmd) = std::env::var("IDEALOS_APP_HOST") {
        return cmd.split_whitespace().map(str::to_owned).collect();
    }
    // On the device, dev-exec prefers a freshly deployed binary in /opt/idealos.
    let dev_exec = "/usr/libexec/idealos/dev-exec";
    if std::path::Path::new(dev_exec).exists() {
        return vec![dev_exec.into(), "app-host".into()];
    }
    vec!["app-host".into()]
}

/// What a request to `session` should do.
#[derive(Debug, PartialEq)]
enum Action {
    /// Answered already; nothing to start or stop.
    None,
    /// Signal this running service's process to exit.
    Terminate(usize),
}

/// Apply request `method` with `params` from `from` to the services: add an app
/// service, mark one to relaunch or stop. Returns the answer and what to do.
fn apply_request(
    services: &mut Vec<Service>,
    from: Option<&Sender>,
    method: &str,
    params: Value,
    app_host: &[String],
    now: Instant,
) -> (Result<Value, BusError>, Action) {
    let refuse = |code: &str, message: String| (Err(BusError { code: code.into(), message }), Action::None);
    if method == method::INFO {
        let services = services.iter().filter(|s| s.child.is_some()).map(|s| s.spec.name.clone()).collect();
        return (Ok(serde_json::to_value(info(services)).expect("serializes")), Action::None);
    }
    // Apps may not start or stop each other; system services and tools may.
    if !matches!(from, Some(Sender::Service(_) | Sender::Conn(_))) {
        return refuse(code::PERMISSION_DENIED, "only system services and tools may manage apps".into());
    }
    let find = |services: &[Service], app: &str| {
        services.iter().position(|s| s.spec.bus == bus::Role::App(app.into()) && !s.removed)
    };
    match method {
        method::APPS => {
            let running = services
                .iter()
                .filter(|s| matches!(s.spec.bus, bus::Role::App(_)) && !s.removed && s.child.is_some())
                .map(|s| s.spec.name.clone())
                .collect();
            (Ok(serde_json::to_value(AppsResult { running }).expect("serializes")), Action::None)
        }
        method::LAUNCH | method::STOP => {
            let Ok(AppParams { app }) = serde_json::from_value::<AppParams>(params) else {
                return refuse(code::BAD_REQUEST, format!("{method} takes {{\"app\": \"<app id>\"}}"));
            };
            if bus::Role::parse(&format!("app:{app}")).is_err() || !valid_app_id(&app) {
                return refuse(code::BAD_REQUEST, format!("{app:?} is not an app id"));
            }
            let found = find(services, &app);
            match (method, found) {
                (method::LAUNCH, Some(i)) => {
                    let svc = &mut services[i];
                    svc.backoff = Duration::ZERO;
                    svc.next_start = now;
                    svc.failures = 0;
                    svc.given_up = false;
                    if svc.child.is_some() {
                        svc.relaunch = true;
                        (Ok(Value::Null), Action::Terminate(i))
                    } else {
                        (Ok(Value::Null), Action::None)
                    }
                }
                (method::LAUNCH, None) => {
                    let mut argv = app_host.to_vec();
                    argv.extend(["--app".into(), app.clone()]);
                    let spec = ServiceSpec {
                        name: app.clone(),
                        restart: Restart::Always,
                        console: false,
                        bus: bus::Role::App(app),
                        argv,
                    };
                    services.push(Service::new(spec, now));
                    (Ok(Value::Null), Action::None)
                }
                (_, Some(i)) => {
                    services[i].removed = true;
                    let running = services[i].child.is_some();
                    (Ok(Value::Null), if running { Action::Terminate(i) } else { Action::None })
                }
                (_, None) => refuse("not_found", format!("app {app} isn't running")),
            }
        }
        other => refuse(code::BAD_REQUEST, format!("session has no method {other:?}")),
    }
}

/// About this device, for `info`.
fn info(services: Vec<String>) -> InfoResult {
    // SAFETY: uname fills in a zeroed struct of NUL-terminated strings.
    let (hostname, kernel, machine) = unsafe {
        let mut u: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut u) == 0 {
            let s = |f: &[libc::c_char]| std::ffi::CStr::from_ptr(f.as_ptr()).to_string_lossy().into_owned();
            (s(&u.nodename), format!("{} {}", s(&u.sysname), s(&u.release)), s(&u.machine))
        } else {
            Default::default()
        }
    };
    // CLOCK_MONOTONIC counts from boot on Linux (not across suspend).
    let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime into a timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) };
    InfoResult {
        version: env!("CARGO_PKG_VERSION").into(),
        hostname,
        kernel,
        machine,
        uptime_secs: t.tv_sec as u64,
        services,
    }
}

/// Reverse-DNS app ids, as the broker checks them.
fn valid_app_id(id: &str) -> bool {
    id.split('.').count() >= 2
        && id.split('.').all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

fn parse_config(text: &str) -> Result<Config, String> {
    let mut specs = Vec::new();
    let mut log = None;
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut words = line.split_whitespace();
        if let Some(rest) = line.strip_prefix("log").filter(|r| r.starts_with(char::is_whitespace)) {
            let path = rest.trim();
            if !path.starts_with('/') || path.contains(char::is_whitespace) {
                return Err(format!("line {}: expected `log /absolute/path`", n + 1));
            }
            if log.replace(std::path::PathBuf::from(path)).is_some() {
                return Err(format!("line {}: only one log", n + 1));
            }
            continue;
        }
        let (Some(name), Some(restart)) = (words.next(), words.next()) else {
            return Err(format!("line {}: expected `name restart command...`", n + 1));
        };
        let restart = match restart {
            "always" => Restart::Always,
            "never" => Restart::Never,
            other => return Err(format!("line {}: restart must be always or never, not {other}", n + 1)),
        };
        let mut words = words.peekable();
        let mut console = false;
        let mut role = bus::Role::None;
        loop {
            match words.peek() {
                Some(&"console") => console = true,
                Some(w) if w.starts_with("bus=") => {
                    role = bus::Role::parse(&w["bus=".len()..]).map_err(|e| format!("line {}: {e}", n + 1))?
                }
                _ => break,
            }
            words.next();
        }
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
        specs.push(ServiceSpec { name: name.to_owned(), restart, console, bus: role, argv });
    }
    Ok(Config { log, services: specs })
}

/// Next backoff after a crash: reset if the service had been healthy, else double.
fn next_backoff(current: Duration, ran_for: Duration) -> Duration {
    if ran_for >= HEALTHY_AFTER { MIN_BACKOFF } else { (current * 2).clamp(MIN_BACKOFF, MAX_BACKOFF) }
}

impl Service {
    /// Start the service, with its bus connection if it has one. Starting the
    /// broker replaces `link`.
    fn start(&mut self, link: &mut Option<bus::Link>, log: Option<&log::Log>) {
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
        // Everything else inherits session's stdout and stderr: the log's pipe.
        if self.spec.console
            && let Some(log) = log
        {
            match (log.console(), log.console()) {
                (Ok(out), Ok(err)) => {
                    cmd.stdout(out).stderr(err);
                }
                (Err(e), _) | (_, Err(e)) => say!("{}: no console of its own: {e}", self.spec.name),
            }
        }
        let sandbox = match &self.spec.bus {
            bus::Role::App(app) if std::env::var_os("IDEALOS_NO_SANDBOX").is_none() => {
                let network = link.as_ref().is_some_and(|l| l.has_network(app));
                let dir = sandbox::runtime_dir(app);
                let spec =
                    sandbox::Spec::for_app(std::path::Path::new(&self.spec.argv[0]), dir.clone(), network);
                match sandbox::Sandbox::prepare(&spec) {
                    Ok(s) => {
                        if !s.limits_files() && cfg!(target_os = "linux") {
                            say!("{}: this kernel has no Landlock, so no file limits", self.spec.name);
                        }
                        cmd.current_dir(&dir)
                            .env("HOME", &dir)
                            .env("TMPDIR", &dir)
                            .env("IDEALOS_APP_DIR", &dir);
                        Some(s)
                    }
                    Err(e) => {
                        say!("cannot make {}'s sandbox: {e}", self.spec.name);
                        self.schedule_restart(Duration::ZERO);
                        return;
                    }
                }
            }
            bus::Role::App(_) => {
                say!("starting {} without a sandbox (IDEALOS_NO_SANDBOX)", self.spec.name);
                None
            }
            _ => None,
        };
        let fd = inherit.as_ref().map(|inherit| {
            cmd.env(inherit.env, inherit.raw().to_string());
            inherit.raw()
        });
        if fd.is_some() || sandbox.is_some() {
            // SAFETY: the closure runs in the child between fork and exec and only
            // makes system calls (fcntl, and the sandbox's), which are
            // async-signal-safe.
            unsafe {
                cmd.pre_exec(move || {
                    // Descriptors are created close-on-exec; let this one through.
                    if let Some(fd) = fd
                        && libc::fcntl(fd, libc::F_SETFD, 0) == -1
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    if let Some(s) = &sandbox {
                        s.enter()?;
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

    /// After a failure (`ran_for` is how long it was up): schedule the next start
    /// with backoff, or give up once it has failed `GIVE_UP_AFTER` times in a row.
    fn schedule_restart(&mut self, ran_for: Duration) {
        self.failures = if ran_for >= HEALTHY_AFTER { 1 } else { self.failures + 1 };
        if self.failures >= GIVE_UP_AFTER {
            self.given_up = true;
            say!(
                "{} failed {} times in a row; not restarting it. To try again: kill $(pidof session)",
                self.spec.name,
                self.failures
            );
            return;
        }
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
    let config = parse_config(&text).unwrap_or_else(|e| {
        say!("{path}: {e}");
        std::process::exit(1);
    });
    let specs = config.services;
    let log = config.log.and_then(|path| match log::Log::start(&path) {
        Ok(log) => {
            say!("logging to {}", path.display());
            Some(log)
        }
        Err(e) => {
            say!("cannot log to {}: {e}; the console only", path.display());
            None
        }
    });

    install_sigterm_handler();
    let now = Instant::now();
    let mut services: Vec<Service> = specs.into_iter().map(|spec| Service::new(spec, now)).collect();
    let app_host = app_host_command();

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
            if let Some(log) = log {
                log.finish();
            }
            return;
        }
        if link.as_mut().is_some_and(|l| !l.poll()) {
            link = None;
        }
        if let Some(l) = link.as_mut() {
            for req in l.take_requests() {
                let (answer, action) = apply_request(
                    &mut services,
                    req.from.as_ref(),
                    &req.method,
                    req.params,
                    &app_host,
                    Instant::now(),
                );
                if let Action::Terminate(i) = action
                    && let Some(child) = &services[i].child
                {
                    // SAFETY: plain kill(2) on a pid we spawned and have not yet reaped.
                    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
                }
                if let Err(e) = &answer {
                    say!("refused {} from {:?}: {}", req.method, req.from, e.message);
                }
                if let Err(e) = l.respond(req.id, answer) {
                    say!("cannot answer {}: {e}", req.method);
                }
            }
        }
        let mut broker_gone = false;
        for svc in &mut services {
            if let Some(child) = &mut svc.child {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let ran_for = svc.started.elapsed();
                        say!("{} exited ({status}) after {ran_for:?}", svc.spec.name);
                        svc.child = None;
                        if svc.relaunch {
                            svc.relaunch = false;
                            svc.next_start = Instant::now();
                        } else if svc.spec.restart == Restart::Always && !svc.removed {
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
                && !svc.removed
                && !svc.given_up
                && (svc.spec.restart == Restart::Always || !svc.launched)
                // A bus service waits while the broker is down, rather than start
                // without the connection it needs.
                && !(svc.spec.bus.adopted() && has_broker && link.is_none())
            {
                svc.start(&mut link, log.as_ref());
                svc.launched = true;
            }
        }
        if broker_gone {
            link = None;
            stop_bus_services(&mut services);
        }
        services.retain(|s| !(s.removed && s.child.is_none()));
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
        .unwrap()
        .services;
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
        .unwrap()
        .services;
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
    fn parses_the_log_and_console_services() {
        let config = parse_config(
            "log /data/log/session.log\nshell always console /bin/sh -l\nui always console bus=system /bin/ui\nbus always bus=broker /usr/bin/busd\n",
        )
        .unwrap();
        assert_eq!(config.log, Some("/data/log/session.log".into()));
        let s = &config.services;
        assert!(s[0].console && s[0].bus == bus::Role::None);
        assert_eq!(s[0].argv, ["/bin/sh", "-l"]);
        assert!(s[1].console && s[1].bus == bus::Role::System);
        assert!(!s[2].console);
        assert_eq!(parse_config("shell always /bin/sh").unwrap().log, None);
        // A service may still be called log-something.
        assert_eq!(parse_config("logger always /bin/logger").unwrap().services[0].name, "logger");
        assert!(parse_config("log data/session.log").is_err(), "relative path");
        assert!(parse_config("log").is_err());
        assert!(parse_config("log /a\nlog /b").is_err());
    }

    #[test]
    fn rejects_bad_lines() {
        assert!(parse_config("shell").is_err());
        assert!(parse_config("shell sometimes /bin/sh").is_err());
        assert!(parse_config("shell always").is_err());
        assert!(parse_config("a always x\na always y").is_err());
    }

    fn tool() -> Option<Sender> {
        Some(Sender::Conn(4))
    }

    #[test]
    fn launch_adds_an_app_service_and_relaunch_and_stop_mark_it() {
        let now = Instant::now();
        let mut services = Vec::new();
        let host = vec!["app-host".to_string()];
        let launch = serde_json::json!({"app": "org.idealos.notes"});
        let (r, a) = apply_request(&mut services, tool().as_ref(), "launch", launch.clone(), &host, now);
        assert_eq!((r, a), (Ok(Value::Null), Action::None));
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].spec.argv, ["app-host", "--app", "org.idealos.notes"]);
        assert_eq!(services[0].spec.bus, bus::Role::App("org.idealos.notes".into()));
        assert_eq!(services[0].spec.restart, Restart::Always);

        // Launching again while it runs restarts it at once.
        services[0].child = Some(Command::new("true").spawn().unwrap());
        let (_, a) = apply_request(&mut services, tool().as_ref(), "launch", launch.clone(), &host, now);
        assert_eq!(a, Action::Terminate(0));
        assert!(services[0].relaunch);
        assert_eq!(services.len(), 1, "no second copy");

        let (r, _) = apply_request(&mut services, tool().as_ref(), "apps", Value::Null, &host, now);
        assert_eq!(r.unwrap(), serde_json::json!({"running": ["org.idealos.notes"]}));

        let (_, a) = apply_request(&mut services, tool().as_ref(), "stop", launch.clone(), &host, now);
        assert_eq!(a, Action::Terminate(0));
        assert!(services[0].removed);
        let (r, _) = apply_request(&mut services, tool().as_ref(), "stop", launch, &host, now);
        assert_eq!(r.unwrap_err().code, "not_found");
        let _ = services[0].child.take().unwrap().wait();
    }

    #[test]
    fn apps_cannot_manage_apps_and_bad_requests_are_refused() {
        let now = Instant::now();
        let mut services = Vec::new();
        let host = vec!["app-host".to_string()];
        let app = Some(Sender::App { app: "org.x.evil".into(), conn: 9 });
        let launch = serde_json::json!({"app": "org.idealos.notes"});
        let (r, _) = apply_request(&mut services, app.as_ref(), "launch", launch, &host, now);
        assert_eq!(r.unwrap_err().code, code::PERMISSION_DENIED);
        let (r, _) = apply_request(&mut services, None, "apps", Value::Null, &host, now);
        assert_eq!(r.unwrap_err().code, code::PERMISSION_DENIED);
        for bad in
            [serde_json::json!({}), serde_json::json!({"app": "notes"}), serde_json::json!({"app": "a b.c"})]
        {
            let (r, _) = apply_request(&mut services, tool().as_ref(), "launch", bad.clone(), &host, now);
            assert_eq!(r.unwrap_err().code, code::BAD_REQUEST, "{bad}");
        }
        let (r, _) = apply_request(&mut services, tool().as_ref(), "reboot", Value::Null, &host, now);
        assert_eq!(r.unwrap_err().code, code::BAD_REQUEST);
        assert!(services.is_empty());
        // Anyone that reaches session may ask what it's running.
        let (r, _) = apply_request(&mut services, app.as_ref(), "info", Value::Null, &host, now);
        let info: InfoResult = serde_json::from_value(r.unwrap()).unwrap();
        assert!(
            !info.kernel.is_empty() && !info.machine.is_empty() && info.version == env!("CARGO_PKG_VERSION")
        );
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

    fn service(name: &str) -> Service {
        let spec = parse_config(&format!("{name} always /bin/false")).unwrap().services.remove(0);
        Service::new(spec, Instant::now())
    }

    #[test]
    fn gives_up_after_repeated_quick_failures() {
        // A compositor with no display device fails at once, every time.
        let mut svc = service("compositor");
        for n in 1..GIVE_UP_AFTER {
            svc.schedule_restart(Duration::from_millis(50));
            assert!(!svc.given_up, "still retrying after {n} failures");
        }
        svc.schedule_restart(Duration::from_millis(50));
        assert!(svc.given_up);
    }

    #[test]
    fn a_healthy_run_resets_the_failure_count() {
        let mut svc = service("compositor");
        for _ in 1..GIVE_UP_AFTER {
            svc.schedule_restart(Duration::from_millis(50));
        }
        // Up for a while, then a crash: the first failure of a new streak.
        svc.schedule_restart(HEALTHY_AFTER);
        assert_eq!(svc.failures, 1);
        assert!(!svc.given_up);
    }

    #[test]
    fn launch_restarts_an_app_session_gave_up_on() {
        let now = Instant::now();
        let mut services = Vec::new();
        let host = vec!["app-host".to_string()];
        let launch = serde_json::json!({"app": "org.idealos.notes"});
        let (r, _) = apply_request(&mut services, tool().as_ref(), "launch", launch.clone(), &host, now);
        assert!(r.is_ok());
        for _ in 0..GIVE_UP_AFTER {
            services[0].schedule_restart(Duration::from_millis(50));
        }
        assert!(services[0].given_up);
        let (r, a) = apply_request(&mut services, tool().as_ref(), "launch", launch, &host, now);
        assert_eq!((r, a), (Ok(Value::Null), Action::None));
        assert!(!services[0].given_up);
        assert_eq!(services[0].failures, 0);
        assert_eq!(services[0].next_start, now);
    }
}
