//! `busd`: the message bus broker.
//!
//! Services and apps exchange requests and pub/sub messages through it over a Unix
//! socket, in the length-prefixed JSON frames `protocol::bus` defines. The broker
//! stamps every message with its sender, routes requests to the connection that
//! claimed the name they're addressed to, and passes everything through a
//! permission hook ([`router::Policy`]) that allows everything until M6.

pub mod router;
pub mod server;

pub use router::{AllowAll, Policy};
pub use server::serve;
