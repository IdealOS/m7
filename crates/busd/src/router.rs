//! The broker's routing rules, as a synchronous state machine.
//!
//! [`Router::handle`] takes one [`Event`] (a connection opened or closed, a message
//! arrived, the clock moved) and appends the resulting [`Action`]s. It does no I/O
//! and reads no clock, so every rule here is tested without sockets or sleeps;
//! `server.rs` is the thin async shell that feeds it.
//!
//! Requests are addressed to names. The router gives each forwarded request its own
//! id, so two clients can both use id 1, and maps the service's response back to
//! the requester's id. A request ends exactly once: with the service's response, a
//! `timeout` or `service_gone` error from the broker, or the requester's cancel.
//!
//! File descriptors never pass through the router. It only carries each message's
//! `fds` count to the message it forwards; the shell sends the descriptors that
//! arrived with a message along with whichever action declares them.

use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

use protocol::broker::{
    AdoptApp, AdoptParams, AdoptResult, AdoptSystem, HelloResult, ListResult, NameParams, SystemTrust,
    TapEvent, method,
};
use protocol::bus::{
    self, BROKER, BusError, DEFAULT_TIMEOUT_MS, MAX_NAMES, MAX_PENDING_REQUESTS, MAX_SUBSCRIPTIONS,
    MAX_TIMEOUT_MS, Message, Sender, TAP_TOPIC, code,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

pub type ConnId = u64;

/// How much a connection is trusted, fixed when it connects.
#[derive(Debug, Clone, PartialEq)]
pub enum Trust {
    /// Reached the broker's socket as the broker's own user (or root): a system
    /// service or a developer tool. May claim names and tap the bus.
    System,
    /// An app's connection, handed over by `session`. May not claim names or tap.
    App { app: String },
    /// The connection `session` holds, which `busd` inherited at startup. Trusted
    /// like `System`, and the only one that may hand the broker new connections.
    Session,
}

#[derive(Debug)]
pub enum Event {
    Opened {
        conn: ConnId,
        trust: Trust,
        /// For a connection session handed over: the `adopt` request to answer.
        adopted_for: Option<(ConnId, u64)>,
    },
    Message {
        conn: ConnId,
        msg: Message,
    },
    Closed {
        conn: ConnId,
    },
    /// The clock reached [`Router::next_deadline`]; expire overdue requests.
    Tick,
}

#[derive(Debug, PartialEq)]
pub enum Action {
    /// Send `msg` to each connection in `to`.
    Deliver { to: Vec<ConnId>, msg: Message },
    /// Start a connection on the descriptor that came with request `id` from
    /// `requester`, then report back with [`Event::Opened`]'s `adopted_for`.
    Adopt { requester: ConnId, id: u64, trust: Trust },
}

/// Decides whether a message may be delivered. Every request (other than to the
/// broker itself), publish and subscribe passes through it. Until permissions
/// exist (M6) the broker runs with [`AllowAll`], so nothing assumes unchecked access.
pub trait Policy: Send {
    fn check(&self, sender: &Sender, msg: &Message) -> Result<(), BusError>;
}

pub struct AllowAll;

impl Policy for AllowAll {
    fn check(&self, _: &Sender, _: &Message) -> Result<(), BusError> {
        Ok(())
    }
}

struct Conn {
    trust: Trust,
    /// Names this connection claimed, oldest first; the first is how it's stamped.
    names: Vec<String>,
    subs: Vec<String>,
    /// Requests this connection sent that are still waiting for an answer.
    waiting: usize,
}

struct Pending {
    requester: ConnId,
    /// The id the requester used.
    client_id: u64,
    service: ConnId,
    deadline: Instant,
}

pub struct Router {
    policy: Box<dyn Policy>,
    conns: HashMap<ConnId, Conn>,
    names: HashMap<String, ConnId>,
    /// Keyed by the id the broker gave the forwarded request.
    pending: HashMap<u64, Pending>,
    /// (requester, client id) to broker id, for cancels.
    by_client: HashMap<(ConnId, u64), u64>,
    deadlines: BTreeSet<(Instant, u64)>,
    next_id: u64,
}

impl Router {
    pub fn new(policy: Box<dyn Policy>) -> Router {
        Router {
            policy,
            conns: HashMap::new(),
            names: HashMap::new(),
            pending: HashMap::new(),
            by_client: HashMap::new(),
            deadlines: BTreeSet::new(),
            next_id: 1,
        }
    }

    /// When the next request times out, if any is pending.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.deadlines.first().map(|(t, _)| *t)
    }

    pub fn handle(&mut self, now: Instant, event: Event, out: &mut Vec<Action>) {
        match event {
            Event::Opened { conn, trust, adopted_for } => {
                self.conns.insert(conn, Conn { trust, names: Vec::new(), subs: Vec::new(), waiting: 0 });
                if let Some((requester, id)) = adopted_for {
                    out.push(Action::Deliver {
                        to: vec![requester],
                        msg: Message::ok(id, to_value(AdoptResult { conn })),
                    });
                }
            }
            Event::Message { conn, msg } => {
                if let Some(sender) = self.sender(conn) {
                    self.tap(conn, &sender, &msg, out);
                    self.message(now, conn, sender, msg, out);
                }
            }
            Event::Closed { conn } => self.close(conn, out),
            Event::Tick => self.expire(now, out),
        }
    }

    /// How messages from `conn` are stamped.
    fn sender(&self, conn: ConnId) -> Option<Sender> {
        let c = self.conns.get(&conn)?;
        Some(match (&c.trust, c.names.first()) {
            (_, Some(name)) => Sender::Service(name.clone()),
            (Trust::App { app }, None) => Sender::App { app: app.clone(), conn },
            (Trust::System | Trust::Session, None) => Sender::Conn(conn),
        })
    }

    fn message(&mut self, now: Instant, conn: ConnId, sender: Sender, msg: Message, out: &mut Vec<Action>) {
        match msg {
            Message::Request { id, ref to, ref method, ref params, fds, .. } if to == BROKER => {
                if method == method::ADOPT {
                    return match self.adopt(conn, params, fds) {
                        Ok(trust) => out.push(Action::Adopt { requester: conn, id, trust }),
                        Err(e) => out.push(Action::Deliver { to: vec![conn], msg: Message::failed(id, e) }),
                    };
                }
                let reply = self.broker(conn, &sender, id, method, params);
                out.push(Action::Deliver { to: vec![conn], msg: reply });
            }
            Message::Request { id, to, method, params, timeout_ms, fds, .. } => {
                let mut reply = |msg| out.push(Action::Deliver { to: vec![conn], msg });
                let Some(&service) = self.names.get(&to) else {
                    return reply(Message::err(id, code::NO_SUCH_SERVICE, format!("no service named {to}")));
                };
                if self.by_client.contains_key(&(conn, id)) {
                    return reply(Message::err(
                        id,
                        code::BAD_REQUEST,
                        format!("request {id} is already pending"),
                    ));
                }
                if self.conns.get(&conn).is_some_and(|c| c.waiting >= MAX_PENDING_REQUESTS) {
                    return reply(Message::err(
                        id,
                        code::LIMIT_EXCEEDED,
                        format!("more than {MAX_PENDING_REQUESTS} requests waiting for answers"),
                    ));
                }
                let forwarded = Message::Request {
                    id: self.next_id,
                    to,
                    method,
                    params,
                    timeout_ms,
                    fds,
                    from: Some(sender.clone()),
                };
                if let Err(e) = self.policy.check(&sender, &forwarded) {
                    return reply(Message::failed(id, e));
                }
                let bus_id = self.next_id;
                self.next_id += 1;
                // Clamped, so a request can't hold its slot (and the service) forever,
                // and the deadline stays far from the clock's limits.
                let wait = timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).min(MAX_TIMEOUT_MS);
                let deadline = now + Duration::from_millis(wait);
                self.pending.insert(bus_id, Pending { requester: conn, client_id: id, service, deadline });
                self.conns.get_mut(&conn).expect("sender exists").waiting += 1;
                self.by_client.insert((conn, id), bus_id);
                self.deadlines.insert((deadline, bus_id));
                out.push(Action::Deliver { to: vec![service], msg: forwarded });
            }
            Message::Response { id, ok, result, error, fds } => {
                // Only the service a request went to may answer it; anything else
                // (a late answer after a timeout or cancel, or a stray id) is dropped.
                if self.pending.get(&id).is_some_and(|p| p.service == conn) {
                    let p = self.finish(id);
                    out.push(Action::Deliver {
                        to: vec![p.requester],
                        msg: Message::Response { id: p.client_id, ok, result, error, fds },
                    });
                }
            }
            Message::Cancel { id } => {
                if let Some(bus_id) = self.by_client.get(&(conn, id)).copied() {
                    let p = self.finish(bus_id);
                    out.push(Action::Deliver { to: vec![p.service], msg: Message::Cancel { id: bus_id } });
                }
            }
            Message::Subscribe { id, ref topic } => {
                let reply = if !bus::valid_pattern(topic) {
                    Message::err(id, code::BAD_REQUEST, format!("not a topic pattern: {topic:?}"))
                } else if topic.starts_with("bus") && !self.trusted(conn) {
                    Message::err(id, code::PERMISSION_DENIED, "only system connections may tap the bus")
                } else if let Err(e) = self.policy.check(&sender, &msg) {
                    Message::failed(id, e)
                } else {
                    let subs = &mut self.conns.get_mut(&conn).expect("sender exists").subs;
                    if subs.contains(topic) {
                        Message::ok(id, Value::Null)
                    } else if subs.len() >= MAX_SUBSCRIPTIONS {
                        Message::err(
                            id,
                            code::LIMIT_EXCEEDED,
                            format!("more than {MAX_SUBSCRIPTIONS} subscriptions"),
                        )
                    } else {
                        subs.push(topic.clone());
                        Message::ok(id, Value::Null)
                    }
                };
                out.push(Action::Deliver { to: vec![conn], msg: reply });
            }
            Message::Unsubscribe { id, topic } => {
                self.conns.get_mut(&conn).expect("sender exists").subs.retain(|t| *t != topic);
                out.push(Action::Deliver { to: vec![conn], msg: Message::ok(id, Value::Null) });
            }
            Message::Publish { topic, data, fds, .. } => {
                // A publish has no id to answer with, so a bad one is only logged.
                if !bus::valid_topic(&topic) || topic.starts_with("bus.") {
                    log::warn!(
                        "bus: dropped publish to {topic:?} from {sender:?}: not a topic clients may use"
                    );
                    return;
                }
                let msg = Message::Publish { topic, data, fds, from: Some(sender.clone()) };
                if let Err(e) = self.policy.check(&sender, &msg) {
                    log::warn!("bus: dropped publish from {sender:?}: {}", e.message);
                    return;
                }
                let Message::Publish { topic, .. } = &msg else { unreachable!() };
                let to = self.subscribers(topic);
                if !to.is_empty() {
                    out.push(Action::Deliver { to, msg });
                }
            }
        }
    }

    /// Check an `adopt` request: only session may hand over connections, with
    /// exactly one descriptor, as `{"trust": "system"}` or `{"app": "<app id>"}`.
    fn adopt(&self, conn: ConnId, params: &Value, fds: u32) -> Result<Trust, BusError> {
        let refuse = |code: &str, message: &str| BusError { code: code.into(), message: message.into() };
        if self.conns.get(&conn).map(|c| &c.trust) != Some(&Trust::Session) {
            return Err(refuse(code::PERMISSION_DENIED, "only session may hand over connections"));
        }
        if fds != 1 {
            return Err(refuse(code::BAD_REQUEST, "adopt takes exactly one descriptor"));
        }
        match parse::<AdoptParams>(params) {
            Some(AdoptParams::App(AdoptApp { app })) if valid_app_id(&app) => Ok(Trust::App { app }),
            Some(AdoptParams::System(AdoptSystem { trust: SystemTrust::System })) => Ok(Trust::System),
            _ => Err(refuse(
                code::BAD_REQUEST,
                r#"adopt needs {"app": "<reverse-DNS id>"} or {"trust": "system"}"#,
            )),
        }
    }

    /// The broker's own methods.
    fn broker(&mut self, conn: ConnId, sender: &Sender, id: u64, method: &str, params: &Value) -> Message {
        let name = parse::<NameParams>(params).map(|p| p.name);
        let name = name.as_deref();
        match method {
            method::HELLO => Message::ok(id, to_value(HelloResult { conn, sender: sender.clone() })),
            method::CLAIM => {
                let Some(name) = name.filter(|n| valid_name(n)) else {
                    return Message::err(
                        id,
                        code::BAD_REQUEST,
                        "claim needs a name: lowercase letters, digits, - and .",
                    );
                };
                if !self.trusted(conn) {
                    return Message::err(id, code::PERMISSION_DENIED, "apps may not claim service names");
                }
                match self.names.get(name) {
                    Some(&owner) if owner == conn => Message::ok(id, Value::Null),
                    Some(&owner) => {
                        Message::err(id, code::NAME_TAKEN, format!("{name} is held by connection {owner}"))
                    }
                    None => {
                        let names = &mut self.conns.get_mut(&conn).expect("sender exists").names;
                        if names.len() >= MAX_NAMES {
                            return Message::err(
                                id,
                                code::LIMIT_EXCEEDED,
                                format!("more than {MAX_NAMES} names"),
                            );
                        }
                        names.push(name.to_owned());
                        self.names.insert(name.to_owned(), conn);
                        Message::ok(id, Value::Null)
                    }
                }
            }
            method::RELEASE => {
                let Some(name) = name else {
                    return Message::err(id, code::BAD_REQUEST, "release needs a name");
                };
                if self.names.get(name) == Some(&conn) {
                    self.names.remove(name);
                    self.conns.get_mut(&conn).expect("sender exists").names.retain(|n| n != name);
                }
                Message::ok(id, Value::Null)
            }
            method::LIST => {
                let services = self.names.iter().map(|(n, c)| (n.clone(), *c)).collect();
                Message::ok(id, to_value(ListResult { services, connections: self.conns.len() as u64 }))
            }
            other => Message::err(id, code::BAD_REQUEST, format!("the broker has no method {other:?}")),
        }
    }

    fn trusted(&self, conn: ConnId) -> bool {
        self.conns.get(&conn).is_some_and(|c| matches!(c.trust, Trust::System | Trust::Session))
    }

    /// Connections with a subscription matching `topic`, each once.
    fn subscribers(&self, topic: &str) -> Vec<ConnId> {
        let mut to: Vec<ConnId> = self
            .conns
            .iter()
            .filter(|(_, c)| c.subs.iter().any(|p| bus::topic_matches(p, topic)))
            .map(|(&id, _)| id)
            .collect();
        to.sort_unstable(); // deterministic order for tests and traces
        to
    }

    /// Publish what `conn` sent on the tap topic, if anyone is listening.
    fn tap(&self, conn: ConnId, sender: &Sender, msg: &Message, out: &mut Vec<Action>) {
        let to = self.subscribers(TAP_TOPIC);
        if to.is_empty() {
            return;
        }
        // For a request that will be forwarded: the id the service will see.
        let bus_id = match msg {
            Message::Request { to: name, .. } if self.names.contains_key(name) => Some(self.next_id),
            _ => None,
        };
        let event = TapEvent { conn, sender: sender.clone(), msg: msg.clone(), bus_id };
        out.push(Action::Deliver {
            to,
            msg: Message::Publish {
                topic: TAP_TOPIC.into(),
                data: to_value(event),
                fds: 0,
                from: Some(Sender::Service(BROKER.into())),
            },
        });
    }

    /// Forget pending request `bus_id` everywhere.
    fn finish(&mut self, bus_id: u64) -> Pending {
        let p = self.pending.remove(&bus_id).expect("caller checked");
        self.by_client.remove(&(p.requester, p.client_id));
        // The requester is already gone when its own close ends the request.
        if let Some(c) = self.conns.get_mut(&p.requester) {
            c.waiting -= 1;
        }
        self.deadlines.remove(&(p.deadline, bus_id));
        p
    }

    fn expire(&mut self, now: Instant, out: &mut Vec<Action>) {
        while let Some(&(deadline, bus_id)) = self.deadlines.first()
            && deadline <= now
        {
            let p = self.finish(bus_id);
            out.push(Action::Deliver {
                to: vec![p.requester],
                msg: Message::err(p.client_id, code::TIMEOUT, "the service didn't answer in time"),
            });
            // The service may still be working on it; tell it to stop.
            out.push(Action::Deliver { to: vec![p.service], msg: Message::Cancel { id: bus_id } });
        }
    }

    fn close(&mut self, conn: ConnId, out: &mut Vec<Action>) {
        let Some(c) = self.conns.remove(&conn) else { return };
        for name in &c.names {
            self.names.remove(name);
        }
        let mut ended: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, p)| p.requester == conn || p.service == conn)
            .map(|(&id, _)| id)
            .collect();
        ended.sort_unstable();
        for bus_id in ended {
            let p = self.finish(bus_id);
            if p.service == conn && p.requester != conn {
                out.push(Action::Deliver {
                    to: vec![p.requester],
                    msg: Message::err(p.client_id, code::SERVICE_GONE, "the service disconnected"),
                });
            } else if p.requester == conn && p.service != conn {
                out.push(Action::Deliver { to: vec![p.service], msg: Message::Cancel { id: bus_id } });
            }
        }
    }
}

