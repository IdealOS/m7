//! The broker's permission policy, from the grants permd publishes.
//!
//! The broker sees every publish, so it reads permd's grant table straight off
//! [`perm::GRANTS_TOPIC`] as it routes it (only from the connection holding
//! `perm`) and decides each message from the newest table, without a round trip.
//! Until a table arrives, apps have no grants.
//!
//! What it decides is coarse: which services an app may send requests to, and
//! which topics it may use. The services check the rest themselves (`db` checks
//! documents against the same grants).
//!
//! - Requests: an app may use the services in [`perm::OPEN_SERVICES`], and any
//!   other service `NAME` it holds `service:NAME` for.
//! - Topics: an app may publish and subscribe only under `app.<its id>`.
//! - System services and tools may do anything.

use std::collections::HashMap;

use protocol::bus::{BusError, Message, Sender, code};
use protocol::perm::{self, AppGrants, GrantsResult, capability};

use crate::Policy;

#[derive(Default)]
pub struct Grants {
    apps: HashMap<String, AppGrants>,
    version: i64,
}

impl Grants {
    pub fn new() -> Grants {
        Grants { apps: HashMap::new(), version: -1 }
    }

    fn of(&self, app: &str) -> Option<&AppGrants> {
        self.apps.get(app)
    }
}

impl Policy for Grants {
    fn check(&self, sender: &Sender, msg: &Message) -> Result<(), BusError> {
        let Sender::App { app, .. } = sender else { return Ok(()) };
        let refuse = |message: String| Err(BusError { code: code::PERMISSION_DENIED.into(), message });
        match msg {
            Message::Request { to, .. } => {
                let granted =
                    self.of(app).is_some_and(|g| g.has(&format!("{}{to}", capability::SERVICE_PREFIX)));
                if perm::OPEN_SERVICES.contains(&to.as_str()) || granted {
                    Ok(())
                } else {
                    refuse(format!("{app} may not use the {to} service"))
                }
            }
            Message::Publish { topic, .. } | Message::Subscribe { topic, .. } => {
                if own_topic(app, topic) {
                    Ok(())
                } else {
                    refuse(format!("{app} may use only topics under app.{app}"))
                }
            }
            _ => Ok(()),
        }
    }

    fn observe(&mut self, sender: &Sender, msg: &Message) {
        let Message::Publish { topic, data, .. } = msg else { return };
        if topic != perm::GRANTS_TOPIC {
            return;
        }
        if *sender != Sender::Service(perm::SERVICE.into()) {
            log::warn!("bus: ignored grants published by {sender:?}");
            return;
        }
        match serde_json::from_value::<GrantsResult>(data.clone()) {
            Ok(table) if table.version >= self.version => {
                self.version = table.version;
                self.apps = perm::by_app(&table.grants);
                log::info!("bus: following {} grants (version {})", table.grants.len(), table.version);
            }
            Ok(_) => {}
            Err(e) => log::warn!("bus: ignored a grant table it couldn't read: {e}"),
        }
    }
}

/// Whether `topic` (or a pattern) is under `app.<app>`.
fn own_topic(app: &str, topic: &str) -> bool {
    let own = format!("app.{app}");
    topic == own || topic.strip_prefix(&own).is_some_and(|rest| rest.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn app(id: &str) -> Sender {
        Sender::App { app: id.into(), conn: 9 }
    }

    fn request(to: &str) -> Message {
        Message::request(1, to, "m", json!({}))
    }

    fn grants(table: serde_json::Value) -> Message {
        Message::publish(perm::GRANTS_TOPIC, table)
    }

    #[test]
    fn apps_reach_open_services_and_granted_ones() {
        let mut p = Grants::new();
        let notes = app("org.x.notes");
        for open in ["db", "compositor", "perm", "secrets"] {
            assert!(p.check(&notes, &request(open)).is_ok(), "{open}");
        }
        assert!(p.check(&notes, &request("session")).is_err());
        assert!(p.check(&notes, &request("net")).is_err());
        assert!(p.check(&Sender::Service("compositor".into()), &request("session")).is_ok(), "services may");
        assert!(p.check(&Sender::Conn(3), &request("net")).is_ok(), "tools may");

        let table = json!({"version": 1, "grants": [{"id": 1, "app": "org.x.notes", "capability": "service:net", "granted": 0, "by": "t"}]});
        p.observe(&Sender::Service("perm".into()), &grants(table));
        assert!(p.check(&notes, &request("net")).is_ok());
        assert!(p.check(&app("org.x.other"), &request("net")).is_err());
    }

    #[test]
    fn only_perm_publishes_grants_and_old_tables_are_ignored() {
        let mut p = Grants::new();
        let notes = app("org.x.notes");
        let table = |v: i64, cap: &str| json!({"version": v, "grants": [{"id": 1, "app": "org.x.notes", "capability": cap, "granted": 0, "by": "t"}]});
        p.observe(&app("org.x.notes"), &grants(table(5, "service:net")));
        p.observe(&Sender::Service("db".into()), &grants(table(5, "service:net")));
        assert!(p.check(&notes, &request("net")).is_err(), "not from perm");
        p.observe(&Sender::Service("perm".into()), &grants(table(5, "service:net")));
        p.observe(&Sender::Service("perm".into()), &grants(table(4, "service:audio")));
        assert!(p.check(&notes, &request("net")).is_ok(), "4 is older than 5");
        p.observe(&Sender::Service("perm".into()), &grants(json!({"version": 6, "grants": []})));
        assert!(p.check(&notes, &request("net")).is_err(), "revoked");
    }

    #[test]
    fn apps_keep_to_their_own_topics() {
        let p = Grants::new();
        let notes = app("org.x.notes");
        let sub = |t: &str| Message::Subscribe { id: 1, topic: t.into() };
        let publish = |t: &str| Message::publish(t, json!(1));
        assert!(p.check(&notes, &sub("app.org.x.notes")).is_ok());
        assert!(p.check(&notes, &sub("app.org.x.notes.*")).is_ok());
        assert!(p.check(&notes, &publish("app.org.x.notes.changed")).is_ok());
        assert!(p.check(&notes, &publish("app.org.x.notesy")).is_err());
        assert!(p.check(&notes, &sub("app.*")).is_err());
        assert!(p.check(&notes, &sub("perm.grants")).is_err());
        assert!(p.check(&notes, &publish("perm.grants")).is_err());
    }
}
