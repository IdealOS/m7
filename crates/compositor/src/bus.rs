//! The compositor on the bus.
//!
//! The frame loop must never wait on I/O, so the bus lives on its own thread with
//! a small tokio runtime. It claims `compositor`, passes each request and cancel to
//! the frame loop as a [`Call`] (then calls the backend's `wake`, so a sleeping loop
//! notices), and sends back the [`Out`]s the loop produces.
//!
//! Without a bus (hosted development with no `busd` running) the compositor still
//! runs; it just has no app windows.

use std::sync::mpsc;
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
pub fn connect(wake: impl Fn() + Send + 'static) -> Option<Bridge> {
    connect_with(None, wake)
}

/// Like [`connect`], to the broker at `socket` if given (for tests).
pub fn connect_with(socket: Option<std::path::PathBuf>, wake: impl Fn() + Send + 'static) -> Option<Bridge> {
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
                let sender = bus.clone();
                tokio::spawn(async move {
                    while let Some(out) = out_rx.recv().await {
                        let sent = match out {
                            Out::Part { id, data } => sender.stream(id, data).await,
                            Out::Respond { id, result } => sender.respond(id, result).await,
                        };
                        if let Err(e) = sent {
                            log::warn!("bus: couldn't answer an app: {e}");
                        }
                    }
                });
                while let Some(incoming) = requests.next().await {
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

async fn connect_and_claim(socket: Option<std::path::PathBuf>) -> Result<Client, String> {
    let bus = match socket {
        Some(path) => Client::connect(path).await,
        None => Client::connect_default().await,
    }
    .map_err(|e| e.to_string())?;
    bus.claim(SERVICE).await.map_err(|e| format!("cannot claim {SERVICE}: {e}"))?;
    Ok(bus)
}
