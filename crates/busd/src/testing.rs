//! A broker for other crates' tests, holding session's connection so it can hand
//! out app connections the way session does.

use std::path::{Path, PathBuf};

use bus_client::Client;
use serde_json::json;
use tokio::net::UnixListener;

use crate::Policy;

pub struct TestBus {
    pub socket: PathBuf,
    session: Client,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl TestBus {
    /// Serve a broker with `policy` on a new socket at `socket`.
    pub async fn start(socket: &Path, policy: Box<dyn Policy>) -> TestBus {
        let _ = std::fs::remove_file(socket);
        let (session_end, broker_end) = tokio::net::UnixStream::pair().expect("socket pair");
        let listener = UnixListener::bind(socket).expect("bind the test bus");
        let server = tokio::spawn(crate::serve(listener, policy, Some(broker_end)));
        TestBus { socket: socket.to_owned(), session: Client::from_stream(session_end), server }
    }

    /// A connection as a system service or tool.
    pub async fn system(&self) -> Client {
        Client::connect(&self.socket).await.expect("connect to the test bus")
    }

    /// A connection identified as app `id`, as session makes them.
    pub async fn app(&self, id: &str) -> Client {
        let (app_end, for_broker) = std::os::unix::net::UnixStream::pair().expect("socket pair");
        self.session
            .request_with("bus", "adopt", json!({"app": id}), vec![for_broker.into()], None)
            .await
            .expect("the broker adopts the app connection");
        app_end.set_nonblocking(true).expect("non-blocking");
        Client::from_stream(tokio::net::UnixStream::from_std(app_end).expect("a tokio stream"))
    }
}

impl Drop for TestBus {
    fn drop(&mut self) {
        self.server.abort();
    }
}
