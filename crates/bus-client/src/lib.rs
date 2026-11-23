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

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use protocol::bus::{self, BROKER, BusError, MAX_FRAME_BYTES, Message, Sender, encode_frame};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot};

/// Frames waiting to be written.
const OUTGOING_FRAMES: usize = 1024;

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

/// A message published on a topic this client subscribed to.
#[derive(Debug, Clone, PartialEq)]
pub struct Publication {
    pub topic: String,
    pub data: Value,
    pub from: Option<Sender>,
}

/// Something addressed to a name this client claimed.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// Answer it with [`Client::respond`], using `id`.
    Request { id: u64, from: Option<Sender>, method: String, params: Value },
    /// The requester gave up on request `id`; any answer will be dropped.
    Cancel { id: u64 },
}

struct Sub {
    key: u64,
    pattern: String,
    tx: mpsc::UnboundedSender<Publication>,
}

struct State {
    pending: HashMap<u64, oneshot::Sender<Result<Value, CallError>>>,
    subs: Vec<Sub>,
    /// Incoming requests queue here from the start, so none are lost between
    /// `claim` and `take_requests`. `None` once the connection closes.
    requests: Option<mpsc::UnboundedSender<Incoming>>,
    /// Handed out once by `take_requests`.
    requests_rx: Option<mpsc::UnboundedReceiver<Incoming>>,
    closed: bool,
}

/// What clients, subscriptions and in-flight requests share. Only they hold it,
/// not the background reader, so when the last one is dropped the outgoing queue
/// closes, the writer shuts down its half of the socket, and the broker sees the
/// connection end.
struct Shared {
    state: Arc<Mutex<State>>,
    out: mpsc::Sender<Vec<u8>>,
    next_id: AtomicU64,
}

impl Shared {
    fn send_now(&self, msg: &Message) {
        // Best effort, for cleanup in Drop: if the queue is full, the broker's
        // timeout ends the request anyway.
        if let Ok(frame) = encode_frame(msg) {
            let _ = self.out.try_send(frame);
        }
    }
}

/// A connection to the bus. Cheap to clone; clones share the connection.
#[derive(Clone)]
pub struct Client {
    shared: Arc<Shared>,
}

impl Client {
    /// Connect to the broker at `protocol::bus::default_socket_path()`.
    pub async fn connect_default() -> io::Result<Client> {
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
        tokio::spawn(read_frames(read, state.clone()));
        let shared = Arc::new(Shared { state, out, next_id: AtomicU64::new(1) });
        Client { shared }
    }

    fn next_id(&self) -> u64 {
        self.shared.next_id.fetch_add(1, Ordering::Relaxed)
    }

    async fn send(&self, msg: &Message) -> Result<(), CallError> {
        let frame = encode_frame(msg).map_err(|e| {
            CallError::Bus(BusError { code: bus::code::BAD_REQUEST.into(), message: e.to_string() })
        })?;
        self.shared.out.send(frame).await.map_err(|_| CallError::Disconnected)
    }

    /// Send a request and wait for its answer, for up to the broker's default
    /// timeout.
    pub async fn request(&self, to: &str, method: &str, params: Value) -> Result<Value, CallError> {
        self.call(to, method, params, None).await
    }

