//! Networking: `netd` joins Wi-Fi and reports connections, over NetworkManager.
//!
//! netd claims `net`, and apps reach it with the `service:net` grant (Settings
//! has it). Passwords go to NetworkManager, which keeps them in its connection
//! profiles on `/data`; netd never logs them.

use serde::{Deserialize, Serialize};

/// The name netd claims.
pub const SERVICE: &str = "net";

/// netd's methods.
///
/// | method       | params             | result |
/// |--------------|--------------------|--------|
/// | `status`     | none               | [`NetStatus`] |
/// | `watch`      | none               | streamed [`NetStatus`]es: now, then each change |
/// | `networks`   | [`NetworksParams`] | [`NetworksResult`] |
/// | `connect`    | [`ConnectParams`]  | null, once connected (or an error) |
/// | `disconnect` | none               | null |
/// | `forget`     | [`SsidParams`]     | null |
/// | `set_wifi`   | [`WifiParams`]     | null |
pub mod method {
    pub const STATUS: &str = "status";
    pub const WATCH: &str = "watch";
    pub const NETWORKS: &str = "networks";
    pub const CONNECT: &str = "connect";
    pub const DISCONNECT: &str = "disconnect";
    pub const FORGET: &str = "forget";
    pub const SET_WIFI: &str = "set_wifi";
}

/// Error codes netd sends, besides the broker's.
pub mod code {
    /// NetworkManager isn't running or has no Wi-Fi device.
    pub const UNAVAILABLE: &str = "unavailable";
    /// Joining failed: a wrong password, or the network went away.
    pub const CONNECT_FAILED: &str = "connect_failed";
    pub const NOT_FOUND: &str = "not_found";
}

/// How connected the device is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum NetState {
    /// Online.
    Connected,
    /// Joining a network.
    Connecting,
    /// Connected to a network with no route to the internet.
    Limited,
    Disconnected,
    /// No network manager to ask.
    Unavailable,
}

/// One active connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Connection {
    /// Its name: the Wi-Fi network's, or the wired profile's.
    pub name: String,
    /// `wifi` or `ethernet` (or another NetworkManager type).
    pub kind: String,
    /// The network interface, such as `wlan0`.
    pub device: String,
    /// Its IPv4 address, with prefix length, if it has one yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip4: Option<String>,
}

/// Result of `status`, and each part of `watch`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NetStatus {
    pub state: NetState,
    pub connections: Vec<Connection>,
    /// Whether Wi-Fi is switched on.
    pub wifi_enabled: bool,
    /// Whether there's a Wi-Fi device at all.
    pub wifi_present: bool,
}

/// Params of `networks`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NetworksParams {
    /// Scan first (takes a few seconds), rather than list what was last seen.
    #[serde(default)]
    pub rescan: bool,
}

/// A Wi-Fi network in range.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WifiNetwork {
    pub ssid: String,
    /// Signal strength, 0 to 100.
    pub strength: u8,
    /// Needs a password.
    pub secure: bool,
    /// The device is on it now.
    pub active: bool,
    /// Joined before: its password is saved.
    pub known: bool,
}

/// Result of `networks`: strongest first, one entry per name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NetworksResult {
    pub networks: Vec<WifiNetwork>,
}

/// Params of `connect`: join `ssid`, with `password` if it's secure and not
/// known yet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ConnectParams {
    pub ssid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

/// Params of `forget`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SsidParams {
    pub ssid: String,
}

/// Params of `set_wifi`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WifiParams {
    pub enabled: bool,
}
