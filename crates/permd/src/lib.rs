//! The `perm` service: grants, the audit log, and permission prompts.
//!
//! permd keeps every grant in its own SQLite file ([`store`]) and publishes the
//! whole table on [`perm::GRANTS_TOPIC`] when it starts and after every change.
//! The broker, `db` and session enforce grants from the latest table they saw;
//! nothing asks permd per message.
//!
//! An app's `request` asks the compositor to `prompt` the user, in its trusted
//! layer, and grants what was asked if the user allows it. An app has at most one
//! prompt waiting at a time, and may never ask for the `permissions` capability:
//! only a manager (a system service, a tool, or an app that already has it) grants
//! that.

pub mod store;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bus_client::{CallError, Client, Incoming};
use protocol::BusError;
use protocol::bus::{Sender, code, valid_app_id};
use protocol::db::Query;
use protocol::perm::{
    self, AuditParams, AuditResult, GrantParams, GrantsParams, GrantsResult, RequestParams, RequestResult,
    RevokeParams, RevokeResult, Rule, capability, method,
};
use protocol::scene::{PromptParams, PromptResult};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

pub use store::Store;

/// How long a prompt may wait for the user before the request fails.
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Who is asking.
#[derive(Debug, Clone, PartialEq)]
enum Who {
    /// May list, grant, revoke and read the audit log; recorded as this name.
    Manager(String),
    /// An app without the `permissions` capability.
    App(String),
}

type Shared = Arc<Mutex<Store>>;

/// Serve `store` on `bus`, which must already have claimed `perm`, until the
/// connection closes.
pub async fn serve(bus: Client, store: Store) -> Result<(), String> {
    let store: Shared = Arc::new(Mutex::new(store));
    let Some(mut requests) = bus.take_requests() else { return Ok(()) };
    publish(&bus, &store).await;
    let prompting: Arc<Mutex<HashSet<String>>> = Arc::default();
    while let Some(incoming) = requests.next().await {
        let Incoming::Request { id, from, method, params, .. } = incoming else { continue };
        let who = match who(&store, from.as_ref()) {
            Ok(w) => w,
            Err(e) => {
                let _ = bus.respond(id, Err(internal(e))).await;
                continue;
            }
        };
        if method == method::REQUEST {
            let (bus, store, prompting) = (bus.clone(), store.clone(), prompting.clone());
            tokio::spawn(async move {
                let answer = request(&bus, &store, &prompting, who, params).await;
                let _ = bus.respond(id, answer).await;
            });
            continue;
        }
        let (answer, changed) = handle(&store, &who, &method, params);
        let _ = bus.respond(id, answer).await;
        if changed {
            publish(&bus, &store).await;
        }
    }
    Ok(())
}

/// Publish every grant on the grants topic.
async fn publish(bus: &Client, store: &Shared) {
    let table = store.lock().unwrap().all();
    match table {
        Ok(t) => {
            log::info!("permd: {} grants (version {})", t.grants.len(), t.version);
            if let Err(e) = bus.publish(perm::GRANTS_TOPIC, to_value(t)).await {
                log::error!("permd: publishing grants: {e}");
            }
        }
        Err(e) => log::error!("permd: reading grants: {e}"),
    }
}

fn who(store: &Shared, from: Option<&Sender>) -> rusqlite::Result<Who> {
    Ok(match from {
        Some(Sender::Service(name)) => Who::Manager(name.clone()),
        Some(Sender::App { app, .. }) => {
            let held = store.lock().unwrap().of(app)?;
            let manages = held.grants.iter().any(|g| g.rule == Rule::capability(capability::PERMISSIONS));
            if manages { Who::Manager(app.clone()) } else { Who::App(app.clone()) }
        }
        Some(Sender::Conn(n)) => Who::Manager(format!("conn {n}")),
        None => Who::Manager("unknown".into()),
    })
}

/// Answer one request other than `request`; also says whether grants changed.
fn handle(store: &Shared, who: &Who, method: &str, params: Value) -> (Result<Value, BusError>, bool) {
    let mut s = store.lock().unwrap();
    let manager = match who {
        Who::Manager(name) => Some(name.as_str()),
        Who::App(_) => None,
    };
    let only_managers = || Err(error(code::PERMISSION_DENIED, format!("only managers may use {method}")));
    match (method, manager) {
        (method::GRANTS, _) => {
            let answer = parse::<GrantsParams>(params).and_then(|p| {
                let table = match (who, p.app) {
                    (Who::App(app), _) => s.of(app),
                    (_, Some(app)) => s.of(&app),
                    (_, None) => s.all(),
                };
                table.map(to_value).map_err(internal)
            });
            (answer, false)
        }
        (method::GRANT, Some(by)) => {
            let answer = parse::<GrantParams>(params).and_then(|p| {
                if !valid_app_id(&p.app) {
                    return Err(error(code::BAD_REQUEST, format!("{:?} is not an app id", p.app)));
                }
                check_rules(&p.rules)?;
                let made = s.grant(&p.app, &p.rules, by).map_err(internal)?;
                for g in &made {
                    log::info!("permd: {by} granted {} to {}", g.rule.describe(), g.app);
                }
                Ok(to_value(GrantsResult { grants: made, version: s.version().map_err(internal)? }))
            });
            let changed =
                answer.as_ref().is_ok_and(|v| v["grants"].as_array().is_some_and(|g| !g.is_empty()));
            (answer, changed)
        }
        (method::REVOKE, Some(by)) => {
            let answer = parse::<RevokeParams>(params).and_then(|p| {
                let revoked = s.revoke(&p.ids, p.app.as_deref(), by).map_err(internal)?;
                log::info!("permd: {by} revoked {revoked} grants");
                Ok(RevokeResult { revoked })
            });
            let changed = answer.as_ref().is_ok_and(|r| r.revoked > 0);
            (answer.map(to_value), changed)
        }
        (method::AUDIT, Some(_)) => {
            let answer =
                parse::<AuditParams>(if params.is_null() { json!({}) } else { params }).and_then(|p| {
                    let entries = s.audit(p.limit.unwrap_or(100)).map_err(internal)?;
                    Ok(to_value(AuditResult { entries }))
                });
            (answer, false)
        }
        (method::GRANT | method::REVOKE | method::AUDIT, None) => (only_managers(), false),
        (other, _) => (Err(error(code::BAD_REQUEST, format!("perm has no method {other:?}"))), false),
    }
}

