//! session's link to the bus broker, and the connections it hands out.
//!
//! Before starting `busd`, session makes a socket pair and gives one end to the
//! broker (`IDEALOS_BUS_SESSION_FD`); the broker treats that connection as session.
//! For each service marked `bus=system` or `bus=app:ID`, session makes another pair,
//! gives one end to the service (`IDEALOS_BUS_FD`) and hands the other to the broker
//! with an `adopt` request. The service's identity on the bus then comes from how
//! its connection was made, not from anything it says or from its pid.
//!
//! A handed connection belongs to one client process: when that client exits, the
//! connection ends (its socket is shut down, not just closed), so a service that
//! runs several bus programs one after another only gets the bus in the first.
//!
//! session is synchronous, so this speaks the bus protocol directly over a
//! non-blocking socket instead of using the async client. Requests to `session`
//! (launching apps) are collected by [`Link::poll`] for the main loop to answer.

use std::collections::HashMap;
use std::io::{self, Read};
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use protocol::broker::{AdoptParams, AdoptResult, NameParams, method};
use protocol::bus::{BROKER, BusError, FrameError, Message, Sender, decode_frame, encode_frame};
use protocol::unix::send_with_fds;
use serde::Deserialize;
use serde_json::Value;

/// How busd finds session's connection.
pub const SESSION_FD_ENV: &str = "IDEALOS_BUS_SESSION_FD";
/// How a service finds the connection session made for it.
pub const BUS_FD_ENV: &str = "IDEALOS_BUS_FD";

/// What a service is on the bus, from its `bus=` setting in session.conf.
#[derive(Debug, Clone, PartialEq)]
pub enum Role {
    /// No connection from session (it may still reach the socket itself).
    None,
    /// The broker itself: gets session's connection.
    Broker,
    /// A system service: may claim names.
    System,
    /// An app, identified on the bus by this id.
    App(String),
}

impl Role {
    pub fn parse(text: &str) -> Result<Role, String> {
        match text {
            "broker" => Ok(Role::Broker),
            "system" => Ok(Role::System),
            other => match other.strip_prefix("app:") {
                Some(id) if !id.is_empty() => Ok(Role::App(id.to_owned())),
                _ => Err(format!("bus= must be broker, system or app:ID, not {other}")),
            },
        }
    }

    /// Whether session hands this service a connection made through the broker.
    pub fn adopted(&self) -> bool {
        matches!(self, Role::System | Role::App(_))
    }
}

/// A descriptor a child should inherit, and the environment variable naming it.
pub struct Inherit {
    pub env: &'static str,
    pub fd: OwnedFd,
}

impl Inherit {
    pub fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// A request someone sent to `session`.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub id: u64,
    pub from: Option<Sender>,
    pub method: String,
    pub params: Value,
}

/// session's end of its connection to the broker.
pub struct Link {
    stream: UnixStream,
    next_id: u64,
    /// Requests waiting for an answer, described for the log.
    pending: HashMap<u64, String>,
    buf: Vec<u8>,
    /// Requests to session, waiting for the main loop.
    incoming: Vec<Request>,
}

impl Link {
    /// A new link and the broker's end of it, for the broker to inherit. Session
    /// claims the name `session`, so `osctl bus list` shows it.
    pub fn new() -> io::Result<(Link, Inherit)> {
        let (ours, theirs) = UnixStream::pair()?;
        ours.set_nonblocking(true)?;
        let mut link =
            Link { stream: ours, next_id: 1, pending: HashMap::new(), buf: Vec::new(), incoming: Vec::new() };
        let claim = to_value(NameParams { name: "session".into() });
        link.send(Message::request(0, BROKER, method::CLAIM, claim), None, "claim session")?;
        Ok((link, Inherit { env: SESSION_FD_ENV, fd: theirs.into() }))
    }

    /// Make a connection for service `name` with `role`: hand one end to the broker
    /// and return the other for the service to inherit.
    pub fn connection_for(&mut self, name: &str, role: &Role) -> io::Result<Inherit> {
        let params = match role {
            Role::System => AdoptParams::system(),
            Role::App(id) => AdoptParams::app(id),
            Role::None | Role::Broker => {
                return Err(io::Error::other("only system services and apps get one"));
            }
        };
        let (service_end, broker_end) = UnixStream::pair()?;
        let msg = Message::request(0, BROKER, method::ADOPT, to_value(params)).with_fds(1);
        self.send(msg, Some(broker_end.as_fd()), &format!("connection for {name}"))?;
        Ok(Inherit { env: BUS_FD_ENV, fd: service_end.into() })
    }

