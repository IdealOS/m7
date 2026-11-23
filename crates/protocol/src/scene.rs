//! Scene-graph ops: how an app builds and updates its window's UI.
//!
//! Apps allocate node ids themselves (per connection) so they never wait on the
//! compositor. A commit carries a batch of ops that the compositor applies
//! atomically before the next frame.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub type NodeId = u64;

/// Node properties. Kept as an open JSON map so new widget props do not need a
/// protocol bump; the compositor ignores props it does not understand.
pub type Props = Map<String, Value>;

/// Node types understood by the compositor. Phase 0 renders the primitives
/// (`rect`, `text`, `group`); widgets arrive in M4 and M5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    Group,
    Rect,
    Text,
    Image,
    Vstack,
    Hstack,
    Label,
    Button,
}

/// One change to a window's scene graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum SceneOp {
    Create {
        id: NodeId,
        #[serde(rename = "type")]
        kind: NodeKind,
        #[serde(default)]
        props: Props,
    },
    /// Merge `props` into the node's existing props.
    Set {
        id: NodeId,
        props: Props,
    },
    Insert {
        parent: NodeId,
        child: NodeId,
        index: usize,
    },
    Remove {
        parent: NodeId,
        child: NodeId,
    },
    Destroy {
        id: NodeId,
    },
    Listen {
        id: NodeId,
        events: Vec<String>,
    },
}

/// Params of a `compositor.commit` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Commit {
    pub window: u64,
    pub ops: Vec<SceneOp>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::Message;

    #[test]
    fn plan_commit_example_parses() {
        let json = r#"{"id": 50, "kind": "request", "to": "compositor", "method": "commit", "params": {"window": 1, "ops": [
          {"op": "create", "id": 10, "type": "vstack", "props": {"gap": 8, "padding": 12}},
          {"op": "create", "id": 11, "type": "label", "props": {"text": "Inbox (3)", "style": "title"}},
          {"op": "create", "id": 12, "type": "button", "props": {"label": "Compose"}},
          {"op": "insert", "parent": 1, "child": 10, "index": 0},
          {"op": "insert", "parent": 10, "child": 11, "index": 0},
          {"op": "insert", "parent": 10, "child": 12, "index": 1},
          {"op": "listen", "id": 12, "events": ["click"]}
        ]}}"#;
        let Message::Request { method, params, .. } = serde_json::from_str(json).unwrap() else {
            panic!("expected a request");
        };
        assert_eq!(method, "commit");
        let commit: Commit = serde_json::from_value(params).unwrap();
        assert_eq!(commit.ops.len(), 7);
        assert!(matches!(commit.ops[0], SceneOp::Create { id: 10, kind: NodeKind::Vstack, .. }));
        assert!(matches!(commit.ops[6], SceneOp::Listen { id: 12, .. }));
    }

    #[test]
    fn set_op_parses() {
        let op: SceneOp =
            serde_json::from_str(r#"{"op": "set", "id": 11, "props": {"text": "Inbox (4)"}}"#).unwrap();
        let SceneOp::Set { id, props } = op else { panic!() };
        assert_eq!(id, 11);
        assert_eq!(props["text"], "Inbox (4)");
    }
}
