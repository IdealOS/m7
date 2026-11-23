//! Wire types shared by every service and app.
//!
//! These mirror the "Key interfaces" section of `docs/implementation-plan.md`.
//! They are drafts: JSON Schema export and generated TypeScript types are still to do
//! (M2); until then `sdk/src/protocol.ts` mirrors them by hand.

pub mod bus;
pub mod scene;
pub mod unix;

pub use bus::{BROKER, BusError, Message, Sender};
pub use scene::{Commit, NodeId, NodeKind, Props, SceneOp};
