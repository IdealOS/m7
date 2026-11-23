//! The async shell around [`Router`]: accepts connections, reads and writes frames,
//! and runs the router's clock.
//!
//! One task owns the router and every connection's outgoing queue, so nothing is
//! shared or locked. Each connection has a reader task, which decodes frames and
//! feeds them to the router through one bounded channel (so a client flooding the
//! bus waits on its own socket, not the broker's memory), and a writer task with a
//! queue of encoded frames, bounded by count and by bytes (frames can be up to
//! `MAX_FRAME_BYTES`, so a count alone would allow gigabytes). A connection whose
//! queue fills, because it
//! stopped reading, is dropped rather than letting it hold messages for everyone.
//! The one exception is the tap's trace: a tap that falls behind (a terminal can't
//! print as fast as the bus routes) misses trace messages instead.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use protocol::bus::{FrameError, MAX_FRAME_BYTES, Message, TAP_TOPIC, encode_frame};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinSet};

use crate::router::{Action, ConnId, Event, Policy, Router, Trust};

/// Frames a connection may have waiting to be written before it counts as stuck.
pub const OUTBOX_FRAMES: usize = 4096;
/// Bytes a connection may have waiting, beyond the frame it's currently sent, before
/// it counts as stuck. One frame of any allowed size always fits in an empty queue.
pub const OUTBOX_BYTES: usize = 16 * 1024 * 1024;
/// Events waiting for the router, from all readers together.
const INBOX_EVENTS: usize = 1024;

type Frame = Arc<Vec<u8>>;

/// What connection tasks tell the router task.
enum Input {
    Opened { conn: ConnId, trust: Trust, outbox: Outbox },
    Event(Event),
}

/// Serve the bus on `listener` until this future is dropped, which stops the router
/// and closes every connection. Connections are accepted only from the broker's own
/// user or root.
pub async fn serve(listener: UnixListener, policy: Box<dyn Policy>) -> io::Result<()> {
    let (tx, rx) = mpsc::channel(INBOX_EVENTS);
    // Every task the bus runs lives here, so dropping `serve` aborts them all.
    let mut tasks = JoinSet::new();
    tasks.spawn(route(Router::new(policy), rx));
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    let mut next_conn: ConnId = 1;
    loop {
        let (stream, _) = listener.accept().await?;
        while tasks.try_join_next().is_some() {} // reap finished connections
        let peer = match stream.peer_cred() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("bus: dropped a connection with no peer credentials: {e}");
                continue;
            }
        };
        if peer.uid() != me && peer.uid() != 0 {
            log::warn!(
                "bus: refused uid {} (pid {:?}): only uid {me} and root may connect",
                peer.uid(),
                peer.pid()
            );
            continue;
        }
        let conn = next_conn;
        next_conn += 1;
        log::debug!("bus: connection {conn} from pid {:?}", peer.pid());
        start(conn, Trust::System, stream, &tx, &mut tasks).await;
    }
}

/// Start a connection's reader and writer tasks and register it with the router.
async fn start(
    conn: ConnId,
    trust: Trust,
    stream: UnixStream,
    tx: &mpsc::Sender<Input>,
    tasks: &mut JoinSet<()>,
) {
    let (read, write) = stream.into_split();
    let (frames, queue) = mpsc::channel(OUTBOX_FRAMES);
    let queued = Arc::new(AtomicUsize::new(0));
    let reader = tasks.spawn(read_frames(conn, read, tx.clone()));
    tasks.spawn(write_frames(conn, write, queue, queued.clone(), tx.clone()));
    let outbox = Outbox { frames, queued, reader };
    let _ = tx.send(Input::Opened { conn, trust, outbox }).await;
}

async fn read_frames(conn: ConnId, read: OwnedReadHalf, tx: mpsc::Sender<Input>) {
    let mut read = BufReader::new(read);
    let mut body = Vec::new();
    let why = loop {
        let mut header = [0u8; 4];
        if let Err(e) = read.read_exact(&mut header).await {
            break if e.kind() == io::ErrorKind::UnexpectedEof { None } else { Some(e.to_string()) };
        }
        let len = u32::from_be_bytes(header);
        if len > MAX_FRAME_BYTES {
            break Some(format!("{:?}", FrameError::TooLarge(len)));
        }
        body.resize(len as usize, 0);
        if let Err(e) = read.read_exact(&mut body).await {
            break Some(e.to_string());
        }
        match serde_json::from_slice::<Message>(&body) {
            Ok(msg) => {
                if tx.send(Input::Event(Event::Message { conn, msg })).await.is_err() {
                    return;
                }
            }
            Err(e) => break Some(format!("bad message: {e}")),
        }
    };
    if let Some(why) = why {
        log::warn!("bus: dropping connection {conn}: {why}");
    }
    let _ = tx.send(Input::Event(Event::Closed { conn })).await;
}

