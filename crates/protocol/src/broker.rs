//! The broker's own methods: requests sent `to` [`BROKER`](crate::bus::BROKER),
//! which `busd` answers itself, and the trace it publishes on
//! [`TAP_TOPIC`](crate::bus::TAP_TOPIC).
//!
//! | method    | params          | result          | who may call       |
//! |-----------|-----------------|-----------------|--------------------|
//! | `hello`   | none            | [`HelloResult`] | anyone             |
//! | `claim`   | [`NameParams`]  | null            | system connections |
//! | `release` | [`NameParams`]  | null            | anyone (own names) |
//! | `list`    | none            | [`ListResult`]  | anyone             |
//! | `adopt`   | [`AdoptParams`] | [`AdoptResult`] | session only       |

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::bus::{Message, Sender};

/// Method names.
pub mod method {
    pub const HELLO: &str = "hello";
    pub const CLAIM: &str = "claim";
    pub const RELEASE: &str = "release";
    pub const LIST: &str = "list";
    pub const ADOPT: &str = "adopt";
}

/// Result of `hello`: who the broker says this connection is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HelloResult {
    /// This connection's number.
    pub conn: u64,
    /// How the broker stamps this connection's messages.
    pub sender: Sender,
}

/// Params of `claim` and `release`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NameParams {
    /// A service name: lowercase letters, digits, `-` and `.`.
    pub name: String,
}

/// Result of `list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ListResult {
    /// Each claimed service name and the connection that holds it.
    pub services: BTreeMap<String, u64>,
    /// How many connections the broker has.
    pub connections: u64,
}

/// Params of `adopt`, with which session hands the broker one end of a socket
/// pair (the request's one descriptor) and says what the connection is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum AdoptParams {
    App(AdoptApp),
    System(AdoptSystem),
}

/// An app's connection: its messages are stamped with this reverse-DNS app id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AdoptApp {
    pub app: String,
}

/// A system service's connection: it may claim names and tap the bus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AdoptSystem {
    pub trust: SystemTrust,
}

impl AdoptParams {
    pub fn app(id: impl Into<String>) -> AdoptParams {
        AdoptParams::App(AdoptApp { app: id.into() })
    }

    pub fn system() -> AdoptParams {
        AdoptParams::System(AdoptSystem { trust: SystemTrust::System })
    }
}

/// The one trust level `adopt` names explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum SystemTrust {
    System,
}

/// Result of `adopt`: the new connection's number.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AdoptResult {
    pub conn: u64,
}

/// The data of each publish on the tap topic: one message the broker received.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct TapEvent {
    /// The connection the message came from.
    pub conn: u64,
    /// How the broker stamped it.
    pub sender: Sender,
    /// The message as the connection sent it (before the broker rewrote its id).
    pub msg: Message,
    /// For a request the broker forwards: the id the service sees and answers
    /// with, to match the request to its response in the trace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bus_id: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn adopt_params_are_one_shape_or_the_other() {
        let app: AdoptParams = serde_json::from_value(json!({"app": "org.example.notes"})).unwrap();
        assert_eq!(app, AdoptParams::app("org.example.notes"));
        let system: AdoptParams = serde_json::from_value(json!({"trust": "system"})).unwrap();
        assert_eq!(system, AdoptParams::system());
        for bad in
            [json!({"app": "a.b", "trust": "system"}), json!({"trust": "root"}), json!({}), json!(null)]
        {
            assert!(serde_json::from_value::<AdoptParams>(bad.clone()).is_err(), "{bad}");
        }
        assert_eq!(serde_json::to_value(&system).unwrap(), json!({"trust": "system"}));
    }

    #[test]
    fn tap_events_leave_out_a_missing_bus_id() {
        let event =
            TapEvent { conn: 3, sender: Sender::Conn(3), msg: Message::Cancel { id: 1 }, bus_id: None };
        let v = serde_json::to_value(&event).unwrap();
        assert_eq!(v, json!({"conn": 3, "sender": {"conn": 3}, "msg": {"kind": "cancel", "id": 1}}));
        assert_eq!(serde_json::from_value::<TapEvent>(v).unwrap(), event);
    }
}
