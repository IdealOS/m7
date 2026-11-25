//! The compositor on the bus.
//!
//! The frame loop must never wait on I/O, so the bus lives on its own thread with
//! a small tokio runtime. It claims `compositor`, passes each request and cancel to
//! the frame loop as a [`Call`] (then calls the backend's `wake`, so a sleeping loop
//! notices), and sends back the [`Out`]s the loop produces. The compositor's own
//! requests (loading bound fields, listing and launching apps) go out the same
//! way, and their answers come back as [`Call::Reply`].
//!
//! Without a bus (hosted development with no `busd` running) the compositor still
//! runs; it just has no app windows.

use std::sync::{Arc, mpsc};
use std::time::Duration;

use bus_client::{Client, Incoming};
use protocol::scene::SERVICE;

use crate::desktop::{Call, Out};

/// The frame loop's end of the bridge.
pub struct Bridge {
    calls: mpsc::Receiver<Call>,
    out: tokio::sync::mpsc::UnboundedSender<Out>,
}

impl Bridge {
    /// Requests waiting for the frame loop.
    pub fn calls(&self) -> mpsc::TryIter<'_, Call> {
        self.calls.try_iter()
    }

    /// Send answers and window events back to apps.
    pub fn send(&self, outs: impl IntoIterator<Item = Out>) {
        for out in outs {
            let _ = self.out.send(out);
        }
    }
}

/// Connect to the bus (the connection session handed over, or the default socket)
/// and claim `compositor`. `wake` is called after each request is queued. Returns
/// `None`, after logging why, if there's no bus.
pub fn connect(wake: impl Fn() + Send + Sync + 'static) -> Option<Bridge> {
    connect_with(None, wake)
}

/// Like [`connect`], to the broker at `socket` if given (for tests).
pub fn connect_with(
    socket: Option<std::path::PathBuf>,
    wake: impl Fn() + Send + Sync + 'static,
) -> Option<Bridge> {
    let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
    let (calls_tx, calls) = mpsc::channel();
    let (out, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<Out>();
    let (ready_tx, ready) = mpsc::channel::<Result<(), String>>();
    std::thread::Builder::new()
        .name("bus".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(r) => r,
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return;
                }
            };
            runtime.block_on(async move {
                let bus = match connect_and_claim(socket).await {
                    Ok(bus) => bus,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));
                let Some(mut requests) = bus.take_requests() else { return };
                tokio::spawn(follow_grants(bus.clone(), calls_tx.clone(), wake.clone()));
                let sender = bus.clone();
                let replies = calls_tx.clone();
                let woken = wake.clone();
                // Terminal attachments in flight, by node.
                let mut attached: std::collections::HashMap<protocol::NodeId, tokio::task::JoinHandle<()>> =
                    std::collections::HashMap::new();
                // When the frame loop drops its `Bridge`, the outgoing queue closes; that
                // ends this thread, which closes the connection and frees the name.
                let (gone_tx, mut gone) = tokio::sync::oneshot::channel::<()>();
                tokio::spawn(async move {
                    let _gone = gone_tx;
                    while let Some(out) = out_rx.recv().await {
                        let sent = match out {
                            Out::Part { id, data } => sender.stream(id, data).await,
                            Out::Respond { id, result } => sender.respond(id, result).await,
                            Out::FetchBlob { hash, app } => {
                                let (bus, replies, woken) = (sender.clone(), replies.clone(), woken.clone());
                                tokio::spawn(async move {
                                    let result = fetch_blob(&bus, &hash, app.as_deref()).await;
                                    if replies.send(Call::Blob { hash, app, result }).is_ok() {
                                        woken();
                                    }
                                });
                                Ok(())
                            }
                            Out::Attach { node, params } => {
                                let (bus, replies, woken) = (sender.clone(), replies.clone(), woken.clone());
                                let task = tokio::spawn(async move {
                                    match bus
                                        .request_stream(
                                            protocol::pty::SERVICE,
                                            protocol::pty::method::ATTACH,
                                            params,
                                        )
                                        .await
                                    {
                                        Ok(mut frames) => {
                                            while let Some(Ok(frame)) = frames.next().await {
                                                if replies.send(Call::Screen { node, frame }).is_err() {
                                                    return;
                                                }
                                                woken();
                                            }
                                        }
                                        Err(e) => log::warn!("bus: couldn't attach a terminal: {e}"),
                                    }
                                    if replies.send(Call::ScreenEnd { node }).is_ok() {
                                        woken();
                                    }
                                });
                                if let Some(old) = attached.insert(node, task) {
                                    old.abort();
                                }
                                attached.retain(|_, t| !t.is_finished());
                                Ok(())
                            }
                            Out::Detach { node } => {
                                // Dropping the stream (aborting its task) tells ptyd.
                                if let Some(task) = attached.remove(&node) {
                                    task.abort();
                                }
                                Ok(())
                            }
                            Out::Request { token, to, method, params } => {
                                // Answered in its own task, so a slow service doesn't
                                // hold up answers to apps.
                                let (bus, replies, woken) = (sender.clone(), replies.clone(), woken.clone());
                                tokio::spawn(async move {
                                    let result =
                                        bus.request(&to, &method, params).await.map_err(|e| match e {
                                            bus_client::CallError::Bus(b) => b,
                                            other => protocol::BusError {
                                                code: other.code().into(),
                                                message: other.to_string(),
                                            },
                                        });
                                    if replies.send(Call::Reply { token, result }).is_ok() {
                                        woken();
                                    }
                                });
                                Ok(())
                            }
                        };
                        if let Err(e) = sent {
                            log::warn!("bus: couldn't answer an app: {e}");
                        }
                    }
                });
                loop {
                    let incoming = tokio::select! {
                        incoming = requests.next() => incoming,
                        _ = &mut gone => return,
                    };
                    let Some(incoming) = incoming else { break };
                    let call = match incoming {
                        Incoming::Request { id, from, method, params, stream, .. } => {
                            Call::Request { id, from, method, params, stream }
                        }
                        Incoming::Cancel { id } => Call::Cancel { id },
                    };
                    if calls_tx.send(call).is_err() {
                        break; // the frame loop has gone
                    }
                    wake();
                }
                log::warn!("bus: the connection closed; app windows will stop updating");
            });
        })
        .ok()?;
    match ready.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(())) => {
            log::info!("bus: serving {SERVICE}");
            Some(Bridge { calls, out })
        }
        Ok(Err(e)) => {
            log::warn!("bus: {e}; running without app windows");
            None
        }
        Err(_) => {
            log::warn!("bus: no answer from the broker; running without app windows");
            None
        }
    }
}