/// Params as `T`, or `None` if they don't have its shape.
fn parse<T: DeserializeOwned>(params: &Value) -> Option<T> {
    T::deserialize(params).ok()
}

fn to_value(v: impl Serialize) -> Value {
    serde_json::to_value(v).expect("broker types serialize")
}

/// App ids are reverse-DNS names such as `org.idealos.mail`: at least two
/// dot-separated segments of letters, digits and `-`.
fn valid_app_id(id: &str) -> bool {
    id.split('.').count() >= 2
        && id.split('.').all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

/// Service names: non-empty, lowercase letters, digits, `-` and `.`, and not the
/// broker's own name.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != BROKER
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Harness {
        router: Router,
        now: Instant,
    }

    impl Harness {
        fn new() -> Harness {
            Harness { router: Router::new(Box::new(AllowAll)), now: Instant::now() }
        }

        fn open(&mut self, conn: ConnId, trust: Trust) {
            assert!(self.run(Event::Opened { conn, trust, adopted_for: None }).is_empty());
        }

        fn run(&mut self, event: Event) -> Vec<Action> {
            let mut out = Vec::new();
            self.router.handle(self.now, event, &mut out);
            out
        }

        fn send(&mut self, conn: ConnId, msg: Message) -> Vec<Action> {
            self.run(Event::Message { conn, msg })
        }

        fn request(&mut self, conn: ConnId, id: u64, to: &str, method: &str, params: Value) -> Vec<Action> {
            self.send(
                conn,
                Message::Request {
                    id,
                    to: to.into(),
                    method: method.into(),
                    params,
                    timeout_ms: None,
                    fds: 0,
                    from: None,
                },
            )
        }

        /// Claim `name` for `conn` and check it worked.
        fn claim(&mut self, conn: ConnId, name: &str) {
            let out = self.request(conn, 999, BROKER, "claim", json!({"name": name}));
            assert_eq!(out, [deliver(conn, Message::ok(999, Value::Null))]);
        }

        fn advance(&mut self, by: Duration) -> Vec<Action> {
            self.now += by;
            self.run(Event::Tick)
        }
    }

    fn deliver(to: ConnId, msg: Message) -> Action {
        Action::Deliver { to: vec![to], msg }
    }

    fn app(name: &str) -> Trust {
        Trust::App { app: name.into() }
    }

    fn error_code(action: &Action) -> &str {
        match action {
            Action::Deliver { msg: Message::Response { ok: false, error: Some(e), .. }, .. } => &e.code,
            other => panic!("not an error response: {other:?}"),
        }
    }

    /// The service sees its own id and the stamped sender; the requester gets the
    /// answer under its own id.
    #[test]
    fn requests_route_by_name_and_answers_come_back_under_the_callers_id() {
        let mut h = Harness::new();
        h.open(1, Trust::System);
        h.open(2, app("org.example.notes"));
        h.claim(1, "db");

        let out = h.request(2, 42, "db", "query", json!({"type": "Note"}));
        let [Action::Deliver { to, msg: Message::Request { id: bus_id, from, params, .. } }] = &out[..]
        else {
            panic!("{out:?}")
        };
        assert_eq!(to, &[1]);
        assert_eq!(from, &Some(Sender::App { app: "org.example.notes".into(), conn: 2 }));
        assert_eq!(params, &json!({"type": "Note"}));

        let out = h.send(1, Message::ok(*bus_id, json!({"docs": []})));
        assert_eq!(out, [deliver(2, Message::ok(42, json!({"docs": []})))]);
        assert!(h.router.next_deadline().is_none(), "nothing is left pending");
    }

    /// A client can't keep a request (and its slot) for longer than an hour.
    #[test]
    fn huge_timeouts_are_clamped() {
        let mut h = Harness::new();
        h.open(1, Trust::System);
        h.open(2, app("org.example.notes"));
        h.claim(1, "db");
        let mut huge = Message::request(7, "db", "slow", Value::Null);
        if let Message::Request { timeout_ms, .. } = &mut huge {
            *timeout_ms = Some(u64::MAX);
        }
        assert_eq!(h.send(2, huge).len(), 1, "forwarded");
        assert_eq!(h.router.next_deadline(), Some(h.now + Duration::from_millis(MAX_TIMEOUT_MS)));
        let out = h.advance(Duration::from_millis(MAX_TIMEOUT_MS));
        assert_eq!(error_code(&out[0]), code::TIMEOUT);
    }

    #[test]
    fn pending_requests_are_limited_per_connection() {
        let mut h = Harness::new();
        h.open(1, Trust::System);
        h.open(2, app("org.example.notes"));
        h.open(3, app("org.example.other"));
        h.claim(1, "db");
        let mut forwarded = Vec::new();
        for id in 0..MAX_PENDING_REQUESTS as u64 {
            let out = h.request(2, id, "db", "q", Value::Null);
            let [Action::Deliver { msg: Message::Request { id: bus_id, .. }, .. }] = &out[..] else {
                panic!("{out:?}")
            };
            forwarded.push(*bus_id);
        }
        let out = h.request(2, 5000, "db", "q", Value::Null);
        assert_eq!(out.len(), 1);
        assert_eq!(error_code(&out[0]), code::LIMIT_EXCEEDED);
        // The limit is per connection: another app still gets through.
        assert!(
            matches!(&h.request(3, 1, "db", "q", Value::Null)[..], [Action::Deliver { to, .. }] if to == &[1])
        );

        // Every way a request ends frees its slot: an answer, a cancel, a timeout.
        h.send(1, Message::ok(forwarded[0], Value::Null));
        assert!(
            matches!(&h.request(2, 5001, "db", "q", Value::Null)[..], [Action::Deliver { to, .. }] if to == &[1])
        );
        h.send(2, Message::Cancel { id: 1 });
        assert!(
            matches!(&h.request(2, 5002, "db", "q", Value::Null)[..], [Action::Deliver { to, .. }] if to == &[1])
        );
        h.advance(Duration::from_millis(DEFAULT_TIMEOUT_MS));
        for id in 0..MAX_PENDING_REQUESTS as u64 {
            let out = h.request(2, 10_000 + id, "db", "q", Value::Null);
            assert!(matches!(&out[..], [Action::Deliver { to, .. }] if to == &[1]), "{id}: {out:?}");
        }
    }

    #[test]
    fn subscriptions_and_names_are_limited_per_connection() {
        let mut h = Harness::new();
        h.open(1, Trust::System);
        for i in 0..MAX_SUBSCRIPTIONS {
            let out = h.send(1, Message::Subscribe { id: i as u64, topic: format!("t.{i}") });
            assert_eq!(out, [deliver(1, Message::ok(i as u64, Value::Null))]);
        }
        // Subscribing again to a held pattern is still fine; a new one isn't.
        let out = h.send(1, Message::Subscribe { id: 1, topic: "t.0".into() });
        assert_eq!(out, [deliver(1, Message::ok(1, Value::Null))]);
        let out = h.send(1, Message::Subscribe { id: 2, topic: "t.new".into() });
        assert_eq!(error_code(&out[0]), code::LIMIT_EXCEEDED);
        h.send(1, Message::Unsubscribe { id: 3, topic: "t.0".into() });
        let out = h.send(1, Message::Subscribe { id: 4, topic: "t.new".into() });
        assert_eq!(out, [deliver(1, Message::ok(4, Value::Null))]);

        for i in 0..MAX_NAMES {
            h.claim(1, &format!("svc-{i}"));
        }
        h.claim(1, "svc-0"); // already held
        let out = h.request(1, 9, BROKER, "claim", json!({"name": "one-more"}));
        assert_eq!(error_code(&out[0]), code::LIMIT_EXCEEDED);
        assert!(!h.router.names.contains_key("one-more"));
    }

    #[test]
    fn two_clients_may_use_the_same_request_id() {
        let mut h = Harness::new();
        h.open(1, Trust::System);
        h.open(2, Trust::System);
        h.open(3, Trust::System);
        h.claim(1, "echo");
        let id_of = |out: Vec<Action>| match &out[..] {
            [Action::Deliver { msg: Message::Request { id, .. }, .. }] => *id,
            other => panic!("{other:?}"),
        };
        let a = id_of(h.request(2, 1, "echo", "x", Value::Null));
        let b = id_of(h.request(3, 1, "echo", "x", Value::Null));
        assert_ne!(a, b);
        assert_eq!(h.send(1, Message::ok(b, json!("b"))), [deliver(3, Message::ok(1, json!("b")))]);
        assert_eq!(h.send(1, Message::ok(a, json!("a"))), [deliver(2, Message::ok(1, json!("a")))]);
    }

    #[test]
    fn a_client_supplied_from_is_replaced() {
        let mut h = Harness::new();
        h.open(1, Trust::System);
        h.open(2, app("evil"));
        h.claim(1, "db");
        let forged = Message::Request {
            id: 1,
            to: "db".into(),
            method: "drop".into(),
            params: Value::Null,
            timeout_ms: None,
            fds: 0,
            from: Some(Sender::Service("permd".into())),
        };
        let out = h.send(2, forged);
        let [Action::Deliver { msg: Message::Request { from, .. }, .. }] = &out[..] else {
            panic!("{out:?}")
        };
        assert_eq!(from, &Some(Sender::App { app: "evil".into(), conn: 2 }));
    }

    #[test]
    fn only_the_service_asked_may_answer() {
        let mut h = Harness::new();
        h.open(1, Trust::System);
        h.open(2, Trust::System);
        h.open(3, Trust::System);
        h.claim(1, "db");
        let out = h.request(2, 5, "db", "query", Value::Null);
        let [Action::Deliver { msg: Message::Request { id: bus_id, .. }, .. }] = &out[..] else { panic!() };
        assert!(h.send(3, Message::ok(*bus_id, json!("forged"))).is_empty());
        assert_eq!(h.send(1, Message::ok(*bus_id, json!("real"))).len(), 1);
    }

    #[test]
    fn unknown_services_and_duplicate_ids_are_errors() {
        let mut h = Harness::new();
        h.open(1, Trust::System);
        h.open(2, Trust::System);
        let out = h.request(2, 1, "nobody", "x", Value::Null);
        assert_eq!(error_code(&out[0]), code::NO_SUCH_SERVICE);
        h.claim(1, "db");
        h.request(2, 1, "db", "x", Value::Null);
        let out = h.request(2, 1, "db", "x", Value::Null);
        assert_eq!(error_code(&out[0]), code::BAD_REQUEST);
    }

    #[test]
    fn requests_time_out_and_the_service_is_told_to_stop() {
        let mut h = Harness::new();
        h.open(1, Trust::System);
        h.open(2, Trust::System);
        h.claim(1, "slow");
        let req = |h: &mut Harness, id, timeout_ms| {
            let msg = Message::Request {
                id,
                to: "slow".into(),
                method: "x".into(),
                params: Value::Null,
                timeout_ms,
                fds: 0,
                from: None,
            };
            match &h.send(2, msg)[..] {
                [Action::Deliver { msg: Message::Request { id, .. }, .. }] => *id,
                other => panic!("{other:?}"),
            }
        };
        let short = req(&mut h, 1, Some(100));
        let default = req(&mut h, 2, None);
        assert_eq!(h.router.next_deadline(), Some(h.now + Duration::from_millis(100)));

        assert!(h.advance(Duration::from_millis(99)).is_empty());
        let out = h.advance(Duration::from_millis(1));
        assert_eq!(out.len(), 2);
        assert_eq!(error_code(&out[0]), code::TIMEOUT);
        assert_eq!(out[1], deliver(1, Message::Cancel { id: short }));
        // A late answer goes nowhere.
        assert!(h.send(1, Message::ok(short, Value::Null)).is_empty());

        let out = h.advance(Duration::from_millis(DEFAULT_TIMEOUT_MS));
        assert_eq!(out[1], deliver(1, Message::Cancel { id: default }));
        assert!(h.router.next_deadline().is_none());
    }

    #[test]
    fn a_cancel_reaches_the_service_and_its_answer_is_dropped() {
        let mut h = Harness::new();
        h.open(1, Trust::System);
        h.open(2, Trust::System);
        h.claim(1, "db");
        let out = h.request(2, 8, "db", "query", Value::Null);
        let [Action::Deliver { msg: Message::Request { id: bus_id, .. }, .. }] = &out[..] else { panic!() };
        let bus_id = *bus_id;
        assert_eq!(h.send(2, Message::Cancel { id: 8 }), [deliver(1, Message::Cancel { id: bus_id })]);
        assert!(h.send(1, Message::ok(bus_id, Value::Null)).is_empty());
        // Cancelling something that isn't pending does nothing.
        assert!(h.send(2, Message::Cancel { id: 8 }).is_empty());
    }

    #[test]
    fn disconnects_end_pending_requests_both_ways() {
        let mut h = Harness::new();
        h.open(1, Trust::System);
        h.open(2, Trust::System);
        h.open(3, Trust::System);
        h.claim(1, "db");
        h.request(2, 1, "db", "x", Value::Null);
        let out = h.request(3, 1, "db", "x", Value::Null);
        let [Action::Deliver { msg: Message::Request { id: from_3, .. }, .. }] = &out[..] else { panic!() };
        let from_3 = *from_3;

        // The requester goes away: the service is told to stop.
        assert_eq!(h.run(Event::Closed { conn: 3 }), [deliver(1, Message::Cancel { id: from_3 })]);
        // The service goes away: the remaining requester gets an error, and the
        // name is free again.
        let out = h.run(Event::Closed { conn: 1 });
        assert_eq!(out.len(), 1);
        assert_eq!(error_code(&out[0]), code::SERVICE_GONE);
        assert!(h.router.next_deadline().is_none());
        h.open(4, Trust::System);
        h.claim(4, "db");
    }

    #[test]
    fn names_are_claimed_once_and_only_by_system_connections() {
        let mut h = Harness::new();
        h.open(1, Trust::System);
        h.open(2, Trust::System);
        h.open(3, app("org.example.app"));
        h.claim(1, "db");
        h.claim(1, "db"); // claiming your own name again is fine
        let claim =
            |h: &mut Harness, conn, name: &str| h.request(conn, 1, BROKER, "claim", json!({"name": name}));
        assert_eq!(error_code(&claim(&mut h, 2, "db")[0]), code::NAME_TAKEN);
        assert_eq!(error_code(&claim(&mut h, 3, "compositor")[0]), code::PERMISSION_DENIED);
        for bad in ["", "bus", "Db", "a b", "x*"] {
            assert_eq!(error_code(&claim(&mut h, 2, bad)[0]), code::BAD_REQUEST, "{bad:?}");
        }
        // After claiming, a connection is stamped with its first name.
        let out = h.request(1, 2, BROKER, "hello", Value::Null);
        assert_eq!(out, [deliver(1, Message::ok(2, json!({"conn": 1, "sender": {"service": "db"}})))]);

        let out = h.request(2, 3, BROKER, "release", json!({"name": "db"}));
        assert_eq!(out.len(), 1);
        let out = h.request(2, 4, BROKER, "list", Value::Null);
        assert_eq!(out, [deliver(2, Message::ok(4, json!({"services": {"db": 1}, "connections": 3})))]);
        h.request(1, 5, BROKER, "release", json!({"name": "db"}));
        h.claim(2, "db");
    }

    #[test]
    fn publishes_reach_matching_subscribers_once() {
        let mut h = Harness::new();
        for c in 1..=4 {
            h.open(c, Trust::System);
        }
        let sub = |h: &mut Harness, conn, topic: &str| {
            let out = h.send(conn, Message::Subscribe { id: 1, topic: topic.into() });
            assert_eq!(out, [deliver(conn, Message::ok(1, Value::Null))]);
        };
        sub(&mut h, 2, "db.live.7");
        sub(&mut h, 3, "db.live.*");
        sub(&mut h, 3, "db.*"); // two matching patterns, still one delivery
        sub(&mut h, 4, "ui.event");

        let out = h.send(1, Message::publish("db.live.7", json!({"added": []})));
        assert_eq!(
            out,
            [Action::Deliver {
                to: vec![2, 3],
                msg: Message::Publish {
                    topic: "db.live.7".into(),
                    data: json!({"added": []}),
                    fds: 0,
                    from: Some(Sender::Conn(1)),
                },
            }]
        );
        assert!(
            h.send(
                1,
                Message::Publish { topic: "nobody.listens".into(), data: Value::Null, fds: 0, from: None }
            )
            .is_empty()
        );

        h.send(3, Message::Unsubscribe { id: 2, topic: "db.live.*".into() });
        h.send(3, Message::Unsubscribe { id: 3, topic: "db.*".into() });
        let out =
            h.send(1, Message::Publish { topic: "db.live.7".into(), data: Value::Null, fds: 0, from: None });
        assert!(matches!(&out[..], [Action::Deliver { to, .. }] if to == &[2]));
    }

    #[test]
    fn bad_subscriptions_and_publishes_are_refused() {
        let mut h = Harness::new();
        h.open(1, Trust::System);
        h.open(2, app("org.example.app"));
        let out = h.send(1, Message::Subscribe { id: 1, topic: "a..b".into() });
        assert_eq!(error_code(&out[0]), code::BAD_REQUEST);
        let out = h.send(2, Message::Subscribe { id: 1, topic: TAP_TOPIC.into() });
        assert_eq!(error_code(&out[0]), code::PERMISSION_DENIED);
        // Clients can't publish on the broker's topics or to patterns.
        h.send(1, Message::Subscribe { id: 2, topic: TAP_TOPIC.into() });
        h.send(1, Message::Subscribe { id: 3, topic: "a.*".into() });
        for topic in ["bus.tap", "a.*"] {
            let out =
                h.send(2, Message::Publish { topic: topic.into(), data: Value::Null, fds: 0, from: None });
            // Only the tap's own trace of the attempt comes out.
            assert!(out.iter().all(|a| matches!(a, Action::Deliver { msg: Message::Publish { topic, .. }, .. } if topic == TAP_TOPIC)));
        }
    }

    #[test]
    fn the_tap_sees_every_message_with_its_sender() {
        let mut h = Harness::new();
        h.open(1, Trust::System);
        h.open(2, Trust::System);
        h.open(9, Trust::System);
        h.claim(1, "db");
        h.send(9, Message::Subscribe { id: 1, topic: TAP_TOPIC.into() });

        let out = h.request(2, 3, "db", "query", Value::Null);
        assert_eq!(out.len(), 2, "the trace, then the forwarded request");
        let Action::Deliver { to, msg: Message::Publish { topic, data, .. } } = &out[0] else {
            panic!("{out:?}")
        };
        assert_eq!((to.as_slice(), topic.as_str()), (&[9][..], TAP_TOPIC));
        assert_eq!(data["conn"], json!(2));
        assert_eq!(data["sender"], json!({"conn": 2}));
        assert_eq!(data["msg"]["method"], json!("query"));
        let Action::Deliver { msg: Message::Request { id: bus_id, .. }, .. } = &out[1] else { panic!() };
        assert_eq!(data["bus_id"], json!(bus_id));
    }

    /// Descriptor counts survive routing, so the shell knows which outgoing
    /// message the descriptors that came in belong to.
    #[test]
    fn descriptor_counts_follow_the_message() {
        let mut h = Harness::new();
        h.open(1, Trust::System);
        h.open(2, Trust::System);
        h.claim(1, "blobs");
        let out = h.send(2, Message::request(1, "blobs", "put", Value::Null).with_fds(2));
        let [Action::Deliver { msg: forwarded, .. }] = &out[..] else { panic!("{out:?}") };
        assert_eq!(forwarded.fds(), 2);
        let Message::Request { id: bus_id, .. } = forwarded else { panic!() };
        let out = h.send(1, Message::ok(*bus_id, Value::Null).with_fds(1));
        let [Action::Deliver { msg: answer, .. }] = &out[..] else { panic!("{out:?}") };
        assert_eq!(answer.fds(), 1);

        h.send(2, Message::Subscribe { id: 2, topic: "frames".into() });
        let out = h.send(1, Message::publish("frames", Value::Null).with_fds(1));
        let [Action::Deliver { msg: published, .. }] = &out[..] else { panic!("{out:?}") };
        assert_eq!(published.fds(), 1);
    }

    fn adopt(h: &mut Harness, conn: ConnId, params: Value, fds: u32) -> Vec<Action> {
        h.send(conn, Message::request(7, BROKER, "adopt", params).with_fds(fds))
    }

    #[test]
    fn session_hands_over_connections_with_their_trust() {
        let mut h = Harness::new();
        h.open(1, Trust::Session);
        let out = adopt(&mut h, 1, json!({"app": "org.example.notes"}), 1);
        assert_eq!(out, [Action::Adopt { requester: 1, id: 7, trust: app("org.example.notes") }]);
        let out = adopt(&mut h, 1, json!({"trust": "system"}), 1);
        assert_eq!(out, [Action::Adopt { requester: 1, id: 7, trust: Trust::System }]);

        // The shell starts the connection and reports it; session gets its number.
        let out =
            h.run(Event::Opened { conn: 5, trust: app("org.example.notes"), adopted_for: Some((1, 7)) });
        assert_eq!(out, [deliver(1, Message::ok(7, json!({"conn": 5})))]);
        let out = h.request(5, 1, BROKER, "hello", Value::Null);
        let sender = json!({"app": {"app": "org.example.notes", "conn": 5}});
        assert_eq!(out, [deliver(5, Message::ok(1, json!({"conn": 5, "sender": sender})))]);
        // Session itself is trusted like a system connection.
        h.claim(1, "session");
    }

    #[test]
    fn only_session_may_adopt_and_only_well_formed_requests() {
        let mut h = Harness::new();
        h.open(1, Trust::Session);
        h.open(2, Trust::System);
        h.open(3, app("org.example.app"));
        let refused = |out: Vec<Action>| error_code(&out[0]).to_owned();
        assert_eq!(refused(adopt(&mut h, 2, json!({"trust": "system"}), 1)), code::PERMISSION_DENIED);
        assert_eq!(refused(adopt(&mut h, 3, json!({"trust": "system"}), 1)), code::PERMISSION_DENIED);
        assert_eq!(refused(adopt(&mut h, 1, json!({"trust": "system"}), 0)), code::BAD_REQUEST);
        assert_eq!(refused(adopt(&mut h, 1, json!({"trust": "system"}), 2)), code::BAD_REQUEST);
        for bad in [
            json!({}),
            json!({"trust": "root"}),
            json!({"app": "notes"}),
            json!({"app": "a..b"}),
            json!({"app": "org.x", "trust": "system"}),
        ] {
            assert_eq!(refused(adopt(&mut h, 1, bad.clone(), 1)), code::BAD_REQUEST, "{bad}");
        }
    }

    /// A policy that refuses everything shows where the hook sits.
    #[test]
    fn the_policy_hook_sees_requests_publishes_and_subscribes() {
        struct DenyAll;
        impl Policy for DenyAll {
            fn check(&self, _: &Sender, _: &Message) -> Result<(), BusError> {
                Err(BusError { code: code::PERMISSION_DENIED.into(), message: "no".into() })
            }
        }
        let mut h = Harness::new();
        h.router = Router::new(Box::new(DenyAll));
        h.open(1, Trust::System);
        h.open(2, Trust::System);
        // The broker's own methods aren't checked, or nothing could claim a name.
        h.claim(1, "db");
        let out = h.request(2, 1, "db", "query", Value::Null);
        assert_eq!(error_code(&out[0]), code::PERMISSION_DENIED);
        let out = h.send(2, Message::Subscribe { id: 2, topic: "x".into() });
        assert_eq!(error_code(&out[0]), code::PERMISSION_DENIED);
        assert!(
            h.send(2, Message::Publish { topic: "x".into(), data: Value::Null, fds: 0, from: None })
                .is_empty()
        );
    }
}
