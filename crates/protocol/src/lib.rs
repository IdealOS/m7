//! Wire types shared by every service and app.
//!
//! These mirror the "Key interfaces" section of `docs/implementation-plan.md`.
//! They are drafts: JSON Schema export and generated TypeScript types come in M2.

pub mod bus;
pub mod scene;

pub use bus::{BusError, Message, Sender};
pub use scene::{Commit, NodeId, NodeKind, Props, SceneOp};
