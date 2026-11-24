//! The `secrets` service: secrets kept for whoever stored them.
//!
//! A secret's owner is the identity the broker stamped on the request that
//! stored it: `service:NAME` for a system service, `app:ID` for an app. Nothing
//! in the request names an owner, so no one can reach another's secrets. Tools
//! (connections without a name) are refused: they have no lasting identity to
//! keep secrets under.

pub mod store;

use bus_client::{Client, Incoming};
use protocol::BusError;
use protocol::bus::{Sender, code};
use protocol::secrets::{GetResult, ListResult, NameParams, PutParams, method};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

pub use store::Store;

/// Whose secrets a request is about.
pub fn owner(from: Option<&Sender>) -> Option<String> {
    match from? {
        Sender::Service(name) => Some(format!("service:{name}")),
        Sender::App { app, .. } => Some(format!("app:{app}")),
        Sender::Conn(_) => None,
    }
}

/// Answer one request from `from`.
pub fn handle(
    store: &mut Store,
    from: Option<&Sender>,
    method: &str,
    params: Value,
) -> Result<Value, BusError> {
    let Some(owner) = owner(from) else {
        return Err(error(
            code::PERMISSION_DENIED,
            "secrets belong to services and apps; tools can't keep any",
        ));
    };
    match method {
        method::PUT => {
            let p: PutParams = parse(params)?;
            store.put(&owner, &p.name, &p.value).map(|()| Value::Null).map_err(err)
        }
        method::GET => {
            let p: NameParams = parse(params)?;
            store.get(&owner, &p.name).map(|value| to_value(GetResult { value })).map_err(err)
        }
        method::DELETE => {
            let p: NameParams = parse(params)?;
            store.delete(&owner, &p.name).map(|()| Value::Null).map_err(err)
        }
        method::LIST => store.list(&owner).map(|names| to_value(ListResult { names })).map_err(err),
        other => Err(error(code::BAD_REQUEST, format!("secrets has no method {other:?}"))),
    }
}

/// Serve `store` on `bus`, which must already have claimed `secrets`.
pub async fn serve(bus: Client, mut store: Store) -> Result<(), String> {
    let Some(mut requests) = bus.take_requests() else { return Ok(()) };
    while let Some(incoming) = requests.next().await {
        let Incoming::Request { id, from, method, params, .. } = incoming else { continue };
        let answer = handle(&mut store, from.as_ref(), &method, params);
        // Never log values; names and owners only.
        if let Err(e) = &answer {
            log::debug!("secretsd: {method} from {from:?}: {}", e.message);
        }
        let _ = bus.respond(id, answer).await;
    }
    Ok(())
}

fn err(e: store::Error) -> BusError {
    match e {
        store::Error::Invalid(why) => error(code::BAD_REQUEST, why),
        store::Error::NotFound => error("not_found", "no such secret"),
        store::Error::Internal(why) => {
            log::error!("secretsd: {why}");
            error("internal", why)
        }
    }
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, BusError> {
    serde_json::from_value(params).map_err(|e| error(code::BAD_REQUEST, format!("bad params: {e}")))
}

fn to_value(v: impl Serialize) -> Value {
    serde_json::to_value(v).expect("results serialize")
}

fn error(code: &str, message: impl Into<String>) -> BusError {
    BusError { code: code.into(), message: message.into() }
}
