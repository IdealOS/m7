//! Async client for the idealos message bus.
//!
//! ```no_run
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! let bus = bus_client::Client::connect_default().await?;
//! let answer = bus.request("db", "query", serde_json::json!({"type": "Note"})).await?;
//! let mut events = bus.subscribe("ui.*").await?;
//! while let Some(event) = events.next().await {
//!     println!("{} {}", event.topic, event.data);
//! }
//! # Ok(()) }
//! ```
//!
//! A background task reads the connection and hands each message to whoever is
//! waiting for it: a pending request, the matching subscriptions, or (for a
//! service) the queue returned by [`Client::take_requests`]. Dropping a request's
//! future before it finishes sends the broker a `cancel`; dropping a subscription
//! unsubscribes once no other subscription uses the same pattern.
//!
//! Requests, responses and publishes can carry file descriptors (`*_with`
//! methods); received ones arrive on [`Reply`], [`Incoming::Request`] and
//! [`Publication`].

pub mod wire;

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use protocol::broker::{NameParams, method};
use protocol::bus::{self, BROKER, BusError, Message, Sender, encode_frame};
use protocol::unix::{MAX_FDS_PER_MESSAGE, MAX_FDS_PER_SEND};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{mpsc, oneshot};
use wire::{FrameReader, write_with_fds};

pub use protocol::broker::{HelloResult, ListResult};

/// Frames waiting to be written.
const OUTGOING_FRAMES: usize = 1024;

/// Set by `session` for a service it gave a bus connection: the descriptor number.
pub const BUS_FD_ENV: &str = "IDEALOS_BUS_FD";

#[derive(Debug, Clone, PartialEq)]
pub enum CallError {
    /// The broker or the service answered with an error.
    Bus(BusError),
    /// The connection closed before an answer came.
    Disconnected,
}

impl CallError {
    /// The error code, or `"disconnected"`.
    pub fn code(&self) -> &str {
        match self {
            CallError::Bus(e) => &e.code,
            CallError::Disconnected => "disconnected",
        }
    }

    fn bad_request(message: impl Into<String>) -> CallError {
        CallError::Bus(BusError { code: bus::code::BAD_REQUEST.into(), message: message.into() })
    }
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CallError::Bus(e) => write!(f, "{}: {}", e.code, e.message),
            CallError::Disconnected => write!(f, "disconnected from the bus"),
        }
    }
}

impl std::error::Error for CallError {}

/// A successful answer and the descriptors that came with it.
#[derive(Debug)]
pub struct Reply {
    pub result: Value,
    pub fds: Vec<OwnedFd>,
}

/// A message published on a topic this client subscribed to.
#[derive(Debug)]
pub struct Publication {
    pub topic: String,
    pub data: Value,
    pub from: Option<Sender>,
    pub fds: Vec<OwnedFd>,
}

/// Something addressed to a name this client claimed.
#[derive(Debug)]
pub enum Incoming {
    /// Answer it with [`Client::respond`], using `id`.
    Request { id: u64, from: Option<Sender>, method: String, params: Value, fds: Vec<OwnedFd> },
    /// The requester gave up on request `id`; any answer will be dropped.
    Cancel { id: u64 },
}

struct Sub {
    key: u64,
    pattern: String,
    tx: mpsc::UnboundedSender<Publication>,
}

struct State {
    pending: HashMap<u64, oneshot::Sender<Result<Reply, CallError>>>,
    subs: Vec<Sub>,
    /// Incoming requests queue here from the start, so none are lost between
    /// `claim` and `take_requests`. `None` once the connection closes.
    requests: Option<mpsc::UnboundedSender<Incoming>>,
    /// Handed out once by `take_requests`.
    requests_rx: Option<mpsc::UnboundedReceiver<Incoming>>,
    closed: bool,
}

/// An encoded frame and the descriptors to send with it.
type Outgoing = (Vec<u8>, Vec<OwnedFd>);

/// What clients, subscriptions and in-flight requests share. Only they hold it,
/// not the background reader, so when the last one is dropped the outgoing queue
/// closes, the writer shuts down its half of the socket, and the broker sees the
/// connection end.
struct Shared {
    state: Arc<Mutex<State>>,
    out: mpsc::Sender<Outgoing>,
    next_id: AtomicU64,
}

impl Shared {
    fn send_now(&self, msg: &Message) {
        // Best effort, for cleanup in Drop: if the queue is full, the broker's
        // timeout ends the request anyway.
        if let Ok(frame) = encode_frame(msg) {
            let _ = self.out.try_send((frame, Vec::new()));
        }
    }
}

/// A connection to the bus. Cheap to clone; clones share the connection.
#[derive(Clone)]
pub struct Client {
    shared: Arc<Shared>,
}

