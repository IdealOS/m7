//! Running an app's bundle: QuickJS, timers and the bus, on one thread.
//!
//! The bundle is an ES module (esbuild's output), evaluated after the prelude that
//! sets up `console`, timers and `__idealos`. The native side, `__host`, takes and
//! gives numbers and JSON strings only: JavaScript keeps its own callback table and
//! is called back through `__dispatch(id, kind, data)`, so Rust never holds a JS
//! value between turns of the loop.
//!
//! The loop runs QuickJS's pending jobs (promise reactions), then waits for the
//! next bus answer or timer, delivers it, and repeats. It ends when the app calls
//! `__idealos.exit`, or when nothing is left to wait for: no timers, requests or
//! streams. An app with an open window has a stream, so it runs until the window
//! closes.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::time::{Duration, Instant};

use bus_client::Client;
use rquickjs::{CatchResultExt, Context, Function, Module, Object, Runtime, promise::PromiseState};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const PRELUDE: &str = include_str!("prelude.js");

/// Most memory an app's QuickJS heap may use.
pub const MEMORY_LIMIT: usize = 256 * 1024 * 1024;

/// Something for JavaScript: `__dispatch(id, kind, data)`.
struct Dispatch {
    id: u64,
    kind: &'static str,
    data: String,
}

/// The native side's state, shared by the `__host` functions and the loop.
struct Host {
    bus: Client,
    events: mpsc::UnboundedSender<Dispatch>,
    /// Due time and id of each pending timer.
    timers: BTreeSet<(Instant, u64)>,
    timer_due: HashMap<u64, Instant>,
    /// Requests and streams in flight, by JavaScript's id.
    tasks: HashMap<u64, JoinHandle<()>>,
    exit: Option<i32>,
}

/// How a run ended.
#[derive(Debug, PartialEq)]
pub enum Exit {
    /// The app called `__idealos.exit(code)`.
    Code(i32),
    /// Nothing left to wait for.
    Idle,
    /// The bundle threw while loading.
    Failed(String),
}

/// Run `source` (an ES module named `name`) against `bus` until it ends. Must be
/// called inside a [`tokio::task::LocalSet`] on a current-thread runtime.
pub async fn run(bus: Client, name: &str, source: &str) -> Result<Exit, String> {
    let js = |e: rquickjs::Error| e.to_string();
    let runtime = Runtime::new().map_err(js)?;
    runtime.set_memory_limit(MEMORY_LIMIT);
    runtime.set_max_stack_size(1024 * 1024);
    runtime.set_host_promise_rejection_tracker(Some(Box::new(|ctx, _promise, reason, handled| {
        if !handled {
            log::error!("unhandled promise rejection: {}", describe(&ctx, reason));
        }
    })));
    let context = Context::full(&runtime).map_err(js)?;
    let (events, mut queue) = mpsc::unbounded_channel();
    let host = Rc::new(RefCell::new(Host {
        bus,
        events,
        timers: BTreeSet::new(),
        timer_due: HashMap::new(),
        tasks: HashMap::new(),
        exit: None,
    }));

    let loaded = context.with(|ctx| -> Result<Result<(), String>, String> {
        install_host(&ctx, &host).map_err(js)?;
        ctx.eval::<(), _>(PRELUDE).catch(&ctx).map_err(|e| format!("prelude: {e}"))?;
        Ok(match Module::evaluate(ctx.clone(), name, source).catch(&ctx) {
            Ok(promise) => {
                ctx.globals().set("__loaded", promise).map_err(js)?;
                Ok(())
            }
            Err(e) => Err(caught(&ctx, e)),
        })
    })?;
    if let Err(e) = loaded {
        return Ok(Exit::Failed(e));
    }

    loop {
        // Promise reactions first, so everything a turn started has happened.
        loop {
            match runtime.execute_pending_job() {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) => {
                    let message = e.0.with(|ctx| {
                        let caught = ctx.catch();
                        describe(&ctx, caught)
                    });
                    log::error!("uncaught exception: {message}");
                }
            }
        }
        // A module whose top level threw (or rejected) failed to load.
        if let Some(failure) = context.with(|ctx| load_failure(&ctx)) {
            return Ok(Exit::Failed(failure));
        }
        if let Some(code) = host.borrow().exit {
            return Ok(Exit::Code(code));
        }
        let next_timer = {
            let h = host.borrow();
            if h.timers.is_empty() && h.tasks.is_empty() && queue.is_empty() {
                return Ok(Exit::Idle);
            }
            h.timers.first().map(|(t, _)| *t)
        };
        let due = async {
            match next_timer {
                Some(t) => tokio::time::sleep_until(t.into()).await,
                None => std::future::pending().await,
            }
        };
        let mut ready = Vec::new();
        tokio::select! {
            event = queue.recv() => ready.extend(event),
            () = due => {}
        }
        // Everything else already waiting, then every timer that's due.
        while let Ok(event) = queue.try_recv() {
            ready.push(event);
        }
        {
            let mut h = host.borrow_mut();
            let now = Instant::now();
            while let Some(&(t, id)) = h.timers.first()
                && t <= now
            {
                h.timers.remove(&(t, id));
                h.timer_due.remove(&id);
                ready.push(Dispatch { id, kind: "timer", data: String::new() });
            }
            for event in &ready {
                if matches!(event.kind, "ok" | "err" | "end") {
                    h.tasks.remove(&event.id);
                }
            }
        }
        context.with(|ctx| {
            let Ok(dispatch) = ctx.globals().get::<_, Function>("__dispatch") else { return };
            for event in ready {
                if let Err(e) = dispatch.call::<_, ()>((event.id, event.kind, event.data)).catch(&ctx) {
                    log::error!("uncaught exception: {e}");
                }
            }
        });
    }
}

