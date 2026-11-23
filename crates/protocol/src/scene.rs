//! Scene-graph ops: how an app builds and updates its window's UI.
//!
//! Apps allocate node ids themselves (per window) so they never wait on the
//! compositor. A commit carries a batch of ops that the compositor applies
//! atomically before the next frame.
//!
//! An app opens a window with `open_window`, sent as a streamed request: the first
//! part says which window it got, later parts are the window's events (clicks on
//! nodes it listens to, the close button), and the window lasts as long as the
//! request. Cancelling it, or the app's connection closing, removes the window, so
//! nothing is left behind by an app that crashes. In a window's commits, node 1 is
//! the window's content area; every other id is the app's own, created by `create`.

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

/// The service name the compositor claims.
pub const SERVICE: &str = "compositor";

/// The compositor's methods.
///
/// | method        | params               | result |
/// |---------------|----------------------|--------|
/// | `open_window` | [`OpenWindowParams`] | streamed [`WindowEvent`]s |
/// | `commit`      | [`Commit`]           | null   |
pub mod method {
    pub const OPEN_WINDOW: &str = "open_window";
    pub const COMMIT: &str = "commit";
}

/// The node id of a window's content area in that window's commits.
pub const CONTENT: NodeId = 1;

/// Most nodes one window may hold, so a runaway app can't exhaust the compositor.
pub const MAX_WINDOW_NODES: usize = 20_000;

/// Params of `open_window`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct OpenWindowParams {
    pub title: String,
    /// Size of the content area in logical pixels (default 360 by 240).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

/// A part of `open_window`'s streamed answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum WindowEvent {
    /// The first part: the window's id, for commits, and its content size.
    Opened { window: u64, width: u32, height: u32 },
    /// Something happened to node `node` (an id from the app's commits) that the
    /// app listens for, such as `click`.
    Event { node: NodeId, event: String },
    /// The user asked to close the window. It stays until the app cancels the
    /// request (or answers it).
    Close,
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
