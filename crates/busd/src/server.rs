//! The async shell around [`Router`]: accepts connections, reads and writes frames
//! (with their file descriptors), and runs the router's clock.
//!
//! One task owns the router and every connection's outgoing queue, so nothing is
//! shared or locked. Each connection has a reader task, which decodes frames and
//! feeds them to the router through one bounded channel (so a client flooding the
//! bus waits on its own socket, not the broker's memory), and a writer task with a
//! queue of encoded frames, bounded by count, by bytes (frames can be up to
//! `MAX_FRAME_BYTES`, so a count alone would allow gigabytes) and by descriptors
//! (so a stuck reader can't exhaust the broker's). A connection whose
//! queue fills, because it stopped reading, is dropped rather than letting it hold
//! messages for everyone. The one exception is the tap's trace: a tap that falls
//! behind (a terminal can't print as fast as the bus routes) misses trace messages
//! instead.
//!
//! Descriptors that arrive with a message go out with whichever of the router's
//! actions declares descriptors (the forwarded request, the response, the
//! publish), or start a connection for `adopt`. If no action takes them (the
//! request failed, nobody subscribed), they're closed.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use bus_client::wire::{FrameReader, write_with_fds};
use protocol::bus::{Message, TAP_TOPIC, encode_frame};
use protocol::unix::MAX_FDS_PER_SEND;
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinSet};

use crate::router::{Action, ConnId, Event, Policy, Router, Trust};

/// Frames a connection may have waiting to be written before it counts as stuck.
pub const OUTBOX_FRAMES: usize = 4096;
/// Bytes a connection may have waiting, beyond the frame it's currently sent, before
/// it counts as stuck. One frame of any allowed size always fits in an empty queue.
pub const OUTBOX_BYTES: usize = 16 * 1024 * 1024;
/// Descriptors a connection may have waiting, so one that stops reading can't
/// hold enough open descriptors to exhaust the broker's.
pub const OUTBOX_FDS: usize = 256;
/// Events waiting for the router, from all readers together.
const INBOX_EVENTS: usize = 1024;

/// An encoded frame and the descriptors that go with it.
#[derive(Clone)]
struct Frame {
    bytes: Arc<Vec<u8>>,
    fds: Arc<Vec<OwnedFd>>,
}

/// What connection tasks tell the router task.
enum Input {
    Opened { conn: ConnId, trust: Trust, outbox: Outbox, adopted_for: Option<(ConnId, u64)> },
    Message { conn: ConnId, msg: Message, fds: Vec<OwnedFd> },
    Event(Event),
}

/// A connection session handed over, for the accept loop to start.
struct Adoption {
    stream: UnixStream,
    trust: Trust,
    requester: ConnId,
    id: u64,
}

/// Serve the bus on `listener` until this future is dropped, which stops the router
/// and closes every connection. Connections are accepted only from the broker's own
/// user or root. `session`, if given, is the connection session holds: trusted, and
/// the only one that may hand the broker more connections.
pub async fn serve(
    listener: UnixListener,
    policy: Box<dyn Policy>,
    session: Option<UnixStream>,
) -> io::Result<()> {
    let (tx, rx) = mpsc::channel(INBOX_EVENTS);
    let (adopt_tx, mut adoptions) = mpsc::unbounded_channel();
    // Every task the bus runs lives here, so dropping `serve` aborts them all.
    let mut tasks = JoinSet::new();
    tasks.spawn(route(Router::new(policy), rx, adopt_tx));
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    let mut next_conn: ConnId = 1;
    let mut new_conn = || {
        let conn = next_conn;
        next_conn += 1;
        conn
    };
    if let Some(stream) = session {
        let conn = new_conn();
        log::info!("bus: session is connection {conn}");
        start(conn, Trust::Session, stream, None, &tx, &mut tasks).await;
    }
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let stream = match accepted {
                    Ok((stream, _)) => stream,
                    Err(e) => {
                        // Out of descriptors, most likely; existing connections keep
                        // working, so wait a moment and keep serving them.
                        log::warn!("bus: accept: {e}");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        continue;
                    }
                };
                while tasks.try_join_next().is_some() {} // reap finished connections
                let peer = match stream.peer_cred() {
                    Ok(c) => c,
                    Err(e) => {
                        log::warn!("bus: dropped a connection with no peer credentials: {e}");
                        continue;
                    }
                };
                if peer.uid() != me && peer.uid() != 0 {
                    log::warn!("bus: refused uid {} (pid {:?}): only uid {me} and root may connect", peer.uid(), peer.pid());
                    continue;
                }
                let conn = new_conn();
                log::debug!("bus: connection {conn} from pid {:?}", peer.pid());
                start(conn, Trust::System, stream, None, &tx, &mut tasks).await;
            }
            Some(a) = adoptions.recv() => {
                let conn = new_conn();
                log::info!("bus: connection {conn} handed over by session as {:?}", a.trust);
                start(conn, a.trust, a.stream, Some((a.requester, a.id)), &tx, &mut tasks).await;
            }
        }
    }
}

