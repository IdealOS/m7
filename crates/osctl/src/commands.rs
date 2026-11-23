//! The `osctl bus` subcommands, written against a connected client and an output
//! stream so tests can run them against an in-process broker.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use bus_client::{Client, Incoming, ListResult, Publication};
use protocol::Message;
use protocol::broker::TapEvent;
use protocol::bus::{Sender, TAP_TOPIC};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, PartialEq)]
pub enum Command {
    List,
    Hello,
    Call { to: String, method: String, params: Value, timeout: Option<Duration> },
    Pub { topic: String, data: Value },
    Sub { pattern: String },
    Tap,
    Echo { name: String },
    Bench { seconds: u64, size: usize },
}

/// `path` is the broker's socket, for commands that open a second connection.
pub async fn run(bus: &Client, path: &Path, command: Command, out: &mut impl Write) -> Result<(), String> {
    let w = |e: std::io::Error| e.to_string();
    match command {
        Command::List => {
            let list = bus.list().await.map_err(|e| e.to_string())?;
            write_list(&list, out).map_err(w)
        }
        Command::Hello => {
            let hello = bus.hello().await.map_err(|e| e.to_string())?;
            writeln!(out, "{}", json!(hello)).map_err(w)
        }
        Command::Call { to, method, params, timeout } => {
            let answer = match timeout {
                Some(t) => bus.request_timeout(&to, &method, params, t).await,
                None => bus.request(&to, &method, params).await,
            };
            let answer = answer.map_err(|e| e.to_string())?;
            writeln!(out, "{answer}").map_err(w)
        }
        Command::Pub { topic, data } => {
            bus.publish(&topic, data).await.map_err(|e| e.to_string())?;
            // Publishing has no answer; a round trip makes sure the broker has read
            // the publish before this process exits and closes the connection.
            bus.hello().await.map(drop).map_err(|e| e.to_string())
        }
        Command::Sub { pattern } => {
            let mut sub = bus.subscribe(&pattern).await.map_err(|e| e.to_string())?;
            while let Some(p) = sub.next().await {
                // Stdout is line-buffered, so each line shows up as it's written.
                writeln!(out, "{}", json!({"topic": p.topic, "from": p.from, "data": p.data})).map_err(w)?;
            }
            Err("the bus closed the connection".into())
        }
        Command::Tap => {
            let mut tap = bus.subscribe(TAP_TOPIC).await.map_err(|e| e.to_string())?;
            while let Some(p) = tap.next().await {
                writeln!(out, "{}", tap_line(&p)).map_err(w)?;
            }
            Err("the bus closed the connection".into())
        }
        Command::Echo { name } => {
            bus.claim(&name).await.map_err(|e| e.to_string())?;
            writeln!(out, "serving {name}").map_err(w)?;
            out.flush().map_err(w)?;
            echo(bus).await;
            Err("the bus closed the connection".into())
        }
        Command::Bench { seconds, size } => {
            let service = Client::connect(path).await.map_err(|e| e.to_string())?;
            let report = crate::bench::run(bus, service, Duration::from_secs(seconds), size).await?;
            write!(out, "{report}").map_err(w)
        }
    }
}

/// Answer every request with its params, until the connection closes.
pub async fn echo(bus: &Client) {
    let Some(mut requests) = bus.take_requests() else { return };
    while let Some(incoming) = requests.next().await {
        if let Incoming::Request { id, params, .. } = incoming
            && bus.respond(id, Ok(params)).await.is_err()
        {
            return;
        }
    }
}

fn write_list(list: &ListResult, out: &mut impl Write) -> std::io::Result<()> {
    // A BTreeMap, so already sorted by name.
    for (name, conn) in &list.services {
        writeln!(out, "{name:<16} connection {conn}")?;
    }
    writeln!(out, "{} connection(s)", list.connections)
}

