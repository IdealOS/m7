//! The `net` service: Wi-Fi and connection status, over NetworkManager ([`nm`])
//! or, for development and tests, a pretend network ([`fake`]).
//!
//! Requests run in their own tasks (joining a network can take seconds). `watch`
//! streams the status when it starts and whenever it changes: netd checks every
//! [`POLL`], and at once after anything it was asked to change.

pub mod fake;
pub mod nm;

use std::sync::Arc;
use std::time::Duration;

use bus_client::{Client, Incoming};
use protocol::BusError;
use protocol::bus::code as bus_code;
use protocol::net::{
    ConnectParams, NetState, NetStatus, NetworksParams, NetworksResult, SsidParams, WifiParams, code, method,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::{Mutex, Notify};

/// How often `watch` checks for changes nobody asked netd for.
pub const POLL: Duration = Duration::from_secs(3);

/// Why a request failed.
#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    pub code: &'static str,
    pub message: String,
}

impl Error {
    pub fn unavailable(m: impl Into<String>) -> Error {
        Error { code: code::UNAVAILABLE, message: m.into() }
    }
    pub fn failed(m: impl Into<String>) -> Error {
        Error { code: code::CONNECT_FAILED, message: m.into() }
    }
    pub fn not_found(m: impl Into<String>) -> Error {
        Error { code: code::NOT_FOUND, message: m.into() }
    }
}

impl From<Error> for BusError {
    fn from(e: Error) -> BusError {
        BusError { code: e.code.into(), message: e.message }
    }
}

/// Where netd gets its network from.
pub enum Backend {
    Nm(nm::Nm),
    Fake(fake::Fake),
    /// No NetworkManager: every status says so.
    None,
}

impl Backend {
    pub async fn status(&self) -> Result<NetStatus, Error> {
        match self {
            Backend::Nm(nm) => nm.status().await,
            Backend::Fake(f) => Ok(f.status()),
            Backend::None => Ok(NetStatus {
                state: NetState::Unavailable,
                connections: Vec::new(),
                wifi_enabled: false,
                wifi_present: false,
            }),
        }
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, BusError> {
        match method {
            method::STATUS => Ok(to_value(self.status().await?)),
            method::NETWORKS => {
                let p: NetworksParams = parse(if params.is_null() { serde_json::json!({}) } else { params })?;
                let networks = match self {
                    Backend::Nm(nm) => nm.networks(p.rescan).await?,
                    Backend::Fake(f) => f.networks()?,
                    Backend::None => Vec::new(),
                };
                Ok(to_value(NetworksResult { networks }))
            }
            method::CONNECT => {
                let p: ConnectParams = parse(params)?;
                match self {
                    Backend::Nm(nm) => nm.connect_wifi(&p.ssid, p.password.as_deref()).await?,
                    Backend::Fake(f) => f.connect(&p.ssid, p.password.as_deref())?,
                    Backend::None => return Err(Error::unavailable("no NetworkManager").into()),
                }
                log::info!("netd: joined {:?}", p.ssid);
                Ok(Value::Null)
            }
            method::DISCONNECT => {
                match self {
                    Backend::Nm(nm) => nm.disconnect().await?,
                    Backend::Fake(f) => f.disconnect(),
                    Backend::None => return Err(Error::unavailable("no NetworkManager").into()),
                }
                Ok(Value::Null)
            }
            method::FORGET => {
                let p: SsidParams = parse(params)?;
                match self {
                    Backend::Nm(nm) => nm.forget(&p.ssid).await?,
                    Backend::Fake(f) => f.forget(&p.ssid)?,
                    Backend::None => return Err(Error::unavailable("no NetworkManager").into()),
                }
                Ok(Value::Null)
            }
            method::SET_WIFI => {
                let p: WifiParams = parse(params)?;
                match self {
                    Backend::Nm(nm) => nm.set_wifi(p.enabled).await?,
                    Backend::Fake(f) => f.set_wifi(p.enabled),
                    Backend::None => return Err(Error::unavailable("no NetworkManager").into()),
                }
                Ok(Value::Null)
            }
            other => Err(BusError {
                code: bus_code::BAD_REQUEST.into(),
                message: format!("net has no method {other:?}"),
            }),
        }
    }
}

/// Serve `backend` on `bus`, which must already have claimed `net`.
pub async fn serve(bus: Client, backend: Backend) -> Result<(), String> {
    let Some(mut requests) = bus.take_requests() else { return Ok(()) };
    let backend = Arc::new(backend);
    let watchers: Arc<Mutex<Vec<u64>>> = Arc::default();
    let changed = Arc::new(Notify::new());
    // Push the status to watchers when it changes.
    let watch_task = {
        let (bus, backend, watchers, changed) =
            (bus.clone(), backend.clone(), watchers.clone(), changed.clone());
        tokio::spawn(async move {
            let mut last: Option<NetStatus> = backend.status().await.ok();
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(POLL) => {}
                    _ = changed.notified() => {}
                }
                let Ok(now) = backend.status().await else { continue };
                if last.as_ref() != Some(&now) {
                    let v = to_value(&now);
                    for &w in watchers.lock().await.iter() {
                        let _ = bus.stream(w, v.clone()).await;
                    }
                    last = Some(now);
                }
            }
        })
    };
    while let Some(incoming) = requests.next().await {
        match incoming {
            Incoming::Request { id, method, stream: true, .. } if method == method::WATCH => {
                let status = backend.status().await.map(to_value);
                match status {
                    Ok(v) => {
                        let _ = bus.stream(id, v).await;
                        watchers.lock().await.push(id);
                    }
                    Err(e) => {
                        let _ = bus.respond(id, Err(e.into())).await;
                    }
                }
            }
            Incoming::Request { id, method, .. } if method == method::WATCH => {
                let e = BusError {
                    code: bus_code::BAD_REQUEST.into(),
                    message: "watch must be sent as a streamed request".into(),
                };
                let _ = bus.respond(id, Err(e)).await;
            }
            Incoming::Request { id, method, params, .. } => {
                let (bus, backend, changed) = (bus.clone(), backend.clone(), changed.clone());
                tokio::spawn(async move {
                    let changes = !matches!(method.as_str(), method::STATUS | method::NETWORKS);
                    let answer = backend.call(&method, params).await;
                    let _ = bus.respond(id, answer).await;
                    if changes {
                        changed.notify_one();
                    }
                });
            }
            Incoming::Cancel { id } => watchers.lock().await.retain(|w| *w != id),
        }
    }
    watch_task.abort();
    Ok(())
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, BusError> {
    serde_json::from_value(params)
        .map_err(|e| BusError { code: bus_code::BAD_REQUEST.into(), message: format!("bad params: {e}") })
}

fn to_value(v: impl Serialize) -> Value {
    serde_json::to_value(v).expect("results serialize")
}