/// Whether this process already took the connection in `IDEALOS_BUS_FD`.
static TOOK_BUS_FD: AtomicBool = AtomicBool::new(false);

impl Client {
    /// The connection `session` handed this process (`IDEALOS_BUS_FD`), the first
    /// time it's asked for; otherwise the broker at
    /// `protocol::bus::default_socket_path()`.
    pub async fn connect_default() -> io::Result<Client> {
        if let Some(stream) = take_handed_connection()? {
            return Ok(Client::from_stream(stream));
        }
        Client::connect(bus::default_socket_path()).await
    }

    pub async fn connect(path: impl AsRef<Path>) -> io::Result<Client> {
        let path = path.as_ref();
        let stream = UnixStream::connect(path).await.map_err(|e| {
            io::Error::new(e.kind(), format!("connect to the bus at {}: {e}", path.display()))
        })?;
        Ok(Client::from_stream(stream))
    }

    /// Use an already-connected stream, such as one `session` handed over.
    pub fn from_stream(stream: UnixStream) -> Client {
        let (read, write) = stream.into_split();
        let (out, frames) = mpsc::channel(OUTGOING_FRAMES);
        let (requests, requests_rx) = mpsc::unbounded_channel();
        let state = State {
            pending: HashMap::new(),
            subs: Vec::new(),
            requests: Some(requests),
            requests_rx: Some(requests_rx),
            closed: false,
        };
        let state = Arc::new(Mutex::new(state));
        tokio::spawn(write_frames(write, frames));
        tokio::spawn(read_frames(FrameReader::new(read), state.clone()));
        let shared = Arc::new(Shared { state, out, next_id: AtomicU64::new(1) });
        Client { shared }
    }

    fn next_id(&self) -> u64 {
        self.shared.next_id.fetch_add(1, Ordering::Relaxed)
    }

    async fn send(&self, msg: Message, fds: Vec<OwnedFd>) -> Result<(), CallError> {
        if fds.len() > MAX_FDS_PER_MESSAGE {
            return Err(CallError::bad_request(format!(
                "at most {MAX_FDS_PER_MESSAGE} descriptors per message"
            )));
        }
        let msg = msg.with_fds(fds.len() as u32);
        let frame = encode_frame(&msg).map_err(|e| CallError::bad_request(e.to_string()))?;
        self.shared.out.send((frame, fds)).await.map_err(|_| CallError::Disconnected)
    }

    /// Send a request and wait for its answer, for up to the broker's default
    /// timeout.
    pub async fn request(&self, to: &str, method: &str, params: Value) -> Result<Value, CallError> {
        Ok(self.request_with(to, method, params, Vec::new(), None).await?.result)
    }