/// Start a connection's reader and writer tasks and register it with the router.
async fn start(
    conn: ConnId,
    trust: Trust,
    stream: UnixStream,
    adopted_for: Option<(ConnId, u64)>,
    tx: &mpsc::Sender<Input>,
    tasks: &mut JoinSet<()>,
) {
    let (read, write) = stream.into_split();
    let (frames, queue) = mpsc::channel(OUTBOX_FRAMES);
    let queued = Arc::new(Queued::default());
    let reader = tasks.spawn(read_frames(conn, FrameReader::new(read), tx.clone()));
    tasks.spawn(write_frames(conn, write, queue, queued.clone(), tx.clone()));
    let outbox = Outbox { frames, queued, reader };
    let _ = tx.send(Input::Opened { conn, trust, outbox, adopted_for }).await;
}

async fn read_frames(conn: ConnId, mut reader: FrameReader, tx: mpsc::Sender<Input>) {
    loop {
        match reader.next().await {
            Ok(Some((msg, fds))) => {
                if tx.send(Input::Message { conn, msg, fds }).await.is_err() {
                    return;
                }
            }
            Ok(None) => break,
            Err(e) => {
                log::warn!("bus: dropping connection {conn}: {e}");
                break;
            }
        }
    }
    let _ = tx.send(Input::Event(Event::Closed { conn })).await;
}

async fn write_frames(
    conn: ConnId,
    write: OwnedWriteHalf,
    mut frames: mpsc::Receiver<Frame>,
    queued: Arc<Queued>,
    tx: mpsc::Sender<Input>,
) {
    let mut bytes = Vec::new();
    let mut held: Vec<Frame> = Vec::new();
    let mut next = frames.recv().await;
    while let Some(first) = next.take() {
        // Write everything that's queued in one go, as long as its descriptors fit
        // in one send.
        bytes.clear();
        held.clear();
        let mut fd_count = first.fds.len();
        bytes.extend_from_slice(&first.bytes);
        held.push(first);
        while let Ok(more) = frames.try_recv() {
            if fd_count + more.fds.len() > MAX_FDS_PER_SEND {
                next = Some(more);
                break;
            }
            fd_count += more.fds.len();
            bytes.extend_from_slice(&more.bytes);
            held.push(more);
        }
        let fds: Vec<BorrowedFd> = held.iter().flat_map(|f| f.fds.iter().map(|fd| fd.as_fd())).collect();
        let result = write_with_fds(&write, &bytes, &fds).await;
        queued.bytes.fetch_sub(bytes.len(), Ordering::Relaxed);
        queued.fds.fetch_sub(fd_count, Ordering::Relaxed);
        if let Err(e) = result {
            log::debug!("bus: write to connection {conn}: {e}");
            let _ = tx.send(Input::Event(Event::Closed { conn })).await;
            return;
        }
        if next.is_none() {
            next = frames.recv().await;
        }
    }
    // The router dropped the outbox: the connection is closed. Dropping `write`
    // shuts down our half of the socket.
}

/// Bytes and descriptors in an outbox or being written; the writer subtracts what
/// it wrote.
#[derive(Default)]
struct Queued {
    bytes: AtomicUsize,
    fds: AtomicUsize,
}

struct Outbox {
    frames: mpsc::Sender<Frame>,
    queued: Arc<Queued>,
    reader: AbortHandle,
}

