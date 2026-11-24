//! Installing apps: what `osctl app install` and `app-host --install` share.
//!
//! An installed app is an `App` document (see `schemas/App.json`) whose `bundle`
//! is the blob holding its JavaScript. Installing stores the bundle with
//! `put_blob`, then creates the document or updates the one with the same app id;
//! session's `launch` then starts it, or restarts it if it was running.

use std::os::fd::OwnedFd;
use std::path::Path;

use bus_client::{CallError, Client};
use protocol::db::{self, Doc, DocsResult};
use protocol::perm::{self, Grant, GrantsResult, Rule};
use protocol::session;
use serde::Deserialize;
use serde_json::{Value, json};

/// What an app says about itself: `manifest.json` beside a built bundle, or the
/// `idealos` section of the app's `package.json`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Manifest {
    /// Reverse-DNS app id, such as `org.idealos.notes`.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    /// Started by `app-host --install` at boot.
    #[serde(default)]
    pub autostart: bool,
    /// What the app needs, granted when it's installed (see [`grant`]).
    #[serde(default)]
    pub permissions: Vec<Rule>,
}

impl Manifest {
    /// Read `manifest.json`, or the `idealos` section of a `package.json`.
    pub fn read(path: &Path) -> Result<Manifest, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let json: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let section = json.get("idealos").cloned().unwrap_or(json.clone());
        let mut m: Manifest = serde_json::from_value(section)
            .map_err(|e| format!("{}: not an app manifest: {e}", path.display()))?;
        if m.version.is_none() {
            m.version = json.get("version").and_then(Value::as_str).map(str::to_owned);
        }
        if !valid_app_id(&m.id) {
            return Err(format!("{}: {:?} is not a reverse-DNS app id", path.display(), m.id));
        }
        Ok(m)
    }
}

/// App ids are reverse-DNS names: two or more dot-separated segments of letters,
/// digits and `-` (as the broker checks).
pub fn valid_app_id(id: &str) -> bool {
    id.split('.').count() >= 2
        && id.split('.').all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

/// What installing did.
#[derive(Debug, Clone, PartialEq)]
pub struct Installed {
    /// The App document's id.
    pub doc: String,
    /// The bundle's blob hash.
    pub hash: String,
    /// Whether the bundle or manifest differed from what was installed.
    pub changed: bool,
}

/// Install the bundle at `bundle` as the app `manifest` describes.
pub async fn install(bus: &Client, manifest: &Manifest, bundle: &Path) -> Result<Installed, CallError> {
    let file = std::fs::File::open(bundle).map_err(|e| local(format!("{}: {e}", bundle.display())))?;
    let reply = bus
        .request_with(db::SERVICE, db::method::PUT_BLOB, Value::Null, vec![OwnedFd::from(file)], None)
        .await?;
    let hash = reply.result["hash"].as_str().ok_or_else(|| local("put_blob gave no hash".into()))?.to_owned();
    let data = json!({
        "id": manifest.id,
        "name": manifest.name,
        "version": manifest.version,
        "entry": "app.js",
        "bundle": {"$blob": hash},
        "autostart": manifest.autostart,
        "permissions": manifest.permissions,
    });
    match find(bus, &manifest.id).await? {
        Some(doc) => {
            let changed = doc.data.iter().any(|(k, v)| data.get(k) != Some(v))
                || data.as_object().is_some_and(|d| d.keys().any(|k| !doc.data.contains_key(k)));
            if changed {
                bus.request(db::SERVICE, db::method::UPDATE, json!({"id": doc.id, "patch": data})).await?;
            }
            Ok(Installed { doc: doc.id, hash, changed })
        }
        None => {
            let created =
                bus.request(db::SERVICE, db::method::CREATE, json!({"type": "App", "data": data})).await?;
            let doc = created["id"].as_str().unwrap_or_default().to_owned();
            Ok(Installed { doc, hash, changed: true })
        }
    }
}

/// Grant app `manifest` the permissions it lists, through permd, returning what
/// was newly granted. Installing from the system image or with `osctl` (both
/// trusted) grants them without a prompt.
pub async fn grant(bus: &Client, manifest: &Manifest) -> Result<Vec<Grant>, CallError> {
    if manifest.permissions.is_empty() {
        return Ok(Vec::new());
    }
    let params = json!({"app": manifest.id, "rules": manifest.permissions});
    let made = bus.request(perm::SERVICE, perm::method::GRANT, params).await?;
    let made: GrantsResult = serde_json::from_value(made).map_err(|e| local(e.to_string()))?;
    Ok(made.grants)
}

/// The installed `App` document for app `id`, if there is one.
pub async fn find(bus: &Client, id: &str) -> Result<Option<Doc>, CallError> {
    let found = bus
        .request(db::SERVICE, db::method::QUERY, json!({"type": "App", "where": {"id": id}, "limit": 1}))
        .await?;
    let docs: DocsResult = serde_json::from_value(found).map_err(|e| local(e.to_string()))?;
    Ok(docs.docs.into_iter().next())
}

/// Every installed app's document.
pub async fn installed(bus: &Client) -> Result<Vec<Doc>, CallError> {
    let found = bus.request(db::SERVICE, db::method::QUERY, json!({"type": "App", "limit": 1000})).await?;
    let docs: DocsResult = serde_json::from_value(found).map_err(|e| local(e.to_string()))?;
    Ok(docs.docs)
}

/// The JavaScript of installed app `id`.
pub async fn fetch_bundle(bus: &Client, id: &str) -> Result<String, CallError> {
    let doc = find(bus, id).await?.ok_or_else(|| local(format!("no app {id} is installed")))?;
    let hash = doc.data.get("bundle").and_then(|b| b.get("$blob")).and_then(Value::as_str);
    let hash = hash.ok_or_else(|| local(format!("app {id} has no bundle")))?;
    let reply =
        bus.request_with(db::SERVICE, db::method::GET_BLOB, json!({"hash": hash}), Vec::new(), None).await?;
    let fd = reply.fds.into_iter().next().ok_or_else(|| local("get_blob sent no descriptor".into()))?;
    let mut text = String::new();
    std::io::Read::read_to_string(&mut std::fs::File::from(fd), &mut text)
        .map_err(|e| local(format!("reading app {id}'s bundle: {e}")))?;
    Ok(text)
}

/// Ask session to start app `id` (or restart it).
pub async fn launch(bus: &Client, id: &str) -> Result<(), CallError> {
    bus.request(session::SERVICE, session::method::LAUNCH, json!({"app": id})).await.map(drop)
}

fn local(message: String) -> CallError {
    CallError::Bus(protocol::BusError { code: "app_host".into(), message })
}
