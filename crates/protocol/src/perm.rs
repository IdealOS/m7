//! Permissions: what each app may do, kept by `permd`.
//!
//! Apps start with nothing. A grant gives one app either access to documents
//! (read, write or create documents of one type, optionally only those matching a
//! filter in the database's query language) or a capability such as network
//! access:
//!
//! ```json
//! {"app": "org.idealos.mail", "type": "Email", "access": ["read", "write"]}
//! {"app": "com.example.frame", "type": "Image", "access": ["read"], "filter": {"album": "Family"}}
//! {"app": "org.idealos.browser", "capability": "network"}
//! ```
//!
//! Besides its grants, an app may always read and write the documents it created.
//!
//! permd claims `perm`. Whenever the grants change (and when it starts) it
//! publishes all of them on [`GRANTS_TOPIC`], so the services that enforce them
//! (the broker, `db`, session) decide locally from the latest table instead of
//! asking for every message. Until a service has seen a table it treats apps as
//! having no grants.
//!
//! An app asks for more with `request`, which shows the user a prompt drawn by
//! the compositor in a layer apps can't cover. System services and tools, and apps
//! holding the `permissions` capability (Settings), list, grant and revoke
//! directly. Every grant, revocation and answer to a prompt goes into an audit log.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The name permd claims.
pub const SERVICE: &str = "perm";

/// Where permd publishes every grant ([`GrantsResult`]) whenever they change.
pub const GRANTS_TOPIC: &str = "perm.grants";

/// permd's methods.
///
/// | method    | params            | result           | who |
/// |-----------|-------------------|------------------|-----|
/// | `grants`  | [`GrantsParams`]  | [`GrantsResult`] | anyone; an app sees only its own |
/// | `request` | [`RequestParams`] | [`RequestResult`] | apps: prompts the user |
/// | `grant`   | [`GrantParams`]   | [`GrantsResult`] (the new grants) | managers |
/// | `revoke`  | [`RevokeParams`]  | [`RevokeResult`] | managers |
/// | `audit`   | [`AuditParams`]   | [`AuditResult`]  | managers |
///
/// Managers are system services, tools, and apps with the `permissions`
/// capability.
pub mod method {
    pub const GRANTS: &str = "grants";
    pub const REQUEST: &str = "request";
    pub const GRANT: &str = "grant";
    pub const REVOKE: &str = "revoke";
    pub const AUDIT: &str = "audit";
}

/// The capabilities a grant can give, besides `service:NAME`.
pub mod capability {
    /// Network access beyond loopback. Takes effect when the app next starts.
    pub const NETWORK: &str = "network";
    /// Reading the clipboard with `get_clipboard`. (Pasting into a text widget
    /// needs nothing: the compositor does it for the user.)
    pub const CLIPBOARD: &str = "clipboard";
    /// Managing other apps' grants: Settings.
    pub const PERMISSIONS: &str = "permissions";
    /// `service:NAME`: sending requests to the system service `NAME`. Apps may
    /// always use `db`, `compositor`, `perm` and `secrets`, which check requests
    /// themselves.
    pub const SERVICE_PREFIX: &str = "service:";
}

/// Services every app may send requests to; each checks what it's asked.
pub const OPEN_SERVICES: &[&str] = &["db", "compositor", "perm", "secrets"];

/// What a data grant allows on the documents it covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Access {
    /// Query, get, search and watch them, read their history and blobs.
    Read,
    /// Update, splice and delete them (and read them). A changed document must
    /// still be covered by the grant.
    Write,
    /// Create documents of the type; each must be covered by the grant's filter.
    Create,
}

/// What a grant gives.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum Rule {
    Data(DataRule),
    Capability(CapabilityRule),
}

/// Access to documents of one type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DataRule {
    #[serde(rename = "type")]
    pub doc_type: String,
    pub access: Vec<Access>,
    /// Only documents matching this filter (the database's query language).
    /// Empty covers every document of the type.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub filter: Map<String, Value>,
}

/// A capability: `network`, `clipboard`, `permissions` or `service:NAME`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CapabilityRule {
    pub capability: String,
}

impl Rule {
    pub fn data(doc_type: &str, access: &[Access]) -> Rule {
        Rule::Data(DataRule { doc_type: doc_type.into(), access: access.to_vec(), filter: Map::new() })
    }

    pub fn capability(name: &str) -> Rule {
        Rule::Capability(CapabilityRule { capability: name.into() })
    }

    /// Why this rule can't be granted, if it can't. (A data rule's filter is
    /// checked by whoever compiles it.)
    pub fn problem(&self) -> Option<String> {
        match self {
            Rule::Data(d) if !crate::db::valid_type(&d.doc_type) => {
                Some(format!("not a document type: {:?}", d.doc_type))
            }
            Rule::Data(d) if d.access.is_empty() => Some("a data rule needs at least one access".into()),
            Rule::Data(_) => None,
            Rule::Capability(c) => {
                let known = [capability::NETWORK, capability::CLIPBOARD, capability::PERMISSIONS];
                let service = c
                    .capability
                    .strip_prefix(capability::SERVICE_PREFIX)
                    .is_some_and(|s| crate::bus::valid_name(s) && !s.starts_with("bus"));
                (!known.contains(&c.capability.as_str()) && !service)
                    .then(|| format!("unknown capability {:?}", c.capability))
            }
        }
    }