impl Outbox {
    /// Queue `frame` unless the queue is full by count, bytes or descriptors.
    fn offer(&self, frame: &Frame) -> bool {
        let (len, fds) = (frame.bytes.len(), frame.fds.len());
        let bytes = self.queued.bytes.load(Ordering::Relaxed);
        if bytes > 0 && bytes + len > OUTBOX_BYTES {
            return false;
        }
        if self.queued.fds.load(Ordering::Relaxed) + fds > OUTBOX_FDS {
            return false;
        }
        if self.frames.try_send(frame.clone()).is_err() {
            return false;
        }
        self.queued.bytes.fetch_add(len, Ordering::Relaxed);
        self.queued.fds.fetch_add(fds, Ordering::Relaxed);
        true
    }
}

/// A descriptor passed to `adopt` as a connection, if it is a socket.
fn adopted_stream(fd: OwnedFd) -> io::Result<UnixStream> {
    // SAFETY: fstat on a descriptor we own, into a zeroed struct.
    let is_socket = unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        libc::fstat(std::os::fd::AsRawFd::as_raw_fd(&fd), &mut st) == 0
            && st.st_mode & libc::S_IFMT == libc::S_IFSOCK
    };
    if !is_socket {
        return Err(io::Error::other("the descriptor isn't a socket"));
    }
    let std = std::os::unix::net::UnixStream::from(fd);
    std.set_nonblocking(true)?;
    UnixStream::from_std(std)
}

async fn route(mut router: Router, mut rx: mpsc::Receiver<Input>, adopt: mpsc::UnboundedSender<Adoption>) {
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
        // Descriptors that came with this input, until an action takes them.
        let mut fds = Vec::new();
        let event = match input {
            Input::Opened { conn, trust, outbox, adopted_for } => {
                outboxes.insert(conn, outbox);
                Event::Opened { conn, trust, adopted_for }
            }
            Input::Message { conn, msg, fds: arrived } => {
                fds = arrived;
                Event::Message { conn, msg }
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
            for action in actions.drain(..) {
                let (to, msg) = match action {
                    Action::Deliver { to, msg } => (to, msg),
                    Action::Adopt { requester, id, trust } => {
                        let stream = match fds.pop().map(adopted_stream) {
                            Some(Ok(stream)) => stream,
                            Some(Err(e)) => {
                                let reply = Message::err(id, protocol::bus::code::BAD_REQUEST, e.to_string());
                                deliver(
                                    &outboxes,
                                    &[requester],
                                    reply,
                                    Vec::new(),
                                    &mut events,
                                    &mut lagging_taps,
                                );
                                continue;
                            }
                            None => continue, // the reader checked the count; can't happen
                        };
                        let _ = adopt.send(Adoption { stream, trust, requester, id });
                        continue;
                    }
                };
                let attached = if msg.fds() > 0 { std::mem::take(&mut fds) } else { Vec::new() };
                deliver(&outboxes, &to, msg, attached, &mut events, &mut lagging_taps);
            }
        }
    }
}

/// Encode `msg` once and queue it, with `fds`, for each connection in `to`.
fn deliver(
    outboxes: &HashMap<ConnId, Outbox>,
    to: &[ConnId],
    msg: Message,
    fds: Vec<OwnedFd>,
    events: &mut VecDeque<Event>,
    lagging_taps: &mut HashSet<ConnId>,
) {
    let trace = matches!(&msg, Message::Publish { topic, .. } if topic == TAP_TOPIC);
    let frame = match encode_frame(&msg) {
        Ok(bytes) => Frame { bytes: Arc::new(bytes), fds: Arc::new(fds) },
        Err(e) => {
            log::error!("bus: cannot encode {msg:?}: {e}");
            return;
        }
    };
    for &conn in to {
        let Some(out) = outboxes.get(&conn) else { continue };
        if out.offer(&frame) {
            continue;
        }
        if trace {
            if lagging_taps.insert(conn) {
                log::warn!("bus: the tap on connection {conn} is falling behind; it will miss messages");
            }
        } else {
            log::warn!("bus: dropping connection {conn}: it stopped reading (its queue is full)");
            events.push_back(Event::Closed { conn });
        }
    }
}