/// The error a module failed to load with, once its evaluation promise rejected.
fn load_failure(ctx: &rquickjs::Ctx) -> Option<String> {
    let promise: rquickjs::Promise = ctx.globals().get("__loaded").ok()?;
    match promise.state() {
        PromiseState::Rejected => {
            // Reading the result raises the rejection as the pending exception.
            let _ = promise.result::<Value2>();
            let reason = ctx.catch();
            let _ = ctx.globals().remove("__loaded");
            Some(describe(ctx, reason))
        }
        PromiseState::Resolved => {
            let _ = ctx.globals().remove("__loaded");
            None
        }
        PromiseState::Pending => None,
    }
}

type Value2<'js> = rquickjs::Value<'js>;

/// A caught error as text, with the thrown value described.
fn caught<'js>(ctx: &rquickjs::Ctx<'js>, e: rquickjs::CaughtError<'js>) -> String {
    match e {
        rquickjs::CaughtError::Exception(ex) => describe(ctx, ex.into_value()),
        rquickjs::CaughtError::Value(v) => describe(ctx, v),
        rquickjs::CaughtError::Error(e) => e.to_string(),
    }
}

/// A thrown value as text: an Error's message and stack, or its JSON.
fn describe<'js>(ctx: &rquickjs::Ctx<'js>, v: rquickjs::Value<'js>) -> String {
    if let Some(e) = v.as_exception() {
        let name: String = e.get("name").unwrap_or_else(|_| "Error".into());
        let stack = e.stack().unwrap_or_default();
        return format!("{name}: {}\n{}", e.message().unwrap_or_default(), stack.trim_end());
    }
    ctx.json_stringify(v)
        .ok()
        .flatten()
        .and_then(|s| s.to_string().ok())
        .unwrap_or_else(|| "a value that isn't an Error".into())
}

/// Define `__host`, the functions the prelude builds on.
fn install_host<'js>(ctx: &rquickjs::Ctx<'js>, host: &Rc<RefCell<Host>>) -> rquickjs::Result<()> {
    let obj = Object::new(ctx.clone())?;

    obj.set(
        "log",
        Function::new(ctx.clone(), |level: String, message: String| match level.as_str() {
            "error" => log::error!("{message}"),
            "warn" => log::warn!("{message}"),
            "debug" => log::debug!("{message}"),
            _ => log::info!("{message}"),
        })?,
    )?;

    let h = host.clone();
    obj.set(
        "setTimer",
        Function::new(ctx.clone(), move |id: u64, ms: f64| {
            let mut h = h.borrow_mut();
            let due = Instant::now() + Duration::from_secs_f64(ms.clamp(0.0, 1e9) / 1000.0);
            if let Some(old) = h.timer_due.insert(id, due) {
                h.timers.remove(&(old, id));
            }
            h.timers.insert((due, id));
        })?,
    )?;

    let h = host.clone();
    obj.set(
        "clearTimer",
        Function::new(ctx.clone(), move |id: u64| {
            let mut h = h.borrow_mut();
            if let Some(due) = h.timer_due.remove(&id) {
                h.timers.remove(&(due, id));
            }
        })?,
    )?;

    let h = host.clone();
    obj.set(
        "request",
        Function::new(ctx.clone(), move |id: u64, to: String, method: String, params: String| {
            let (bus, events) = {
                let h = h.borrow();
                (h.bus.clone(), h.events.clone())
            };
            let task = tokio::task::spawn_local(async move {
                let params: Value = serde_json::from_str(&params).unwrap_or(Value::Null);
                let event = match bus.request(&to, &method, params).await {
                    Ok(v) => Dispatch { id, kind: "ok", data: v.to_string() },
                    Err(e) => Dispatch { id, kind: "err", data: error_json(&e) },
                };
                let _ = events.send(event);
            });
            h.borrow_mut().tasks.insert(id, task);
        })?,
    )?;

    let h = host.clone();
    obj.set(
        "stream",
        Function::new(ctx.clone(), move |id: u64, to: String, method: String, params: String| {
            let (bus, events) = {
                let h = h.borrow();
                (h.bus.clone(), h.events.clone())
            };
            let task = tokio::task::spawn_local(async move {
                let params: Value = serde_json::from_str(&params).unwrap_or(Value::Null);
                let mut stream = match bus.request_stream(&to, &method, params).await {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = events.send(Dispatch { id, kind: "err", data: error_json(&e) });
                        return;
                    }
                };
                loop {
                    let event = match stream.next().await {
                        Some(Ok(part)) => Dispatch { id, kind: "part", data: part.to_string() },
                        Some(Err(e)) => Dispatch { id, kind: "err", data: error_json(&e) },
                        None => Dispatch { id, kind: "end", data: String::new() },
                    };
                    let last = event.kind != "part";
                    let _ = events.send(event);
                    if last {
                        break;
                    }
                }
            });
            h.borrow_mut().tasks.insert(id, task);
        })?,
    )?;

    let h = host.clone();
    obj.set(
        "cancel",
        Function::new(ctx.clone(), move |id: u64| {
            // Aborting drops the request or stream, which tells the broker.
            if let Some(task) = h.borrow_mut().tasks.remove(&id) {
                task.abort();
            }
        })?,
    )?;

    let h = host.clone();
    obj.set(
        "exit",
        Function::new(ctx.clone(), move |code: i32| {
            h.borrow_mut().exit = Some(code);
        })?,
    )?;

    ctx.globals().set("__host", obj)
}

fn error_json(e: &bus_client::CallError) -> String {
    let message = match e {
        bus_client::CallError::Bus(b) => b.message.clone(),
        other => other.to_string(),
    };
    serde_json::json!({"code": e.code(), "message": message}).to_string()
}
