//! session's methods on the bus: starting and stopping apps.
//!
//! session claims the name `session`. `launch` starts an installed app (an `App`
//! document in the database) in its own `app-host` process, with a bus connection
//! that identifies it by its app id, or restarts it if it's running. Only trusted
//! connections (system services and tools) may call these.

use serde::{Deserialize, Serialize};

/// The name session claims.
pub const SERVICE: &str = "session";

/// session's methods.
///
/// | method   | params        | result       |
/// |----------|---------------|--------------|
/// | `launch` | [`AppParams`] | null         |
/// | `stop`   | [`AppParams`] | null         |
/// | `apps`   | none          | [`AppsResult`] |
pub mod method {
    pub const LAUNCH: &str = "launch";
    pub const STOP: &str = "stop";
    pub const APPS: &str = "apps";
}

/// Params of `launch` and `stop`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AppParams {
    /// The app id, such as `org.idealos.notes`.
    pub app: String,
}

/// Result of `apps`: the apps session is running.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AppsResult {
    pub running: Vec<String>,
}
