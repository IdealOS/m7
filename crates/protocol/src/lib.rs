//! Wire types shared by every service and app.
//!
//! These mirror the "Key interfaces" section of `docs/implementation-plan.md`.
//! With the `schema` feature they derive `JsonSchema`; `crates/protocol-schema`
//! exports them to `schema.json`, and `sdk/src/protocol.ts` is generated from that
//! (`make protocol`). A test fails when either is out of date.

pub mod broker;
pub mod bus;
pub mod db;
pub mod scene;
pub mod unix;

pub use bus::{BROKER, BusError, Message, Sender};
pub use scene::{Commit, NodeId, NodeKind, Props, SceneOp};
