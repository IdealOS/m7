//! `browser-host [--socket PATH]`: the `web` service. It runs Servo, keeps the
//! pages apps open, and draws each into a surface the compositor shows
//! ([`protocol::web`], [`protocol::surface`]).
//!
//! Servo's objects live on one thread, so that thread is Servo's: it waits for a
//! wake-up (Servo has work, or a request came in), takes the requests, lets Servo
//! run, and sends out what changed: page events to the app, frames to the
//! compositor. The bus has its own thread with a small tokio runtime, and the two
//! talk over channels.

mod gl;
mod host;
mod shm;
mod surface;

use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

use bus_client::{Client, Incoming};
use protocol::bus::code;
use protocol::{BusError, Sender};
use serde_json::Value;

/// A request for the Servo thread, from the bus thread.
pub enum Cmd {
    /// Request `id` from `caller` (`app:ID`, or another sender), its `as` taken out.
    Request { id: u64, caller: String, method: String, params: Value },
    /// Request `id` was given up on (or its requester went away).
    Cancel { id: u64 },
    /// The bus connection closed.
    Closed,
}

/// Something for the bus thread to send.
pub enum Out {
    Part { id: u64, data: Value, fds: Vec<OwnedFd> },
    Respond { id: u64, result: Result<Value, BusError> },
}

/// Wakes the Servo thread, from Servo's threads or the bus thread.
#[derive(Clone, Default)]
pub struct Waker(Arc<(Mutex<bool>, Condvar)>);

impl Waker {
    fn wait(&self, timeout: Duration) {
        let (lock, cv) = &*self.0;
        let mut woken = lock.lock().unwrap();
        if !*woken {
            woken = cv.wait_timeout(woken, timeout).unwrap().0;
        }
        *woken = false;
    }

    fn wake(&self) {
        let (lock, cv) = &*self.0;
        *lock.lock().unwrap() = true;
        cv.notify_one();
    }
}

impl servo::EventLoopWaker for Waker {
    fn clone_box(&self) -> Box<dyn servo::EventLoopWaker> {
        Box::new(self.clone())
    }
    fn wake(&self) {
        Waker::wake(self);
    }
}

pub fn error(code: &str, message: impl Into<String>) -> BusError {
    BusError { code: code.into(), message: message.into() }
}

/// Who a request is from, for owning pages: `app:ID`, `service:NAME` or
/// `conn:N`. A system service acting for an app (the compositor) says so with
/// `"as"`, an app's id or `conn:N` (an app run with `app-host --bundle` in
/// development is a tool's connection); apps may not.
pub fn caller(from: Option<&Sender>, params: &mut Value) -> Result<String, BusError> {
    let on_behalf = params.as_object_mut().and_then(|p| p.remove("as"));
    match (from, on_behalf) {
        (Some(Sender::App { app, .. }), None) => Ok(format!("app:{app}")),
        (Some(Sender::App { .. }), Some(_)) => {
            Err(error(code::PERMISSION_DENIED, "apps can't act for other apps"))
        }
        (_, Some(Value::String(app))) if protocol::bus::valid_app_id(&app) => Ok(format!("app:{app}")),
        (_, Some(Value::String(conn)))
            if conn.strip_prefix("conn:").is_some_and(|n| n.parse::<u64>().is_ok()) =>
        {
            Ok(conn)
        }
        (_, Some(_)) => Err(error(code::BAD_REQUEST, "`as` takes an app id")),
        (Some(Sender::Service(name)), None) => Ok(format!("service:{name}")),
        (Some(Sender::Conn(n)), None) => Ok(format!("conn:{n}")),
        (None, None) => Err(error(code::BAD_REQUEST, "requests need a sender")),
    }
}