    /// Like [`Client::request`], but the broker answers `timeout` after `timeout`.
    pub async fn request_timeout(
        &self,
        to: &str,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CallError> {
        Ok(self.request_with(to, method, params, Vec::new(), Some(timeout)).await?.result)
    }

    /// A request carrying descriptors, whose answer may carry some back.
    pub async fn request_with(
        &self,
        to: &str,
        method: &str,
        params: Value,
        fds: Vec<OwnedFd>,
        timeout: Option<Duration>,
    ) -> Result<Reply, CallError> {
        let id = self.next_id();
        let (tx, rx) = oneshot::channel();
        {
            let mut state = self.shared.state.lock().unwrap();
            if state.closed {
                return Err(CallError::Disconnected);
            }
            state.pending.insert(id, tx);
        }
        // From here on, being dropped means the caller gave up: tell the broker.
        let guard = CancelOnDrop { shared: &self.shared, id, armed: true };
        let timeout_ms = timeout.map(|t| t.as_millis().min(u64::MAX as u128) as u64);
        let msg = Message::Request {
            id,
            to: to.into(),
            method: method.into(),
            params,
            timeout_ms,
            fds: 0,
            from: None,
        };
        self.send(msg, fds).await?;
        let result = rx.await.unwrap_or(Err(CallError::Disconnected));
        guard.disarm();
        result
    }

    /// A broker method with typed params and result (see [`protocol::broker`]).
    async fn broker<T: DeserializeOwned>(
        &self,
        method: &str,
        params: impl Serialize,
    ) -> Result<T, CallError> {
        let params = serde_json::to_value(params).map_err(|e| CallError::bad_request(e.to_string()))?;
        let result = self.request(BROKER, method, params).await?;
        T::deserialize(&result)
            .map_err(|e| CallError::bad_request(format!("the broker's {method} answer: {e}")))
    }

    /// Who the broker says this connection is, and its connection number.
    pub async fn hello(&self) -> Result<HelloResult, CallError> {
        self.broker(method::HELLO, ()).await
    }

    /// Claimed service names and their connections, and how many connections
    /// the broker has.
    pub async fn list(&self) -> Result<ListResult, CallError> {
        self.broker(method::LIST, ()).await
    }

    /// Claim a service name, so requests addressed to it come to this connection.
    /// Read them from [`Client::take_requests`].
    pub async fn claim(&self, name: &str) -> Result<(), CallError> {
        self.broker::<()>(method::CLAIM, NameParams { name: name.into() }).await
    }

    /// Give up a name this connection claimed.
    pub async fn release(&self, name: &str) -> Result<(), CallError> {
        self.broker::<()>(method::RELEASE, NameParams { name: name.into() }).await
    }

    /// The queue of requests sent to names this connection claimed. Returns `None`
    /// after the first call; requests that arrive before it are kept.
    pub fn take_requests(&self) -> Option<Requests> {
        self.shared.state.lock().unwrap().requests_rx.take().map(|rx| Requests { rx })
    }

    /// Answer request `id` from [`Client::take_requests`].
    pub async fn respond(&self, id: u64, result: Result<Value, BusError>) -> Result<(), CallError> {
        self.respond_with(id, result, Vec::new()).await
    }

    /// Answer request `id` with descriptors (only sent with a successful answer).
    pub async fn respond_with(
        &self,
        id: u64,
        result: Result<Value, BusError>,
        fds: Vec<OwnedFd>,
    ) -> Result<(), CallError> {
        match result {
            Ok(v) => self.send(Message::ok(id, v), fds).await,
            Err(e) => self.send(Message::failed(id, e), Vec::new()).await,
        }
    }

    /// Subscribe to a topic or `prefix.*` pattern. The broker confirms before this
    /// returns, so nothing published afterwards is missed.
    pub async fn subscribe(&self, pattern: &str) -> Result<Subscription, CallError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let key = self.next_id();
        let first = {
            let mut state = self.shared.state.lock().unwrap();
            let first = !state.subs.iter().any(|s| s.pattern == pattern);
            state.subs.push(Sub { key, pattern: pattern.into(), tx });
            first
        };
        let sub = Subscription { rx, key, pattern: pattern.into(), shared: self.shared.clone() };
        if first {
            let id = self.next_id();
            let (done, answer) = oneshot::channel();
            self.shared.state.lock().unwrap().pending.insert(id, done);
            self.send(Message::Subscribe { id, topic: pattern.into() }, Vec::new()).await?;
            answer.await.unwrap_or(Err(CallError::Disconnected))?;
        }
        Ok(sub)
    }

    pub async fn publish(&self, topic: &str, data: Value) -> Result<(), CallError> {
        self.publish_with(topic, data, Vec::new()).await
    }

    /// Publish with descriptors; each subscriber gets its own copies.
    pub async fn publish_with(&self, topic: &str, data: Value, fds: Vec<OwnedFd>) -> Result<(), CallError> {
        self.send(Message::publish(topic, data), fds).await
    }
}

/// The connection in `IDEALOS_BUS_FD`, the first time this is called in a process.
/// The descriptor is checked to be a socket, so a stale number inherited by a
/// grandchild (which `session` never meant to give one) isn't used by mistake.
fn take_handed_connection() -> io::Result<Option<UnixStream>> {
    let Some(text) = std::env::var_os(BUS_FD_ENV) else { return Ok(None) };
    if TOOK_BUS_FD.swap(true, Ordering::SeqCst) {
        return Ok(None);
    }
    let fd: RawFd = text
        .to_str()
        .and_then(|t| t.parse().ok())
        .ok_or_else(|| io::Error::other(format!("{BUS_FD_ENV} is not a descriptor number: {text:?}")))?;
    // SAFETY: fstat with a zeroed struct; any descriptor number is safe to ask about.
    let is_socket = unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        libc::fstat(fd, &mut st) == 0 && st.st_mode & libc::S_IFMT == libc::S_IFSOCK
    };
    if !is_socket {
        return Err(io::Error::other(format!("{BUS_FD_ENV}={fd} is not an open socket")));
    }
    // SAFETY: session gave this process the descriptor for exactly this use, and
    // TOOK_BUS_FD makes sure it's only wrapped once. Mark it close-on-exec again so
    // programs this process starts don't inherit the connection.
    let stream = unsafe {
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        std::os::unix::net::UnixStream::from(OwnedFd::from_raw_fd(fd))
    };
    stream.set_nonblocking(true)?;
    Ok(Some(UnixStream::from_std(stream)?))
}

/// Sends a cancel for request `id` if dropped before [`CancelOnDrop::disarm`].
struct CancelOnDrop<'a> {
    shared: &'a Shared,
    id: u64,
    armed: bool,
}

