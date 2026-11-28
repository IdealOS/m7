//! Randomized tests of the router: many seeds of random events across a few
//! connections, checking the router's bookkeeping after every event, that every
//! forwarded request ends exactly once, and that stream frames reach only the
//! requester of an open streamed request, from the service it went to.
//!
//! Deterministic (a seeded generator, no dependencies), so a failure prints the
//! seed and step that reproduce it. `BUS_FUZZ_SEEDS=n` runs more seeds.

use super::*;
use std::collections::HashMap;

/// xorshift64*: small, fast and good enough to pick test inputs.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

const NAMES: &[&str] = &["db", "svc", "bus", "nope", "x.y", "Bad Name", ""];
const METHODS: &[&str] = &["hello", "claim", "release", "list", "adopt", "q", "hang"];
const TOPICS: &[&str] = &["a", "a.*", "*", "bus.tap", "bus.*", "", "a..b", "db.live.1", "a.b"];

fn random_params(rng: &mut Rng) -> Value {
    match rng.below(9) {
        0 => Value::Null,
        1 | 2 => serde_json::json!({"name": rng.pick(NAMES)}),
        3 => serde_json::json!({"app": "org.example.a"}),
        4 => serde_json::json!({"trust": "system"}),
        5 => serde_json::json!({"app": "bad"}),
        6 => serde_json::json!({"app": "org.a.b", "trust": "system"}),
        7 => serde_json::json!({"name": 5}),
        _ => serde_json::json!([1, 2, 3]),
    }
}

fn random_message(rng: &mut Rng, router: &Router) -> Message {
    let id = rng.below(8);
    match rng.below(11) {
        0..=3 => Message::Request {
            id,
            to: rng.pick(NAMES).to_string(),
            method: rng.pick(METHODS).to_string(),
            params: random_params(rng),
            timeout_ms: match rng.below(5) {
                0 => None,
                1 => Some(u64::MAX),
                2 => Some(0),
                _ => Some(rng.below(60_000)),
            },
            stream: rng.chance(30),
            fds: rng.below(3) as u32,
            // Clients may send anything here; the router must overwrite it.
            from: rng.chance(20).then(|| Sender::Service("forged".into())),
        },
        4 | 5 => {
            // Mostly answers to real pending requests, from whoever.
            let mut ids: Vec<u64> = router.pending.keys().copied().collect();
            ids.sort_unstable();
            let id = if !ids.is_empty() && rng.chance(80) { *rng.pick(&ids) } else { id };
            Message::Response {
                id,
                ok: rng.chance(50),
                result: Some(Value::Null),
                error: None,
                fds: rng.below(2) as u32,
            }
        }
        6 => Message::Cancel { id },
        10 => {
            // Mostly parts for real pending requests (streamed or not), from whoever.
            let mut ids: Vec<u64> = router.pending.keys().copied().collect();
            ids.sort_unstable();
            let id = if !ids.is_empty() && rng.chance(80) { *rng.pick(&ids) } else { id };
            Message::Stream { id, data: Value::Null, fds: 0 }
        }
        7 => Message::Subscribe { id, topic: rng.pick(TOPICS).to_string() },
        8 => Message::Unsubscribe { id, topic: rng.pick(TOPICS).to_string() },
        _ => Message::Publish {
            topic: rng.pick(TOPICS).to_string(),
            data: Value::Null,
            fds: rng.below(2) as u32,
            from: None,
        },
    }
}

/// A forwarded request that hasn't ended: where it went, the id the broker gave
/// it, and whether it's streamed.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Outstanding {
    service: ConnId,
    bus_id: u64,
    streamed: bool,
}

/// What the test expects from the outside: the requests the router forwarded
/// that haven't ended yet, keyed by (requester, the requester's id).
#[derive(Default)]
struct Model {
    outstanding: HashMap<(ConnId, u64), Outstanding>,
    /// `adopt` requests the router accepted, answered when the connection opens.
    /// (Counted: session may reuse an id while an earlier adopt is in flight.)
    adopting: HashMap<(ConnId, u64), usize>,
}