/// One line per traced message: who sent it, then what it was.
pub fn tap_line(p: &Publication) -> String {
    let Ok(TapEvent { conn, sender, msg, bus_id }) = TapEvent::deserialize(&p.data) else {
        return format!("? {}", p.data);
    };
    let who = match sender {
        Sender::Service(name) => name,
        Sender::App { app, .. } => format!("app {app}"),
        Sender::Conn(_) => "conn".to_owned(),
    };
    let who = format!("{who}#{conn}");
    let compact = |v: &Value| if v.is_null() { String::new() } else { format!(" {v}") };
    match msg {
        Message::Request { id, to, method, params, .. } => {
            let routed = bus_id.map(|id| format!(" (bus id {id})")).unwrap_or_default();
            format!("{who} request {id} -> {to}.{method}{}{routed}", compact(&params))
        }
        Message::Response { id, ok: true, result, .. } => {
            format!("{who} response {id} ok{}", compact(&result.unwrap_or_default()))
        }
        Message::Response { id, error, .. } => match error {
            Some(e) => format!("{who} response {id} error {}: {}", e.code, e.message),
            None => format!("{who} response {id} error"),
        },
        Message::Publish { topic, data, .. } => format!("{who} publish {topic}{}", compact(&data)),
        Message::Subscribe { topic, .. } => format!("{who} subscribe {topic}"),
        Message::Unsubscribe { topic, .. } => format!("{who} unsubscribe {topic}"),
        Message::Cancel { id } => format!("{who} cancel {id}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tokio::net::UnixListener;

    struct Bus {
        path: PathBuf,
        dir: PathBuf,
    }

    impl Bus {
        fn start(name: &str) -> Bus {
            let dir = std::env::temp_dir().join(format!("osctl-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("bus.sock");
            tokio::spawn(busd::serve(UnixListener::bind(&path).unwrap(), Box::new(busd::AllowAll), None));
            Bus { path, dir }
        }

        async fn client(&self) -> Client {
            Client::connect(&self.path).await.unwrap()
        }
    }

    impl Drop for Bus {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    async fn output(bus: &Client, command: Command) -> Result<String, String> {
        let mut out = Vec::new();
        run(bus, Path::new("/nonexistent"), command, &mut out).await?;
        Ok(String::from_utf8(out).unwrap())
    }

    #[tokio::test]
    async fn call_list_and_echo() {
        let bus = Bus::start("call");
        let server = bus.client().await;
        server.claim("echo").await.unwrap();
        tokio::spawn(async move { echo(&server).await });
        let me = bus.client().await;

        let call = Command::Call {
            to: "echo".into(),
            method: "ping".into(),
            params: json!({"n": 1}),
            timeout: None,
        };
        assert_eq!(output(&me, call).await.unwrap(), "{\"n\":1}\n");
        let list = output(&me, Command::List).await.unwrap();
        assert!(list.starts_with("echo "), "{list}");
        assert!(list.ends_with("2 connection(s)\n"), "{list}");

        let call =
            Command::Call { to: "nobody".into(), method: "x".into(), params: Value::Null, timeout: None };
        let err = output(&me, call).await.unwrap_err();
        assert!(err.starts_with("no_such_service:"), "{err}");
    }

    #[tokio::test]
    async fn pub_reaches_a_subscriber() {
        let bus = Bus::start("pub");
        let listener = bus.client().await;
        let mut sub = listener.subscribe("news.*").await.unwrap();
        let me = bus.client().await;
        output(&me, Command::Pub { topic: "news.today".into(), data: json!("hi") }).await.unwrap();
        let p = sub.next().await.unwrap();
        assert_eq!((p.topic.as_str(), p.data), ("news.today", json!("hi")));
    }

    #[tokio::test]
    async fn bench_reports_numbers() {
        let bus = Bus::start("bench");
        let me = bus.client().await;
        let mut out = Vec::new();
        run(&me, &bus.path, Command::Bench { seconds: 1, size: 16 }, &mut out).await.unwrap();
        let text = String::from_utf8(out).unwrap();
        let last = text.lines().last().unwrap();
        assert!(last.starts_with("BENCH size=16 rtt_p50_us="), "{text}");
        let rps: f64 =
            last.split("requests_per_s=").nth(1).unwrap().split(' ').next().unwrap().parse().unwrap();
        assert!(rps > 100.0, "{text}");
    }

    #[test]
    fn tap_lines_read_as_who_did_what() {
        let line = |data: Value| {
            tap_line(&Publication { topic: TAP_TOPIC.into(), data, from: None, fds: Vec::new() })
        };
        assert_eq!(
            line(json!({"conn": 3, "sender": {"conn": 3}, "bus_id": 9,
                "msg": {"kind": "request", "id": 1, "to": "db", "method": "query", "params": {"a": 1}}})),
            r#"conn#3 request 1 -> db.query {"a":1} (bus id 9)"#
        );
        assert_eq!(
            line(
                json!({"conn": 2, "sender": {"service": "db"}, "msg": {"kind": "response", "id": 9, "ok": true, "result": []}})
            ),
            "db#2 response 9 ok []"
        );
        assert_eq!(
            line(json!({"conn": 4, "sender": {"app": {"app": "org.x", "conn": 4}},
                "msg": {"kind": "response", "id": 2, "ok": false, "error": {"code": "timeout", "message": "slow"}}})),
            "app org.x#4 response 2 error timeout: slow"
        );
        assert_eq!(
            line(
                json!({"conn": 2, "sender": {"service": "db"}, "msg": {"kind": "publish", "topic": "db.live.1", "data": null}})
            ),
            "db#2 publish db.live.1"
        );
        assert_eq!(line(json!({"not": "a tap event"})), r#"? {"not":"a tap event"}"#);
    }
}
