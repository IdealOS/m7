//! The `pty` service: shells on pseudo-terminals, and their screens.
//!
//! Each session is one task that owns its pty and its emulator ([`term`]): it
//! reads the shell's output into the emulator, streams screen frames to whoever
//! attached (at most one every [`FRAME`]), and takes writes, resizes and the hang
//! up from the main loop. See `protocol::pty` for the protocol.

pub mod pty;
pub mod term;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use bus_client::{Client, Incoming};
use protocol::BusError;
use protocol::bus::{Sender, code, valid_app_id};
use protocol::pty::{
    IdParams, MAX_CELLS, MAX_SESSIONS, MAX_WRITE_BYTES, OpenParams, ResizeParams, SessionEvent, WriteParams,
    method,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::Instant;

/// The most often a session streams a frame: changes in between are batched.
pub const FRAME: Duration = Duration::from_millis(16);
/// How long a hung-up shell gets to exit before it's killed.
const HANGUP_GRACE: Duration = Duration::from_secs(2);

/// What runs in a session, and where.
#[derive(Debug, Clone)]
pub struct Config {
    pub shell: Vec<String>,
    pub dir: PathBuf,
}

/// Who a request is from, for owning sessions: `app:ID`, `service:NAME` or
/// `conn:N`. A system service acting for an app says so with `"as"`.
pub fn caller(from: Option<&Sender>, params: &mut Value) -> Result<String, BusError> {
    let on_behalf = params.as_object_mut().and_then(|p| p.remove("as"));
    match (from, on_behalf) {
        (Some(Sender::App { app, .. }), None) => Ok(format!("app:{app}")),
        (Some(Sender::App { .. }), Some(_)) => {
            Err(error(code::PERMISSION_DENIED, "apps can't act for other apps"))
        }
        (_, Some(Value::String(app))) if valid_app_id(&app) => Ok(format!("app:{app}")),
        (_, Some(_)) => Err(error(code::BAD_REQUEST, "`as` takes an app id")),
        (Some(Sender::Service(name)), None) => Ok(format!("service:{name}")),
        (Some(Sender::Conn(n)), None) => Ok(format!("conn:{n}")),
        (None, None) => Err(error(code::BAD_REQUEST, "requests need a sender")),
    }
}

enum Cmd {
    Attach(u64),
    Detach(u64),
    Write(Vec<u8>),
    Resize(u16, u16),
    Hangup,
}

struct Session {
    owner: String,
    cmds: mpsc::UnboundedSender<Cmd>,
}

/// Serve on `bus`, which must already have claimed `pty`, until it closes.
pub async fn serve(bus: Client, config: Config) -> Result<(), String> {
    let Some(mut requests) = bus.take_requests() else { return Ok(()) };
    let mut sessions: HashMap<String, Session> = HashMap::new();
    // Open streams: the bus id of each `open` and `attach`, its session, and
    // whether it's the `open`.
    let mut streams: HashMap<u64, (String, bool)> = HashMap::new();
    let (ended_tx, mut ended) = mpsc::unbounded_channel::<String>();
    let mut next = 1u64;
    loop {
        let incoming = tokio::select! {
            incoming = requests.next() => match incoming {
                Some(i) => i,
                None => break,
            },
            Some(id) = ended.recv() => {
                sessions.remove(&id);
                streams.retain(|_, (s, _)| *s != id);
                continue;
            }
        };
        let (id, from, method, mut params, stream) = match incoming {
            Incoming::Request { id, from, method, params, stream, .. } => (id, from, method, params, stream),
            Incoming::Cancel { id } => {
                if let Some((session, open)) = streams.remove(&id)
                    && let Some(s) = sessions.get(&session)
                {
                    let _ = s.cmds.send(if open { Cmd::Hangup } else { Cmd::Detach(id) });
                }
                continue;
            }
        };
        let who = match caller(from.as_ref(), &mut params) {
            Ok(w) => w,
            Err(e) => {
                let _ = bus.respond(id, Err(e)).await;
                continue;
            }
        };
        // The session `params` names, if `who` owns it.
        let owned = |params: &Value, sessions: &HashMap<String, Session>| -> Result<String, BusError> {
            let sid = params.get("id").and_then(Value::as_str).unwrap_or_default();
            match sessions.get(sid) {
                None => Err(error("not_found", format!("no session {sid:?}"))),
                Some(s) if s.owner != who => Err(error(code::PERMISSION_DENIED, "that session isn't yours")),
                Some(_) => Ok(sid.to_owned()),
            }
        };
        let answer: Result<Option<Value>, BusError> = match method.as_str() {
            method::OPEN if !stream => {
                Err(error(code::BAD_REQUEST, "open must be sent as a streamed request"))
            }
            method::OPEN => match parse::<OpenParams>(params) {
                Ok(p) if sessions.len() >= MAX_SESSIONS => {
                    let _ = p;
                    Err(error(code::LIMIT_EXCEEDED, format!("more than {MAX_SESSIONS} sessions")))
                }
                Ok(p) => {
                    let (cols, rows) = (p.cols.clamp(1, MAX_CELLS), p.rows.clamp(1, MAX_CELLS));
                    match pty::spawn(&config.shell, &config.dir, cols, rows) {
                        Ok(pty) => {
                            let sid = next.to_string();
                            next += 1;
                            log::info!("ptyd: session {sid} for {who}");
                            let (cmds, rx) = mpsc::unbounded_channel();
                            sessions.insert(sid.clone(), Session { owner: who.clone(), cmds });
                            streams.insert(id, (sid.clone(), true));
                            let _ = bus.stream(id, to_value(SessionEvent::Opened { id: sid.clone() })).await;
                            let emu = term::Emulator::new(cols, rows);
                            tokio::spawn(run_session(bus.clone(), sid, id, pty, emu, rx, ended_tx.clone()));
                            Ok(None)
                        }
                        Err(e) => Err(error("internal", format!("starting the shell: {e}"))),
                    }
                }
                Err(e) => Err(e),
            },
            method::ATTACH if !stream => {
                Err(error(code::BAD_REQUEST, "attach must be sent as a streamed request"))
            }
            method::ATTACH => {
                parse::<IdParams>(params.clone()).and_then(|_| owned(&params, &sessions)).map(|sid| {
                    streams.insert(id, (sid.clone(), false));
                    let _ = sessions[&sid].cmds.send(Cmd::Attach(id));
                    None
                })
            }
            method::WRITE => parse::<WriteParams>(params.clone()).and_then(|p| {
                if p.data.len() > MAX_WRITE_BYTES {
                    return Err(error(
                        code::BAD_REQUEST,
                        format!("a write is at most {MAX_WRITE_BYTES} bytes"),
                    ));
                }
                let sid = owned(&params, &sessions)?;
                let _ = sessions[&sid].cmds.send(Cmd::Write(p.data.into_bytes()));
                Ok(Some(Value::Null))
            }),
            method::RESIZE => parse::<ResizeParams>(params.clone()).and_then(|p| {
                let sid = owned(&params, &sessions)?;
                let _ = sessions[&sid]
                    .cmds
                    .send(Cmd::Resize(p.cols.clamp(1, MAX_CELLS), p.rows.clamp(1, MAX_CELLS)));
                Ok(Some(Value::Null))
            }),
            other => Err(error(code::BAD_REQUEST, format!("pty has no method {other:?}"))),
        };
        match answer {
            Ok(Some(v)) => {
                let _ = bus.respond(id, Ok(v)).await;
            }
            Ok(None) => {} // a stream, answered as it goes
            Err(e) => {
                let _ = bus.respond(id, Err(e)).await;
            }
        }
    }
    Ok(())
}

/// One session, until its shell exits.
async fn run_session(
    bus: Client,
    sid: String,
    open: u64,
    mut pty: pty::Pty,
    mut emu: term::Emulator,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    ended: mpsc::UnboundedSender<String>,
) {
    let mut attached: Vec<u64> = Vec::new();
    let mut flush_at: Option<Instant> = None;
    let mut kill_at: Option<Instant> = None;
    let mut buf = vec![0u8; 16 * 1024];
    let mut hung_up = false;
    loop {
        let far = Instant::now() + Duration::from_secs(3600);
        tokio::select! {
            n = pty.read(&mut buf) => match n {
                Ok(n) if n > 0 => {
                    emu.feed(&buf[..n]);
                    if let Some(title) = emu.term.take_title() {
                        let _ = bus.stream(open, to_value(SessionEvent::Title { title })).await;
                    }
                    let replies = emu.term.take_replies();
                    if !replies.is_empty() {
                        let _ = pty.write(&replies).await;
                    }
                    flush_at.get_or_insert_with(|| Instant::now() + FRAME);
                }
                _ => break,
            },
            cmd = cmds.recv() => match cmd {
                Some(Cmd::Attach(stream)) => {
                    send_frame(&bus, &attached, emu.term.frame(false)).await;
                    let _ = bus.stream(stream, to_value(emu.term.snapshot())).await;
                    attached.push(stream);
                }
                Some(Cmd::Detach(stream)) => attached.retain(|s| *s != stream),
                Some(Cmd::Write(bytes)) => {
                    let _ = pty.write(&bytes).await;
                }
                Some(Cmd::Resize(cols, rows)) => {
                    if emu.term.size() != (cols, rows) {
                        emu.term.resize(cols, rows);
                        pty.resize(cols, rows);
                        flush_at = Some(Instant::now());
                    }
                }
                Some(Cmd::Hangup) | None if !hung_up => {
                    hung_up = true;
                    pty.hangup();
                    kill_at = Some(Instant::now() + HANGUP_GRACE);
                }
                Some(Cmd::Hangup) | None => {}
            },
            _ = tokio::time::sleep_until(flush_at.unwrap_or(far)), if flush_at.is_some() => {
                flush_at = None;
                send_frame(&bus, &attached, emu.term.frame(false)).await;
            }
            _ = tokio::time::sleep_until(kill_at.unwrap_or(far)), if kill_at.is_some() => {
                kill_at = None;
                let _ = pty.child.start_kill();
            }
        }
    }
    send_frame(&bus, &attached, emu.term.frame(false)).await;
    let code = match pty.child.wait().await {
        Ok(status) => {
            use std::os::unix::process::ExitStatusExt;
            status.code().unwrap_or_else(|| -status.signal().unwrap_or(0))
        }
        Err(_) => -1,
    };
    log::info!("ptyd: session {sid} ended ({code})");
    let _ = bus.stream(open, to_value(SessionEvent::Exited { code })).await;
    let _ = bus.respond(open, Ok(Value::Null)).await;
    for s in attached {
        let _ = bus.respond(s, Ok(Value::Null)).await;
    }
    let _ = ended.send(sid);
}

async fn send_frame(bus: &Client, to: &[u64], frame: Option<protocol::pty::ScreenFrame>) {
    let Some(frame) = frame else { return };
    let v = to_value(frame);
    for &s in to {
        let _ = bus.stream(s, v.clone()).await;
    }
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, BusError> {
    serde_json::from_value(params).map_err(|e| error(code::BAD_REQUEST, format!("bad params: {e}")))
}

fn to_value(v: impl Serialize) -> Value {
    serde_json::to_value(v).expect("results serialize")
}

fn error(code: &str, message: impl Into<String>) -> BusError {
    BusError { code: code.into(), message: message.into() }
}
