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

/// Who sent a message, as stamped by the broker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sender {
    /// A system service that claimed a well-known name such as `db`.
    Service(String),
    /// An app, identified by its app id and bus connection number.
    App { app: String, conn: u64 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BusError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Message {
    Request {
        id: u64,
        to: String,
        method: String,
        #[serde(default)]
        params: Value,
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
    },
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<Sender>,
    },
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
