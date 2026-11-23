//! Bus envelope.
//!
//! Frames on the wire are a 4-byte big-endian length followed by one UTF-8 JSON
//! [`Message`]. The broker stamps `from` on every message it forwards; any `from`
//! a client sends is overwritten, so identity cannot be forged.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Largest frame the broker accepts. Bulk data (blobs, GPU buffers) travels as
/// passed file descriptors, never inside JSON, so frames stay small.
pub const MAX_FRAME_BYTES: u32 = 4 * 1024 * 1024;

/// The broker's own name: requests sent `to` it are handled by `busd` itself
/// (`hello`, `claim`, `release`, `list`, and `adopt` from session).
pub const BROKER: &str = "bus";

/// Topic the broker publishes a trace of every message it receives on. Only
/// trusted connections may subscribe (`osctl bus tap` does).
pub const TAP_TOPIC: &str = "bus.tap";

/// How long the broker waits for a response before answering `timeout` itself,
/// unless the request sets `timeout_ms`.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Longest `timeout_ms` the broker honors; longer ones are shortened to this.
pub const MAX_TIMEOUT_MS: u64 = 60 * 60 * 1000;

/// Most requests one connection may have waiting for an answer at once. Past it,
/// the broker answers `limit_exceeded` without forwarding.
pub const MAX_PENDING_REQUESTS: usize = 1024;

/// Most subscription patterns one connection may hold.
pub const MAX_SUBSCRIPTIONS: usize = 256;

/// Most service names one connection may claim.
pub const MAX_NAMES: usize = 64;