/// The router's tables agree with each other.
fn check_tables(r: &Router) -> Result<(), String> {
    let timed = r.pending.values().filter(|p| !p.streamed()).count();
    if r.by_client.len() != r.pending.len() || r.deadlines.len() != timed {
        return Err(format!(
            "{} pending ({timed} with timeouts), {} by client, {} deadlines",
            r.pending.len(),
            r.by_client.len(),
            r.deadlines.len()
        ));
    }
    for (bus_id, p) in &r.pending {
        if r.by_client.get(&(p.requester, p.client_id)) != Some(bus_id) {
            return Err(format!("request {bus_id} missing from by_client"));
        }
        if let Some(deadline) = p.deadline
            && !r.deadlines.contains(&(deadline, *bus_id))
        {
            return Err(format!("request {bus_id} has no deadline"));
        }
        if !r.conns.contains_key(&p.requester) || !r.conns.contains_key(&p.service) {
            return Err(format!("request {bus_id} outlived a connection"));
        }
    }
    for (id, c) in &r.conns {
        let mine = || r.pending.values().filter(|p| p.requester == *id);
        let waiting = mine().filter(|p| !p.streamed()).count();
        let streams = mine().filter(|p| p.streamed()).count();
        if c.waiting != waiting || c.streams != streams {
            return Err(format!(
                "conn {id} counts {} waiting and {} streams, has {waiting} and {streams}",
                c.waiting, c.streams
            ));
        }
        if c.waiting > MAX_PENDING_REQUESTS
            || c.streams > MAX_STREAMS
            || c.subs.len() > MAX_SUBSCRIPTIONS
            || c.names.len() > MAX_NAMES
        {
            return Err(format!("conn {id} is over a limit"));
        }
        for name in &c.names {
            if r.names.get(name) != Some(id) {
                return Err(format!("conn {id} lists {name} but doesn't hold it"));
            }
        }
    }
    for (name, owner) in &r.names {
        if !r.conns.get(owner).is_some_and(|c| c.names.contains(name)) {
            return Err(format!("{name} is held by {owner}, which doesn't list it"));
        }
    }
    Ok(())
}

/// Check what one event produced, and update the model.
fn check_actions(r: &Router, event: &Event, out: &[Action], model: &mut Model) -> Result<(), String> {
    let (sent_by, sent_id) = match event {
        Event::Message {
            conn,
            msg: Message::Request { id, .. } | Message::Subscribe { id, .. } | Message::Unsubscribe { id, .. },
        } => (Some(*conn), Some(*id)),
        Event::Message { conn, .. } => (Some(*conn), None),
        _ => (None, None),
    };
    if let Event::Closed { conn } = event {
        model.outstanding.retain(|(c, _), _| c != conn);
        model.adopting.retain(|(c, _), _| c != conn);
    }
    let adopted_for = match event {
        Event::Opened { adopted_for: Some(a), .. } => match model.adopting.get_mut(a) {
            Some(n) => {
                *n -= 1;
                if *n == 0 {
                    model.adopting.remove(a);
                }
                Some(*a)
            }
            None => None,
        },
        _ => None,
    };
    for action in out {
        if let Action::Adopt { requester, id, .. } = action {
            *model.adopting.entry((*requester, *id)).or_default() += 1;
        }
        let Action::Deliver { to, msg } = action else { continue };
        if to.is_empty() || to.iter().any(|c| !r.conns.contains_key(c)) {
            return Err(format!("delivery to {to:?}, not all connected"));
        }
        if let Message::Request { from, .. } | Message::Publish { from, .. } = msg
            && from.as_ref().is_some_and(|f| *f == Sender::Service("forged".into()))
        {
            return Err("a forged sender got through".into());
        }
        // A forwarded request (perhaps to a name the requester holds itself): the
        // requester now waits for exactly one answer.
        if let (Some(requester), Some(id), Message::Request { id: bus_id, stream, .. }) =
            (sent_by, sent_id, msg)
        {
            let forwarded = Outstanding { service: to[0], bus_id: *bus_id, streamed: *stream };
            if model.outstanding.insert((requester, id), forwarded).is_some() {
                return Err(format!("({requester}, {id}) forwarded twice"));
            }
        }
        // A part of a streamed answer: to the requester of an open streamed
        // request, sent by the service it went to, and the request stays open.
        if let Message::Stream { id, .. } = msg {
            let [conn] = to[..] else { return Err("a stream frame to several connections".into()) };
            let Some(o) = model.outstanding.get(&(conn, *id)) else {
                return Err(format!("stream frame {id} to {conn}, which has no such request open"));
            };
            let from_service = matches!(
                event,
                Event::Message { conn: from, msg: Message::Stream { id: sent, .. } }
                    if *from == o.service && *sent == o.bus_id
            );
            if !o.streamed || !from_service {
                return Err(format!("stream frame for request {} reached {conn} wrongly", o.bus_id));
            }
        }
        if let Message::Response { id, .. } = msg {
            let [conn] = to[..] else { return Err("a response to several connections".into()) };
            // An immediate reply to what this connection just sent, or the end of
            // an outstanding request.
            let immediate = sent_by == Some(conn) && sent_id == Some(*id) || adopted_for == Some((conn, *id));
            if immediate {
                continue;
            }
            let Some(Outstanding { service, bus_id, .. }) = model.outstanding.remove(&(conn, *id)) else {
                return Err(format!("response {id} to {conn}, which wasn't waiting for it"));
            };
            // Ended by the service's own answer, or by the broker (a timeout, or
            // the service going away), never by someone else's answer.
            if let Event::Message { conn: from, msg: Message::Response { id: answered, .. } } = event
                && (*from != service || *answered != bus_id)
            {
                return Err(format!("connection {from} answered request {bus_id}, sent to {service}"));
            }
        }
    }
    // A cancel ends a request without an answer.
    if let Event::Message { conn, msg: Message::Cancel { id } } = event {
        model.outstanding.remove(&(*conn, *id));
    }
    // Everything outstanding is still pending in the router, and vice versa.
    let pending: HashMap<(ConnId, u64), Outstanding> = r
        .pending
        .iter()
        .map(|(&bus_id, p)| {
            ((p.requester, p.client_id), Outstanding { service: p.service, bus_id, streamed: p.streamed() })
        })
        .collect();
    if pending != model.outstanding {
        return Err(format!("router has {pending:?}, model has {:?}", model.outstanding));
    }
    Ok(())
}