/// Pass permd's grant tables to the frame loop: the current one, then each change.
/// (The desktop keeps the newest, by version.)
async fn follow_grants(bus: Client, calls: mpsc::Sender<Call>, wake: Arc<dyn Fn() + Send + Sync>) {
    use protocol::perm::{self, GrantsResult};
    let mut sub = match bus.subscribe(perm::GRANTS_TOPIC).await {
        Ok(s) => s,
        Err(e) => {
            log::warn!("bus: cannot follow grants: {e}; apps get no capabilities");
            return;
        }
    };
    let pass = |table: GrantsResult| {
        let sent = calls.send(Call::Grants(table)).is_ok();
        if sent {
            wake();
        }
        sent
    };
    if let Ok(v) = bus.request(perm::SERVICE, perm::method::GRANTS, serde_json::json!({})).await
        && let Ok(table) = serde_json::from_value(v)
        && !pass(table)
    {
        return;
    }
    while let Some(p) = sub.next().await {
        if p.from != Some(protocol::Sender::Service(perm::SERVICE.into())) {
            continue;
        }
        if let Ok(table) = serde_json::from_value(p.data)
            && !pass(table)
        {
            return;
        }
    }
}

/// Blob `hash`'s bytes, read from the descriptor `get_blob` answers with. For
/// an app's image, `db` checks the app may read it.
async fn fetch_blob(bus: &Client, hash: &str, app: Option<&str>) -> Result<Vec<u8>, String> {
    let mut params = serde_json::json!({"hash": hash});
    if let Some(app) = app {
        params["as"] = app.into();
    }
    let reply =
        bus.request_with("db", "get_blob", params, Vec::new(), None).await.map_err(|e| e.to_string())?;
    let fd = reply.fds.into_iter().next().ok_or("get_blob sent no descriptor")?;
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::from(fd)
            .take(crate::image::MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() as u64 > crate::image::MAX_BYTES {
            return Err("the image is too big".into());
        }
        Ok(bytes)
    })
    .await
    .map_err(|e| e.to_string())?
}

async fn connect_and_claim(socket: Option<std::path::PathBuf>) -> Result<Client, String> {
    let bus = match socket {
        Some(path) => Client::connect(path).await,
        None => Client::connect_default().await,
    }
    .map_err(|e| e.to_string())?;
    bus.claim(SERVICE).await.map_err(|e| format!("cannot claim {SERVICE}: {e}"))?;
    Ok(bus)
}