/// Error codes the broker itself sends. Services may send their own.
pub mod code {
    /// Declares each code as a constant, and lists them all with their docs for
    /// the JSON Schema export.
    macro_rules! codes {
        ($($(#[doc = $doc:literal])+ $name:ident = $value:literal;)+) => {
            $($(#[doc = $doc])+ pub const $name: &str = $value;)+
            /// Every code above, with its description.
            pub const ALL: &[(&str, &str)] = &[$(($value, concat!($($doc),+))),+];
        };
    }

    codes! {
        /// The frame was valid JSON but the message makes no sense (unknown method,
        /// bad params).
        BAD_REQUEST = "bad_request";
        /// No connection has claimed the name the request was sent to.
        NO_SUCH_SERVICE = "no_such_service";
        /// Another connection already holds the name.
        NAME_TAKEN = "name_taken";
        /// The permission hook refused the message.
        PERMISSION_DENIED = "permission_denied";
        /// The service didn't answer within the request's timeout.
        TIMEOUT = "timeout";
        /// The service disconnected before answering.
        SERVICE_GONE = "service_gone";
        /// The connection is at one of the broker's per-connection limits (pending
        /// requests, subscriptions or names).
        LIMIT_EXCEEDED = "limit_exceeded";
    }
}

/// Who sent a message, as stamped by the broker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Sender {
    /// A system service that claimed a well-known name such as `db`.
    Service(String),
    /// An app, identified by its app id and bus connection number.
    App { app: String, conn: u64 },
    /// A trusted local connection (a tool such as `osctl`, or a service before it
    /// claims a name), identified by its bus connection number.
    Conn(u64),
}

/// Why a request failed. `code` is one of the broker's codes or a service's own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BusError {
    pub code: String,
    pub message: String,
}

/// One bus frame's JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Message {
    Request {
        id: u64,
        to: String,
        method: String,
        #[serde(default)]
        params: Value,
        /// Overrides [`DEFAULT_TIMEOUT_MS`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
        /// How many file descriptors travel with this message (see [`crate::unix`]).
        #[serde(default, skip_serializing_if = "is_zero")]
        fds: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<Sender>,
    },
    Response {
        id: u64,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<BusError>,
        #[serde(default, skip_serializing_if = "is_zero")]
        fds: u32,
    },
    /// The requester no longer wants the answer to request `id`. The broker
    /// forwards it to the service and drops the response if one still comes.
    Cancel {
        id: u64,
    },
    /// `topic` is an exact topic, or a prefix ending in `.*` (see
    /// [`topic_matches`]). The broker answers with a response carrying `id`.
    Subscribe {
        id: u64,
        topic: String,
    },
    Unsubscribe {
        id: u64,
        topic: String,
    },
    Publish {
        topic: String,
        #[serde(default)]
        data: Value,
        #[serde(default, skip_serializing_if = "is_zero")]
        fds: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<Sender>,
    },
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl Message {
    pub fn ok(id: u64, result: Value) -> Message {
        Message::Response { id, ok: true, result: Some(result), error: None, fds: 0 }
    }

    pub fn err(id: u64, code: &str, message: impl Into<String>) -> Message {
        Message::failed(id, BusError { code: code.into(), message: message.into() })
    }

    pub fn failed(id: u64, error: BusError) -> Message {
        Message::Response { id, ok: false, result: None, error: Some(error), fds: 0 }
    }

    pub fn request(id: u64, to: impl Into<String>, method: impl Into<String>, params: Value) -> Message {
        Message::Request {
            id,
            to: to.into(),
            method: method.into(),
            params,
            timeout_ms: None,
            fds: 0,
            from: None,
        }
    }

    pub fn publish(topic: impl Into<String>, data: Value) -> Message {
        Message::Publish { topic: topic.into(), data, fds: 0, from: None }
    }

    /// How many file descriptors travel with this message.
    pub fn fds(&self) -> u32 {
        match self {
            Message::Request { fds, .. } | Message::Response { fds, .. } | Message::Publish { fds, .. } => {
                *fds
            }
            _ => 0,
        }
    }

    /// Set the descriptor count, where the message kind can carry descriptors.
    pub fn with_fds(mut self, n: u32) -> Message {
        if let Message::Request { fds, .. } | Message::Response { fds, .. } | Message::Publish { fds, .. } =
            &mut self
        {
            *fds = n;
        }
        self
    }
}

/// Whether subscription `pattern` covers `topic`. A pattern is an exact topic, a
/// prefix ending in `.*` that matches every topic below it (`db.live.*` matches
/// `db.live.7` and `db.live.7.x`, not `db.live` or `db.lively`), or `*` for every
/// topic except the broker's own `bus.` topics.
pub fn topic_matches(pattern: &str, topic: &str) -> bool {
    if pattern == "*" {
        return !topic.starts_with("bus.");
    }
    match pattern.strip_suffix(".*") {
        None => pattern == topic,
        Some(prefix) => {
            topic.len() > prefix.len() + 1
                && topic.starts_with(prefix)
                && topic.as_bytes()[prefix.len()] == b'.'
        }
    }
}

/// Whether `pattern` is one [`topic_matches`] understands: `*`, or dot-separated
/// non-empty segments with `*` allowed only as a final `.*`.
pub fn valid_pattern(pattern: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let body = pattern.strip_suffix(".*").unwrap_or(pattern);
    valid_topic(body)
}

/// Whether `topic` can be published to: dot-separated non-empty segments, no `*`.
pub fn valid_topic(topic: &str) -> bool {
    !topic.is_empty() && !topic.contains('*') && topic.split('.').all(|s| !s.is_empty())
}

/// Where `busd` listens: `$IDEALOS_BUS` if set; otherwise `/run/idealos/bus.sock`
/// on the device (Linux, running as root) and a per-user directory elsewhere, so
/// hosted development on a Mac needs no setup.
pub fn default_socket_path() -> std::path::PathBuf {
    if let Some(p) = std::env::var_os("IDEALOS_BUS") {
        return p.into();
    }
    // init starts session with no USER set; a developer's shell in the build VM has one.
    let root = match std::env::var("USER") {
        Ok(user) => user == "root",
        Err(_) => true,
    };
    if cfg!(target_os = "linux") && root {
        return "/run/idealos/bus.sock".into();
    }
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => std::path::PathBuf::from(dir).join("idealos").join("bus.sock"),
        None => {
            let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
            std::env::temp_dir().join(format!("idealos-{user}")).join("bus.sock")
        }
    }
}

/// Encode a message as one length-prefixed frame.
pub fn encode_frame(msg: &Message) -> serde_json::Result<Vec<u8>> {
    let body = serde_json::to_vec(msg)?;
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

#[derive(Debug, PartialEq)]
pub enum FrameError {
    /// Not enough bytes yet; read more and try again.
    Incomplete,
    TooLarge(u32),
    Json(String),
}

/// Decode one frame from the front of `buf`. Returns the message and the number
/// of bytes consumed.
pub fn decode_frame(buf: &[u8]) -> Result<(Message, usize), FrameError> {
    if buf.len() < 4 {
        return Err(FrameError::Incomplete);
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len));
    }
    let end = 4 + len as usize;
    if buf.len() < end {
        return Err(FrameError::Incomplete);
    }
    let msg = serde_json::from_slice(&buf[4..end]).map_err(|e| FrameError::Json(e.to_string()))?;
    Ok((msg, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The examples from the implementation plan must parse unchanged.
    #[test]
    fn plan_examples_parse() {
        let lines = [
            r#"{"id": 42, "kind": "request", "to": "db", "method": "query", "params": {"type": "Email", "where": {"read": false}}}"#,
            r#"{"id": 42, "kind": "response", "ok": true, "result": {"docs": []}}"#,
            r#"{"id": 43, "kind": "response", "ok": false, "error": {"code": "permission_denied", "message": "read Email not granted"}}"#,
            r#"{"id": 44, "kind": "subscribe", "topic": "db.live.7"}"#,
            r#"{"kind": "publish", "topic": "db.live.7", "from": {"service": "db"}, "data": {"added": [], "changed": [], "removed": []}}"#,
        ];
        for line in lines {
            let msg: Message = serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}"));
            let again: Message = serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
            assert_eq!(msg, again);
        }
    }

    #[test]
    fn cancel_and_timeouts_round_trip() {
        let line = r#"{"id": 7, "kind": "request", "to": "db", "method": "query", "timeout_ms": 500}"#;
        let msg: Message = serde_json::from_str(line).unwrap();
        assert!(matches!(msg, Message::Request { timeout_ms: Some(500), .. }));
        let cancel: Message = serde_json::from_str(r#"{"kind": "cancel", "id": 7}"#).unwrap();
        assert_eq!(cancel, Message::Cancel { id: 7 });
        let with_fds: Message =
            serde_json::from_str(r#"{"kind": "publish", "topic": "t", "fds": 2}"#).unwrap();
        assert_eq!(with_fds.fds(), 2);
        // A zero count stays off the wire, so the plan's examples are unchanged.
        assert!(!serde_json::to_string(&Message::publish("t", Value::Null)).unwrap().contains("fds"));
        let conn = serde_json::to_value(Sender::Conn(3)).unwrap();
        assert_eq!(conn, serde_json::json!({"conn": 3}));
    }

    #[test]
    fn topic_patterns() {
        assert!(topic_matches("ui.event", "ui.event"));
        assert!(!topic_matches("ui.event", "ui.events"));
        assert!(topic_matches("db.live.*", "db.live.7"));
        assert!(topic_matches("db.live.*", "db.live.7.x"));
        assert!(!topic_matches("db.live.*", "db.live"));
        assert!(!topic_matches("db.live.*", "db.lively"));
        assert!(topic_matches("*", "anything.at.all"));
        // The broker's trace only goes to connections that name it.
        assert!(!topic_matches("*", "bus.tap"));
        assert!(topic_matches("bus.*", "bus.tap"));
        for good in ["*", "a", "a.b", "a.*", "a.b.*"] {
            assert!(valid_pattern(good), "{good}");
        }
        for bad in ["", ".", "a.", ".a", "a..b", "a*", "*.a", "a.*.b", ".*"] {
            assert!(!valid_pattern(bad), "{bad}");
        }
        assert!(!valid_topic("a.*"));
    }

    #[test]
    fn frames_round_trip_and_report_partial_reads() {
        let msg = Message::Subscribe { id: 1, topic: "ui.event".into() };
        let frame = encode_frame(&msg).unwrap();
        assert_eq!(decode_frame(&frame[..3]), Err(FrameError::Incomplete));
        assert_eq!(decode_frame(&frame[..frame.len() - 1]), Err(FrameError::Incomplete));
        let (decoded, used) = decode_frame(&frame).unwrap();
        assert_eq!(decoded, msg);
        assert_eq!(used, frame.len());
    }

    #[test]
    fn oversized_frames_are_rejected_before_reading_the_body() {
        let header = (MAX_FRAME_BYTES + 1).to_be_bytes();
        assert_eq!(decode_frame(&header), Err(FrameError::TooLarge(MAX_FRAME_BYTES + 1)));
    }
}