    /// The rule in words, for prompts and Settings: "read and change your Note
    /// documents".
    pub fn describe(&self) -> String {
        match self {
            Rule::Capability(c) => match c.capability.as_str() {
                capability::NETWORK => "use the network".into(),
                capability::CLIPBOARD => "read what you copy".into(),
                capability::PERMISSIONS => "change what other apps may do".into(),
                other => match other.strip_prefix(capability::SERVICE_PREFIX) {
                    Some(s) => format!("use the {s} service"),
                    None => format!("have {other:?}"),
                },
            },
            Rule::Data(d) => {
                let mut verbs = Vec::new();
                if d.access.contains(&Access::Read) || d.access.contains(&Access::Write) {
                    verbs.push("read");
                }
                if d.access.contains(&Access::Write) {
                    verbs.push("change");
                }
                if d.access.contains(&Access::Create) {
                    verbs.push("add");
                }
                let verbs = match verbs.as_slice() {
                    [one] => one.to_string(),
                    [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
                    [] => "touch".into(),
                };
                let which = if d.filter.is_empty() {
                    String::new()
                } else {
                    format!(" matching {}", serde_json::to_string(&d.filter).unwrap_or_default())
                };
                format!("{verbs} your {} documents{which}", d.doc_type)
            }
        }
    }
}

/// One grant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Grant {
    pub id: i64,
    pub app: String,
    #[serde(flatten)]
    pub rule: Rule,
    /// When, in milliseconds since the Unix epoch.
    pub granted: i64,
    /// Who granted it: `user` (a prompt), or the service, tool or app that called
    /// `grant`.
    pub by: String,
}

/// Params of `grants`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GrantsParams {
    /// Only this app's grants. (An app always gets only its own.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
}

/// Result of `grants` and `grant`, and the data published on [`GRANTS_TOPIC`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GrantsResult {
    pub grants: Vec<Grant>,
    /// Grows with every change (it survives restarts), so a follower that gets
    /// tables out of order keeps the newest.
    #[serde(default)]
    pub version: i64,
}

/// Params of `request`: what the app would like, and why, in its own words.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RequestParams {
    pub rules: Vec<Rule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Result of `request`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RequestResult {
    /// Whether the app now has everything it asked for (because the user allowed
    /// it, or it already had it).
    pub granted: bool,
}

/// Params of `grant`: give app `app` these rules, without asking the user.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GrantParams {
    pub app: String,
    pub rules: Vec<Rule>,
}

/// Params of `revoke`: the grants with these ids, and every grant of `app`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RevokeParams {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ids: Vec<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
}

/// Result of `revoke`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RevokeResult {
    /// How many grants were removed.
    pub revoked: u64,
}

/// Params of `audit`: the most recent entries.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AuditParams {
    /// At most this many (default 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// What happened, in an audit entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum AuditAction {
    /// A grant was made.
    Grant,
    /// A grant was removed.
    Revoke,
    /// The user turned down a request.
    Deny,
}

/// One entry in the audit log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AuditEntry {
    pub id: i64,
    /// When, in milliseconds since the Unix epoch.
    pub at: i64,
    pub app: String,
    pub action: AuditAction,
    pub rule: Rule,
    /// Who did it: `user`, or a service, tool or app.
    pub by: String,
}

/// Result of `audit`, newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AuditResult {
    pub entries: Vec<AuditEntry>,
}

/// The grants of one app, as the enforcing services use them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AppGrants {
    pub data: Vec<DataRule>,
    pub capabilities: Vec<String>,
}

impl AppGrants {
    pub fn has(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
    }
}

/// A grant table split by app.
pub fn by_app(grants: &[Grant]) -> std::collections::HashMap<String, AppGrants> {
    let mut apps: std::collections::HashMap<String, AppGrants> = std::collections::HashMap::new();
    for g in grants {
        let entry = apps.entry(g.app.clone()).or_default();
        match &g.rule {
            Rule::Data(d) => entry.data.push(d.clone()),
            Rule::Capability(c) => entry.capabilities.push(c.capability.clone()),
        }
    }
    apps
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn grants_parse_as_the_plan_writes_them() {
        let g: Grant = serde_json::from_value(json!({
            "id": 1, "app": "com.example.frame", "type": "Image", "access": ["read"],
            "filter": {"album": "Family"}, "granted": 5, "by": "user"
        }))
        .unwrap();
        let Rule::Data(d) = &g.rule else { panic!("{g:?}") };
        assert_eq!((d.doc_type.as_str(), d.access.as_slice()), ("Image", &[Access::Read][..]));
        assert_eq!(d.filter["album"], "Family");
        let c: Grant = serde_json::from_value(json!({
            "id": 2, "app": "org.idealos.browser", "capability": "network", "granted": 5, "by": "osctl"
        }))
        .unwrap();
        assert_eq!(c.rule, Rule::capability("network"));
        // And back.
        assert_eq!(serde_json::to_value(&c).unwrap()["capability"], "network");
    }

    #[test]
    fn rules_are_checked_and_described() {
        assert_eq!(Rule::data("Note", &[Access::Read]).problem(), None);
        assert!(Rule::data("Note", &[]).problem().is_some());
        assert!(Rule::data("not a type", &[Access::Read]).problem().is_some());
        assert_eq!(Rule::capability("network").problem(), None);
        assert_eq!(Rule::capability("service:net").problem(), None);
        assert!(Rule::capability("service:bus").problem().is_some(), "the broker isn't a service");
        assert!(Rule::capability("root").problem().is_some());
        assert_eq!(
            Rule::data("Note", &[Access::Write, Access::Create]).describe(),
            "read, change and add your Note documents"
        );
        assert_eq!(Rule::data("Image", &[Access::Read]).describe(), "read your Image documents");
        assert_eq!(Rule::capability("network").describe(), "use the network");
    }
}
