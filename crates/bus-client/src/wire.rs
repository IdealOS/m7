//! Reading and writing bus frames, with file descriptors, on a tokio Unix socket.
//! Shared by the client and the broker.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use protocol::bus::{FrameError, Message, decode_frame};
use protocol::unix::{self, MAX_FDS_PER_MESSAGE, MAX_FDS_PER_SEND};
use tokio::io::Interest;
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

/// Bytes asked for per read.
const READ_CHUNK: usize = 64 * 1024;

#[derive(Debug)]
pub enum WireError {
    Io(io::Error),
    /// The peer broke the protocol: a frame too large or not a message, or
    /// descriptors that don't match what the messages declare.
    Protocol(String),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::Io(e) => write!(f, "{e}"),
            WireError::Protocol(e) => write!(f, "{e}"),
        }
    }
}

/// Reads frames from one connection, pairing each message with the descriptors it
/// declares (see `protocol::unix` for why a queue is enough).
pub struct FrameReader {
    read: OwnedReadHalf,
    buf: Vec<u8>,
    /// Where the next unparsed frame starts in `buf`.
    start: usize,
    fds: VecDeque<OwnedFd>,
}

impl FrameReader {
    pub fn new(read: OwnedReadHalf) -> FrameReader {
        FrameReader { read, buf: Vec::new(), start: 0, fds: VecDeque::new() }
    }

    /// The next message and its descriptors; `None` when the peer closes cleanly.
    pub async fn next(&mut self) -> Result<Option<(Message, Vec<OwnedFd>)>, WireError> {
        loop {
            if let Some(frame) = self.parse()? {
                return Ok(Some(frame));
            }
            if self.read_more().await? == 0 {
                return if self.start == self.buf.len() {
                    Ok(None)
                } else {
                    Err(WireError::Protocol("connection closed in the middle of a frame".into()))
                };
            }
        }
    }

    fn parse(&mut self) -> Result<Option<(Message, Vec<OwnedFd>)>, WireError> {
        let (msg, used) = match decode_frame(&self.buf[self.start..]) {
            Ok(frame) => frame,
            Err(FrameError::Incomplete) => return Ok(None),
            Err(FrameError::TooLarge(n)) => return Err(WireError::Protocol(format!("frame of {n} bytes"))),
            Err(FrameError::Json(e)) => return Err(WireError::Protocol(format!("bad message: {e}"))),
        };
        self.start += used;
        let n = msg.fds() as usize;
        if n > MAX_FDS_PER_MESSAGE {
            return Err(WireError::Protocol(format!(
                "{n} descriptors in one message (most {MAX_FDS_PER_MESSAGE})"
            )));
        }
        if self.fds.len() < n {
            return Err(WireError::Protocol(format!(
                "message declares {n} descriptors, {} arrived",
                self.fds.len()
            )));
        }
        Ok(Some((msg, self.fds.drain(..n).collect())))
    }

    async fn read_more(&mut self) -> Result<usize, WireError> {
        // Drop parsed bytes before growing the buffer.
        if self.start > 0 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        let filled = self.buf.len();
        self.buf.resize(filled + READ_CHUNK, 0);
        let stream: &UnixStream = self.read.as_ref();
        let mut received = Vec::new();
        let result = loop {
            if let Err(e) = stream.readable().await {
                break Err(e);
            }
            let buf = &mut self.buf[filled..];
            match stream
                .try_io(Interest::READABLE, || unix::recv_with_fds(stream.as_fd(), buf, &mut received))
            {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                other => break other,
            }
        };
        self.fds.extend(received);
        let n = match result {
            Ok(n) => n,
            Err(e) => {
                self.buf.truncate(filled);
                return Err(WireError::Io(e));
            }
        };
        self.buf.truncate(filled + n);
        // Descriptors nobody claims would pile up; a well-behaved peer never sends
        // more than the next few messages need.
        if self.fds.len() > 2 * MAX_FDS_PER_SEND {
            return Err(WireError::Protocol(format!("{} descriptors no message claimed", self.fds.len())));
        }
        Ok(n)
    }
}

/// Write `bytes` in full, sending `fds` with the first part.
pub async fn write_with_fds(write: &OwnedWriteHalf, bytes: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<()> {
    let stream: &UnixStream = write.as_ref();
    let mut sent = 0;
    while sent < bytes.len() {
        stream.writable().await?;
        let attach = if sent == 0 { fds } else { &[] };
        match stream
            .try_io(Interest::WRITABLE, || unix::send_with_fds(stream.as_fd(), &bytes[sent..], attach))
        {
            Ok(n) => sent += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