fn main() {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info,servo=warn,script=error"),
    )
    .init();
    let mut socket: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => socket = args.next().map(PathBuf::from),
            other => {
                eprintln!("browser-host: unknown argument {other}\nusage: browser-host [--socket PATH]");
                std::process::exit(2);
            }
        }
    }
    if rustls::crypto::aws_lc_rs::default_provider().install_default().is_err() {
        log::warn!("browser-host: a TLS provider was already installed");
    }

    let waker = Waker::default();
    let (cmds_tx, cmds) = mpsc::channel::<Cmd>();
    let (out, out_rx) = tokio::sync::mpsc::unbounded_channel::<Out>();
    let (ready_tx, ready) = mpsc::channel::<Result<(), String>>();
    let bus_waker = waker.clone();
    std::thread::Builder::new()
        .name("bus".into())
        .spawn(move || run_bus(socket, cmds_tx, out_rx, bus_waker, ready_tx))
        .expect("spawn the bus thread");
    match ready.recv() {
        Ok(Ok(())) => log::info!("browser-host: serving {}", protocol::web::SERVICE),
        Ok(Err(e)) => {
            log::error!("browser-host: {e}");
            std::process::exit(1);
        }
        Err(_) => std::process::exit(1),
    }

    let mut host = host::Host::new(waker.clone(), out);
    loop {
        waker.wait(Duration::from_millis(250));
        while let Ok(cmd) = cmds.try_recv() {
            match cmd {
                Cmd::Request { id, caller, method, params } => host.request(id, &caller, &method, params),
                Cmd::Cancel { id } => host.cancel(id),
                Cmd::Closed => {
                    log::error!("browser-host: the bus closed the connection");
                    std::process::exit(1);
                }
            }
        }
        host.run();
    }
}

/// The bus thread: claim `web`, pass requests to the Servo thread, send its answers.
fn run_bus(
    socket: Option<PathBuf>,
    cmds: mpsc::Sender<Cmd>,
    mut out: tokio::sync::mpsc::UnboundedReceiver<Out>,
    waker: Waker,
    ready: mpsc::Sender<Result<(), String>>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            let _ = ready.send(Err(e.to_string()));
            return;
        }
    };
    runtime.block_on(async move {
        let connected = match &socket {
            Some(path) => Client::connect(path).await,
            None => Client::connect_default().await,
        };
        let bus = match connected {
            Ok(bus) => bus,
            Err(e) => {
                let _ = ready.send(Err(format!("cannot reach the bus: {e}")));
                return;
            }
        };
        if let Err(e) = bus.claim(protocol::web::SERVICE).await {
            let _ = ready.send(Err(format!("cannot claim {}: {e}", protocol::web::SERVICE)));
            return;
        }
        let Some(mut requests) = bus.take_requests() else { return };
        let _ = ready.send(Ok(()));
        let sender = bus.clone();
        tokio::spawn(async move {
            while let Some(o) = out.recv().await {
                let sent = match o {
                    Out::Part { id, data, fds } => sender.stream_with(id, data, fds).await,
                    Out::Respond { id, result } => sender.respond(id, result).await,
                };
                if let Err(e) = sent {
                    log::warn!("browser-host: couldn't answer: {e}");
                }
            }
        });
        while let Some(incoming) = requests.next().await {
            let cmd = match incoming {
                Incoming::Request { id, from, method, mut params, .. } => {
                    match caller(from.as_ref(), &mut params) {
                        Ok(caller) => Cmd::Request { id, caller, method, params },
                        Err(e) => {
                            let _ = bus.respond(id, Err(e)).await;
                            continue;
                        }
                    }
                }
                Incoming::Cancel { id } => Cmd::Cancel { id },
            };
            if cmds.send(cmd).is_err() {
                return;
            }
            waker.wake();
        }
        let _ = cmds.send(Cmd::Closed);
        waker.wake();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_a_service_may_act_for_an_app() {
        let app = Sender::App { app: "org.idealos.web".into(), conn: 4 };
        assert_eq!(caller(Some(&app), &mut json!({"id": "1"})).unwrap(), "app:org.idealos.web");
        assert!(caller(Some(&app), &mut json!({"as": "org.other"})).is_err());
        let compositor = Sender::Service("compositor".into());
        let mut params = json!({"id": "1", "as": "org.idealos.web"});
        assert_eq!(caller(Some(&compositor), &mut params).unwrap(), "app:org.idealos.web");
        assert_eq!(params, json!({"id": "1"}), "`as` is taken out");
        assert!(caller(Some(&compositor), &mut json!({"as": "not an id!"})).is_err());
        assert_eq!(caller(Some(&compositor), &mut json!({"as": "conn:12"})).unwrap(), "conn:12");
        assert!(caller(Some(&compositor), &mut json!({"as": "conn:x"})).is_err());
        assert!(caller(Some(&app), &mut json!({"as": "conn:12"})).is_err(), "not apps, though");
    }
}