fn run_seed(seed: u64, steps: usize) {
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let mut router = Router::new(Box::new(AllowAll));
    let mut model = Model::default();
    let mut now = Instant::now();
    let mut adoptions: Vec<(ConnId, u64, Trust)> = Vec::new();
    // Connection numbers are never reused, as in busd.
    let mut next_conn = 6;
    let trusts = |c: ConnId| match c % 6 {
        1 => Trust::Session,
        2 | 3 => Trust::System,
        _ => Trust::App { app: format!("org.example.a{c}") },
    };
    let fail = |step: usize, event: &str, why: String| -> ! {
        panic!("seed {seed}, step {step}: after {event}: {why}");
    };
    for c in 1..=6 {
        router.handle(now, Event::Opened { conn: c, trust: trusts(c), adopted_for: None }, &mut Vec::new());
    }

    for step in 0..steps {
        let mut conns: Vec<ConnId> = router.conns.keys().copied().collect();
        conns.sort_unstable();
        let event = match rng.below(100) {
            0..=2 if !conns.is_empty() => Event::Closed { conn: *rng.pick(&conns) },
            3..=5 => {
                // A new connection, or one an adopt asked for. (Its requester may
                // have gone since; the router must cope.)
                next_conn += 1;
                match adoptions.pop() {
                    Some((requester, id, trust)) => {
                        Event::Opened { conn: next_conn, trust, adopted_for: Some((requester, id)) }
                    }
                    None => Event::Opened { conn: next_conn, trust: trusts(next_conn), adopted_for: None },
                }
            }
            6..=10 => {
                now += Duration::from_millis(match rng.below(10) {
                    0 => MAX_TIMEOUT_MS,
                    _ => rng.below(40_000),
                });
                Event::Tick
            }
            _ if !conns.is_empty() => {
                Event::Message { conn: *rng.pick(&conns), msg: random_message(&mut rng, &router) }
            }
            _ => continue,
        };
        let described = format!("{event:?}");
        let mut out = Vec::new();
        router.handle(now, event.clone(), &mut out);
        for action in &out {
            if let Action::Adopt { requester, id, trust } = action {
                adoptions.push((*requester, *id, trust.clone()));
            }
        }
        if let Err(why) = check_tables(&router) {
            fail(step, &described, why);
        }
        if let Err(why) = check_actions(&router, &event, &out, &mut model) {
            fail(step, &described, why);
        }
    }

    // Every request ends: after the longest timeout only streamed ones are left,
    // and closing every connection ends those.
    now += Duration::from_millis(MAX_TIMEOUT_MS + 1);
    let mut out = Vec::new();
    router.handle(now, Event::Tick, &mut out);
    if let Err(why) = check_actions(&router, &Event::Tick, &out, &mut model) {
        fail(steps, "the final tick", why);
    }
    if !router.pending.values().all(Pending::streamed) {
        fail(steps, "the final tick", "a request with a timeout outlived it".into());
    }
    let mut conns: Vec<ConnId> = router.conns.keys().copied().collect();
    conns.sort_unstable();
    for conn in conns {
        let event = Event::Closed { conn };
        let mut out = Vec::new();
        router.handle(now, event.clone(), &mut out);
        if let Err(why) = check_tables(&router).and_then(|_| check_actions(&router, &event, &out, &mut model))
        {
            fail(steps, &format!("{event:?}"), why);
        }
    }
    assert!(router.pending.is_empty() && model.outstanding.is_empty(), "seed {seed}: requests never ended");
}

#[test]
fn random_traffic_keeps_the_router_consistent() {
    let seeds: u64 = std::env::var("BUS_FUZZ_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(200);
    for seed in 0..seeds {
        run_seed(seed, 2_000);
    }
}