    /// Requests to session that arrived since the last call.
    pub fn take_requests(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.incoming)
    }

    /// Answer request `id` from [`Link::take_requests`].
    pub fn respond(&mut self, id: u64, result: Result<Value, BusError>) -> io::Result<()> {
        let msg = match result {
            Ok(v) => Message::ok(id, v),
            Err(e) => Message::failed(id, e),
        };
        self.write(&msg, &[])
    }

    fn send(&mut self, mut msg: Message, fd: Option<std::os::fd::BorrowedFd>, what: &str) -> io::Result<()> {
        let id = self.next_id;
        self.next_id += 1;
        if let Message::Request { id: slot, .. } = &mut msg {
            *slot = id;
        }
        let fds: Vec<_> = fd.into_iter().collect();
        self.write(&msg, &fds)?;
        self.pending.insert(id, what.to_owned());
        Ok(())
    }

    fn write(&mut self, msg: &Message, fds: &[std::os::fd::BorrowedFd]) -> io::Result<()> {
        let frame = encode_frame(msg).map_err(io::Error::other)?;
        let mut sent = 0;
        // The broker reads these as soon as it starts; the socket buffer holds
        // them until then. Waiting is only for a broker that has stopped reading.
        let deadline = Instant::now() + Duration::from_secs(1);
        while sent < frame.len() {
            let attach = if sent == 0 { fds } else { &[] };
            match send_with_fds(self.stream.as_fd(), &frame[sent..], attach) {
                Ok(n) => sent += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Read the broker's answers and log the failures. Returns false once the
    /// broker has closed the connection.
    pub fn poll(&mut self) -> bool {
        let mut chunk = [0u8; 4096];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => return false,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    say!("reading from busd: {e}");
                    return false;
                }
            }
        }
        loop {
            match decode_frame(&self.buf) {
                Ok((msg, used)) => {
                    self.buf.drain(..used);
                    self.answer(msg);
                }
                Err(FrameError::Incomplete) => return true,
                Err(e) => {
                    say!("bad frame from busd: {e:?}");
                    return false;
                }
            }
        }
    }

    fn answer(&mut self, msg: Message) {
        if let Message::Request { id, from, method, params, .. } = msg {
            self.incoming.push(Request { id, from, method, params });
            return;
        }
        let Message::Response { id, ok, error, result, .. } = msg else { return };
        let what = self.pending.remove(&id).unwrap_or_else(|| format!("request {id}"));
        if ok {
            if let Some(AdoptResult { conn }) = result.and_then(|r| AdoptResult::deserialize(&r).ok()) {
                say!("busd took the {what} as connection {conn}");
            }
        } else if let Some(e) = error {
            say!("busd refused the {what}: {}: {}", e.code, e.message);
        }
    }
}

fn to_value(v: impl serde::Serialize) -> Value {
    serde_json::to_value(v).expect("broker params serialize")
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::unix::recv_with_fds;
    use serde_json::json;

    #[test]
    fn roles_parse() {
        assert_eq!(Role::parse("broker").unwrap(), Role::Broker);
        assert_eq!(Role::parse("system").unwrap(), Role::System);
        assert_eq!(Role::parse("app:org.example.notes").unwrap(), Role::App("org.example.notes".into()));
        for bad in ["", "app:", "root", "app"] {
            assert!(Role::parse(bad).is_err(), "{bad:?}");
        }
    }

    /// What a broker would read from session: a claim, then an adopt request whose
    /// descriptor is a working socket to the service's end.
    #[test]
    fn hands_the_broker_one_end_of_each_connection() {
        let (mut link, broker) = Link::new().unwrap();
        let broker = UnixStream::from(broker.fd);
        let service = link.connection_for("notes", &Role::App("org.example.notes".into())).unwrap();
        assert_eq!(service.env, BUS_FD_ENV);

        let mut buf = vec![0u8; 4096];
        let mut fds = Vec::new();
        let mut bytes = Vec::new();
        let mut messages = Vec::new();
        while messages.len() < 2 {
            let n = recv_with_fds(broker.as_fd(), &mut buf, &mut fds).unwrap();
            assert!(n > 0, "session's end closed");
            bytes.extend_from_slice(&buf[..n]);
            while let Ok((msg, used)) = decode_frame(&bytes) {
                bytes.drain(..used);
                messages.push(msg);
            }
        }
        assert!(matches!(&messages[0], Message::Request { method, .. } if method == "claim"));
        let Message::Request { method, params, .. } = &messages[1] else { panic!() };
        assert_eq!((method.as_str(), params), ("adopt", &json!({"app": "org.example.notes"})));
        assert_eq!(messages[1].fds(), 1);
        assert_eq!(fds.len(), 1);

        // The descriptor the broker got talks to the service's end.
        let mut to_service = UnixStream::from(fds.pop().unwrap());
        let mut service = UnixStream::from(service.fd);
        std::io::Write::write_all(&mut to_service, b"hi").unwrap();
        let mut got = [0u8; 2];
        service.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hi");

        // Answers are read and logged; the link stays up until the broker closes.
        let mut broker = broker;
        std::io::Write::write_all(&mut broker, &encode_frame(&Message::ok(2, json!({"conn": 3}))).unwrap())
            .unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert!(link.poll());
        assert!(!link.pending.contains_key(&2));
        drop(broker);
        assert!(!link.poll());
    }
}