async fn write_frames(
    conn: ConnId,
    mut write: OwnedWriteHalf,
    mut frames: mpsc::Receiver<Frame>,
    queued: Arc<AtomicUsize>,
    tx: mpsc::Sender<Input>,
) {
    let mut batch = Vec::new();
    while let Some(frame) = frames.recv().await {
        // Write everything that's queued in one go.
        batch.clear();
        batch.extend_from_slice(&frame);
        while let Ok(more) = frames.try_recv() {
            batch.extend_from_slice(&more);
        }
        let result = write.write_all(&batch).await;
        queued.fetch_sub(batch.len(), Ordering::Relaxed);
        if let Err(e) = result {
            log::debug!("bus: write to connection {conn}: {e}");
            let _ = tx.send(Input::Event(Event::Closed { conn })).await;
            return;
        }
    }
    // The router dropped the outbox: the connection is closed. Dropping `write`
    // shuts down our half of the socket.
}

struct Outbox {
    frames: mpsc::Sender<Frame>,
    /// Bytes in `frames` or being written; the writer subtracts what it wrote.
    queued: Arc<AtomicUsize>,
    reader: AbortHandle,
}

impl Outbox {
    /// Queue `frame` unless the queue is full by count or by bytes.
    fn offer(&self, frame: &Frame) -> bool {
        let queued = self.queued.load(Ordering::Relaxed);
        if queued > 0 && queued + frame.len() > OUTBOX_BYTES {
            return false;
        }
        if self.frames.try_send(frame.clone()).is_err() {
            return false;
        }
        self.queued.fetch_add(frame.len(), Ordering::Relaxed);
        true
    }
}

async fn route(mut router: Router, mut rx: mpsc::Receiver<Input>) {
    let mut outboxes: HashMap<ConnId, Outbox> = HashMap::new();
    let mut actions = Vec::new();
    let mut events = VecDeque::new();
    // Taps already told that they're missing trace messages.
    let mut lagging_taps = HashSet::new();
    loop {
        let input = match router.next_deadline() {
            Some(deadline) => tokio::select! {
                input = rx.recv() => input,
                () = tokio::time::sleep_until(deadline.into()) => Some(Input::Event(Event::Tick)),
            },
            None => rx.recv().await,
        };
        let Some(input) = input else { return };
        let event = match input {
            Input::Opened { conn, trust, outbox } => {
                outboxes.insert(conn, outbox);
                Event::Opened { conn, trust }
            }
            Input::Event(event) => event,
        };
        events.push_back(event);
        // Delivering can drop a stuck connection, which is itself an event.
        while let Some(event) = events.pop_front() {
            if let Event::Closed { conn } = event
                && let Some(gone) = outboxes.remove(&conn)
            {
                gone.reader.abort();
                lagging_taps.remove(&conn);
            }
            router.handle(Instant::now(), event, &mut actions);
            for Action::Deliver { to, msg } in actions.drain(..) {
                let trace = matches!(&msg, Message::Publish { topic, .. } if topic == TAP_TOPIC);
                let frame = match encode_frame(&msg) {
                    Ok(f) => Arc::new(f),
                    Err(e) => {
                        log::error!("bus: cannot encode {msg:?}: {e}");
                        continue;
                    }
                };
                for conn in to {
                    let Some(out) = outboxes.get(&conn) else { continue };
                    if out.offer(&frame) {
                        continue;
                    }
                    if trace {
                        if lagging_taps.insert(conn) {
                            log::warn!(
                                "bus: the tap on connection {conn} is falling behind; it will miss messages"
                            );
                        }
                    } else {
                        log::warn!("bus: dropping connection {conn}: it stopped reading (its queue is full)");
                        events.push_back(Event::Closed { conn });
                    }
                }
            }
        }
    }
}