impl CancelOnDrop<'_> {
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for CancelOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed && self.shared.state.lock().unwrap().pending.remove(&self.id).is_some() {
            self.shared.send_now(&Message::Cancel { id: self.id });
        }
    }
}

pub struct Requests {
    rx: mpsc::UnboundedReceiver<Incoming>,
}

impl Requests {
    /// The next request or cancel; `None` once the connection closes.
    pub async fn next(&mut self) -> Option<Incoming> {
        self.rx.recv().await
    }
}

pub struct Subscription {
    rx: mpsc::UnboundedReceiver<Publication>,
    key: u64,
    pattern: String,
    shared: Arc<Shared>,
}

impl Subscription {
    /// The next publication; `None` once the connection closes.
    pub async fn next(&mut self) -> Option<Publication> {
        self.rx.recv().await
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().unwrap();
        state.subs.retain(|s| s.key != self.key);
        if !state.closed && !state.subs.iter().any(|s| s.pattern == self.pattern) {
            let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
            drop(state);
            self.shared.send_now(&Message::Unsubscribe { id, topic: self.pattern.clone() });
        }
    }
}

async fn write_frames(write: OwnedWriteHalf, mut frames: mpsc::Receiver<Outgoing>) {
    let mut bytes = Vec::new();
    let mut fds: Vec<OwnedFd> = Vec::new();
    let mut next = frames.recv().await;
    while let Some((frame, frame_fds)) = next.take() {
        // Write everything that's queued in one go, as long as its descriptors fit
        // in one send.
        bytes.clear();
        fds.clear();
        bytes.extend_from_slice(&frame);
        fds.extend(frame_fds);
        while let Ok((more, more_fds)) = frames.try_recv() {
            if fds.len() + more_fds.len() > MAX_FDS_PER_SEND {
                next = Some((more, more_fds));
                break;
            }
            bytes.extend_from_slice(&more);
            fds.extend(more_fds);
        }
        let borrowed: Vec<BorrowedFd> = fds.iter().map(|f| f.as_fd()).collect();
        if write_with_fds(&write, &bytes, &borrowed).await.is_err() {
            return;
        }
        if next.is_none() {
            next = frames.recv().await;
        }
    }
}

async fn read_frames(mut reader: FrameReader, state: Arc<Mutex<State>>) {
    loop {
        match reader.next().await {
            Ok(Some((msg, fds))) => dispatch(&mut state.lock().unwrap(), msg, fds),
            Ok(None) => break,
            Err(e) => {
                log::warn!("bus-client: disconnecting: {e}");
                break;
            }
        }
    }
    // Fail everything still waiting, and end every stream.
    let mut state = state.lock().unwrap();
    state.closed = true;
    for (_, waiter) in state.pending.drain() {
        let _ = waiter.send(Err(CallError::Disconnected));
    }
    state.subs.clear();
    state.requests = None;
}

fn dispatch(state: &mut State, msg: Message, fds: Vec<OwnedFd>) {
    match msg {
        Message::Response { id, ok, result, error, .. } => {
            if let Some(waiter) = state.pending.remove(&id) {
                let answer = if ok {
                    Ok(Reply { result: result.unwrap_or(Value::Null), fds })
                } else {
                    Err(CallError::Bus(error.unwrap_or_else(|| BusError {
                        code: "unknown".into(),
                        message: "error response without an error".into(),
                    })))
                };
                let _ = waiter.send(answer);
            }
        }
        Message::Publish { topic, data, from, .. } => {
            let matching: Vec<&Sub> =
                state.subs.iter().filter(|s| bus::topic_matches(&s.pattern, &topic)).collect();
            let Some((last, rest)) = matching.split_last() else { return };
            // Every subscription but the last gets duplicates of the descriptors.
            for sub in rest {
                let copies = fds.iter().filter_map(|f| f.try_clone().ok()).collect();
                let p =
                    Publication { topic: topic.clone(), data: data.clone(), from: from.clone(), fds: copies };
                let _ = sub.tx.send(p);
            }
            let _ = last.tx.send(Publication { topic, data, from, fds });
        }
        Message::Request { id, method, params, from, .. } => {
            send_incoming(state, Incoming::Request { id, from, method, params, fds });
        }
        Message::Cancel { id } => send_incoming(state, Incoming::Cancel { id }),
        Message::Subscribe { .. } | Message::Unsubscribe { .. } => {
            log::warn!("bus-client: the broker sent a subscribe or unsubscribe; ignoring it");
        }
    }
}

fn send_incoming(state: &mut State, incoming: Incoming) {
    if let Some(tx) = &state.requests {
        let _ = tx.send(incoming);
    }
}