/// An app asks for `rules`: prompt the user for whatever it doesn't hold yet.
async fn request(
    bus: &Client,
    store: &Shared,
    prompting: &Mutex<HashSet<String>>,
    who: Who,
    params: Value,
) -> Result<Value, BusError> {
    let Who::App(app) = who else {
        return Err(error(code::BAD_REQUEST, "managers grant directly, with grant"));
    };
    let p: RequestParams = parse(params)?;
    check_rules(&p.rules)?;
    if p.rules.contains(&Rule::capability(capability::PERMISSIONS)) {
        return Err(error(code::PERMISSION_DENIED, "apps can't ask for the permissions capability"));
    }
    let missing = store.lock().unwrap().missing(&app, &p.rules).map_err(internal)?;
    if missing.is_empty() {
        return Ok(to_value(RequestResult { granted: true }));
    }
    if !prompting.lock().unwrap().insert(app.clone()) {
        return Err(error(code::LIMIT_EXCEEDED, format!("{app} is already waiting for an answer")));
    }
    let answer = prompt(bus, &app, &missing, p.reason).await;
    prompting.lock().unwrap().remove(&app);
    let allow = answer?;
    if allow {
        let made = store.lock().unwrap().grant(&app, &missing, "user").map_err(internal)?;
        log::info!("permd: the user granted {app} {} things", made.len());
        publish(bus, store).await;
    } else {
        store.lock().unwrap().deny(&app, &missing).map_err(internal)?;
        log::info!("permd: the user turned down {app}");
    }
    Ok(to_value(RequestResult { granted: allow }))
}

/// Ask the user, through the compositor, whether `app` may have `rules`.
async fn prompt(bus: &Client, app: &str, rules: &[Rule], reason: Option<String>) -> Result<bool, BusError> {
    let name = app_name(bus, app).await.unwrap_or_else(|| app.to_owned());
    let params =
        PromptParams { app: app.into(), name, asks: rules.iter().map(Rule::describe).collect(), reason };
    let answer = bus
        .request_timeout(
            protocol::scene::SERVICE,
            protocol::scene::method::PROMPT,
            to_value(params),
            PROMPT_TIMEOUT,
        )
        .await
        .map_err(|e| match e {
            CallError::Bus(b) => b,
            other => error(other.code(), other.to_string()),
        })?;
    let PromptResult { allow } = parse(answer)?;
    Ok(allow)
}

/// The name app `app` was installed under, if it was.
async fn app_name(bus: &Client, app: &str) -> Option<String> {
    let q = json!({"type": "App", "where": {"id": app}, "limit": 1});
    let found = bus.request(protocol::db::SERVICE, protocol::db::method::QUERY, q).await.ok()?;
    found["docs"][0]["data"]["name"].as_str().map(str::to_owned)
}

/// Refuse rules that can't be granted, filters included.
fn check_rules(rules: &[Rule]) -> Result<(), BusError> {
    if rules.is_empty() {
        return Err(error(code::BAD_REQUEST, "no rules given"));
    }
    for rule in rules {
        if let Some(why) = rule.problem() {
            return Err(error(code::BAD_REQUEST, why));
        }
        if let Rule::Data(d) = rule {
            // A rule for every type (`*`) is checked against a stand-in type name.
            let doc_type = if d.doc_type == perm::ALL_TYPES { "Any".to_owned() } else { d.doc_type.clone() };
            let q = Query { filter: d.filter.clone(), ..Query::all(doc_type) };
            db::query::compile(&q).map_err(|e| error(code::BAD_REQUEST, format!("bad filter: {e}")))?;
        }
    }
    Ok(())
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, BusError> {
    serde_json::from_value(params).map_err(|e| error(code::BAD_REQUEST, format!("bad params: {e}")))
}

fn to_value(v: impl Serialize) -> Value {
    serde_json::to_value(v).expect("perm results serialize")
}

fn error(code: &str, message: impl Into<String>) -> BusError {
    BusError { code: code.into(), message: message.into() }
}

fn internal(e: rusqlite::Error) -> BusError {
    log::error!("permd: {e}");
    error("internal", e.to_string())
}