    /// Like [`Client::request`], but the broker answers `timeout` after `timeout`.
    pub async fn request_timeout(
        &self,
        to: &str,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CallError> {
        self.call(to, method, params, Some(timeout.as_millis().min(u64::MAX as u128) as u64)).await
    }

    async fn call(
        &self,
        to: &str,
        method: &str,
        params: Value,
        timeout_ms: Option<u64>,
    ) -> Result<Value, CallError> {
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
        let msg =
            Message::Request { id, to: to.into(), method: method.into(), params, timeout_ms, from: None };
        self.send(&msg).await?;
        let result = rx.await.unwrap_or(Err(CallError::Disconnected));
        guard.disarm();
        result
    }

    /// Who the broker says this connection is, and its connection number.
    pub async fn hello(&self) -> Result<(u64, Sender), CallError> {
        let v = self.request(BROKER, "hello", Value::Null).await?;
        let conn = v["conn"].as_u64().unwrap_or_default();
        let sender = serde_json::from_value(v["sender"].clone()).map_err(|e| {
            CallError::Bus(BusError { code: bus::code::BAD_REQUEST.into(), message: e.to_string() })
        })?;
        Ok((conn, sender))
    }

    /// Claim a service name, so requests addressed to it come to this connection.
    /// Read them from [`Client::take_requests`].
    pub async fn claim(&self, name: &str) -> Result<(), CallError> {
        self.request(BROKER, "claim", json!({"name": name})).await.map(drop)
    }

    /// The queue of requests sent to names this connection claimed. Returns `None`
    /// after the first call; requests that arrive before it are kept.
    pub fn take_requests(&self) -> Option<Requests> {
        self.shared.state.lock().unwrap().requests_rx.take().map(|rx| Requests { rx })
    }

    /// Answer request `id` from [`Client::take_requests`].
    pub async fn respond(&self, id: u64, result: Result<Value, BusError>) -> Result<(), CallError> {
        let msg = match result {
            Ok(v) => Message::ok(id, v),
            Err(e) => Message::Response { id, ok: false, result: None, error: Some(e) },
        };
        self.send(&msg).await
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
            self.send(&Message::Subscribe { id, topic: pattern.into() }).await?;
            answer.await.unwrap_or(Err(CallError::Disconnected))?;
        }
        Ok(sub)
    }

    pub async fn publish(&self, topic: &str, data: Value) -> Result<(), CallError> {
        self.send(&Message::Publish { topic: topic.into(), data, from: None }).await
    }
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

async fn write_frames(mut write: OwnedWriteHalf, mut frames: mpsc::Receiver<Vec<u8>>) {
    let mut batch = Vec::new();
    while let Some(frame) = frames.recv().await {
        batch.clear();
        batch.extend_from_slice(&frame);
        while let Ok(more) = frames.try_recv() {
            batch.extend_from_slice(&more);
        }
        if write.write_all(&batch).await.is_err() {
            return;
        }
    }
}

async fn read_frames(read: OwnedReadHalf, state: Arc<Mutex<State>>) {
    let mut read = BufReader::new(read);
    let mut body = Vec::new();
    loop {
        let mut header = [0u8; 4];
        if read.read_exact(&mut header).await.is_err() {
            break;
        }
        let len = u32::from_be_bytes(header);
        if len > MAX_FRAME_BYTES {
            log::warn!("bus-client: frame of {len} bytes from the broker; disconnecting");
            break;
        }
        body.resize(len as usize, 0);
        if read.read_exact(&mut body).await.is_err() {
            break;
        }
        match serde_json::from_slice::<Message>(&body) {
            Ok(msg) => dispatch(&mut state.lock().unwrap(), msg),
            Err(e) => log::warn!("bus-client: ignoring a message it can't parse: {e}"),
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

fn dispatch(state: &mut State, msg: Message) {
    match msg {
        Message::Response { id, ok, result, error } => {
            if let Some(waiter) = state.pending.remove(&id) {
                let answer = if ok {
                    Ok(result.unwrap_or(Value::Null))
                } else {
                    Err(CallError::Bus(error.unwrap_or_else(|| BusError {
                        code: "unknown".into(),
                        message: "error response without an error".into(),
                    })))
                };
                let _ = waiter.send(answer);
            }
        }
        Message::Publish { topic, data, from } => {
            for sub in state.subs.iter().filter(|s| bus::topic_matches(&s.pattern, &topic)) {
                let _ =
                    sub.tx.send(Publication { topic: topic.clone(), data: data.clone(), from: from.clone() });
            }
        }
        Message::Request { id, method, params, from, .. } => {
            let incoming = Incoming::Request { id, from, method, params };
            send_incoming(state, incoming);
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
