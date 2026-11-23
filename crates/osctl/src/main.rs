//! `osctl`: command-line tool for a running idealos system.
//!
//! ```text
//! osctl [--socket PATH] bus list                      services and connection count
//! osctl [--socket PATH] bus hello                     this connection's number and identity
//! osctl [--socket PATH] bus call TO METHOD [JSON] [--timeout MS]
//! osctl [--socket PATH] bus pub TOPIC [JSON]
//! osctl [--socket PATH] bus sub PATTERN               print publications as JSON lines
//! osctl [--socket PATH] bus tap                       print every message the broker sees
//! osctl [--socket PATH] bus echo NAME                 serve NAME, answering with the params
//! osctl [--socket PATH] bus bench [--seconds N] [--size BYTES]
//! osctl [--socket PATH] db create TYPE [JSON]         create a document; prints it
//! osctl [--socket PATH] db get ID
//! osctl [--socket PATH] db update ID JSON             apply a JSON merge patch
//! osctl [--socket PATH] db delete ID
//! osctl [--socket PATH] db query JSON                 e.g. '{"type": "Note", "where": {"done": false}}'
//! osctl [--socket PATH] db search WORDS... [--type TYPE] [--limit N]
//! osctl [--socket PATH] db history ID                 every change to a document
//! osctl [--socket PATH] db at ID CHANGE_SET           a document as it was then
//! osctl [--socket PATH] db changes [--limit N]        recent change sets
//! osctl [--socket PATH] db undo CHANGE_SET            revert a change set (undo an undo to redo)
//! ```
//!
//! Without `--socket`, osctl uses the bus connection session gave it, if it was
//! started as a `bus=` service, and otherwise the default socket.
//!
//! Exit status: 0 on success, 1 when the bus or a service answers with an error or
//! can't be reached, 2 for bad usage.

mod bench;
mod commands;

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use commands::Command;

const USAGE: &str = "usage: osctl [--socket PATH] bus (list | hello | call TO METHOD [JSON] [--timeout MS] | pub TOPIC [JSON] \
| sub PATTERN | tap | echo NAME | bench [--seconds N] [--size BYTES])
       osctl [--socket PATH] db (create TYPE [JSON] | get ID | update ID JSON | delete ID | query JSON \
| search WORDS... [--type TYPE] [--limit N] | history ID | at ID CHANGE_SET | changes [--limit N] | undo CHANGE_SET)";

fn main() {
    let (socket, command) = match parse(std::env::args().skip(1).collect()) {
        Ok(parsed) => parsed,
        Err(e) => {
            eprintln!("osctl: {e}\n{USAGE}");
            std::process::exit(2);
        }
    };
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    let result = runtime.block_on(async {
        let bus = match &socket {
            Some(path) => bus_client::Client::connect(path).await,
            None => bus_client::Client::connect_default().await,
        }
        .map_err(|e| e.to_string())?;
        let path = socket.unwrap_or_else(protocol::bus::default_socket_path);
        let mut out = std::io::stdout().lock();
        commands::run(&bus, &path, command, &mut out).await
    });
    if let Err(e) = result {
        let _ = std::io::stdout().flush();
        eprintln!("osctl: {e}");
        std::process::exit(1);
    }
}

fn parse(args: Vec<String>) -> Result<(Option<PathBuf>, Command), String> {
    let mut args = args.into_iter().peekable();
    let mut socket = None;
    if args.peek().map(String::as_str) == Some("--socket") {
        args.next();
        socket = Some(args.next().ok_or("--socket needs a path")?.into());
    }
    let group = match args.next() {
        Some(g) if g == "bus" || g == "db" => g,
        Some(other) => return Err(format!("unknown command {other}")),
        None => return Err("missing command".into()),
    };
    let sub = args.next().ok_or(format!("missing {group} subcommand"))?;
    // Split the rest into positionals and --flag value pairs.
    let mut positional = Vec::new();
    let mut flags = Vec::new();
    while let Some(arg) = args.next() {
        if let Some(flag) = arg.strip_prefix("--") {
            let value = args.next().ok_or(format!("--{flag} needs a value"))?;
            flags.push((flag.to_owned(), value));
        } else {
            positional.push(arg);
        }
    }
    let number = |name: &str, default: u64| -> Result<u64, String> {
        match flags.iter().find(|(f, _)| f == name) {
            Some((_, v)) => v.parse().map_err(|_| format!("--{name} must be a whole number, not {v}")),
            None => Ok(default),
        }
    };
    let allowed = |names: &[&str]| -> Result<(), String> {
        match flags.iter().find(|(f, _)| !names.contains(&f.as_str())) {
            Some((f, _)) => Err(format!("{group} {sub} has no --{f}")),
            None => Ok(()),
        }
    };
    let json = |text: Option<&String>| -> Result<serde_json::Value, String> {
        match text {
            None => Ok(serde_json::Value::Null),
            Some(t) => serde_json::from_str(t).map_err(|e| format!("not JSON: {t}: {e}")),
        }
    };
    let arity = |min: usize, max: usize| -> Result<(), String> {
        if positional.len() < min || positional.len() > max {
            Err(format!(
                "{group} {sub} takes {} argument(s)",
                if min == max { min.to_string() } else { format!("{min} to {max}") }
            ))
        } else {
            Ok(())
        }
    };
    if group == "db" {
        return Ok((socket, db_command(&sub, &positional, &flags, arity, allowed, json)?));
    }
    let command = match sub.as_str() {
        "list" => {
            arity(0, 0)?;
            allowed(&[])?;
            Command::List
        }
        "hello" => {
            arity(0, 0)?;
            allowed(&[])?;
            Command::Hello
        }
        "call" => {
            arity(2, 3)?;
            allowed(&["timeout"])?;
            let timeout =
                flags.iter().any(|(f, _)| f == "timeout").then(|| number("timeout", 0)).transpose()?;
            Command::Call {
                to: positional[0].clone(),
                method: positional[1].clone(),
                params: json(positional.get(2))?,
                timeout: timeout.map(Duration::from_millis),
            }
        }
        "pub" => {
            arity(1, 2)?;
            allowed(&[])?;
            Command::Pub { topic: positional[0].clone(), data: json(positional.get(1))? }
        }
        "sub" => {
            arity(1, 1)?;
            allowed(&[])?;
            Command::Sub { pattern: positional[0].clone() }
        }
        "tap" => {
            arity(0, 0)?;
            allowed(&[])?;
            Command::Tap
        }
        "echo" => {
            arity(1, 1)?;
            allowed(&[])?;
            Command::Echo { name: positional[0].clone() }
        }
        "bench" => {
            arity(0, 0)?;
            allowed(&["seconds", "size"])?;
            Command::Bench { seconds: number("seconds", 2)?.max(1), size: number("size", 64)? as usize }
        }
        other => return Err(format!("unknown bus subcommand {other}")),
    };
    Ok((socket, command))
}

fn change_set(text: &str) -> Result<i64, String> {
    text.parse().map_err(|_| format!("a change set is a number, not {text}"))
}

/// `osctl db ...` is shorthand for a request to the db service.
fn db_command(
    sub: &str,
    positional: &[String],
    flags: &[(String, String)],
    arity: impl Fn(usize, usize) -> Result<(), String>,
    allowed: impl Fn(&[&str]) -> Result<(), String>,
    json: impl Fn(Option<&String>) -> Result<serde_json::Value, String>,
) -> Result<Command, String> {
    use protocol::db::method;
    use serde_json::json;
    let object = |text: Option<&String>| -> Result<serde_json::Value, String> {
        match json(text)? {
            serde_json::Value::Null => Ok(json!({})),
            v @ serde_json::Value::Object(_) => Ok(v),
            other => Err(format!("expected a JSON object, not {other}")),
        }
    };
    let (method, params) = match sub {
        "create" => {
            arity(1, 2)?;
            allowed(&[])?;
            (method::CREATE, json!({"type": positional[0], "data": object(positional.get(1))?}))
        }
        "get" | "delete" => {
            arity(1, 1)?;
            allowed(&[])?;
            (if sub == "get" { method::GET } else { method::DELETE }, json!({"id": positional[0]}))
        }
        "update" => {
            arity(2, 2)?;
            allowed(&[])?;
            (method::UPDATE, json!({"id": positional[0], "patch": object(positional.get(1))?}))
        }
        "query" => {
            arity(1, 1)?;
            allowed(&[])?;
            (method::QUERY, object(positional.first())?)
        }
        "search" => {
            if positional.is_empty() {
                return Err("db search needs words to look for".into());
            }
            allowed(&["type", "limit"])?;
            let mut params = json!({"text": positional.join(" ")});
            for (flag, value) in flags {
                params[flag.as_str()] = match flag.as_str() {
                    "limit" => json!(
                        value
                            .parse::<u32>()
                            .map_err(|_| format!("--limit must be a whole number, not {value}"))?
                    ),
                    _ => json!(value),
                };
            }
            (method::SEARCH, params)
        }
        "history" => {
            arity(1, 1)?;
            allowed(&[])?;
            (method::HISTORY, json!({"id": positional[0]}))
        }
        "at" => {
            arity(2, 2)?;
            allowed(&[])?;
            (method::AT, json!({"id": positional[0], "change_set": change_set(&positional[1])?}))
        }
        "changes" => {
            arity(0, 0)?;
            allowed(&["limit"])?;
            let mut params = json!({});
            if let Some((_, v)) = flags.iter().find(|(f, _)| f == "limit") {
                params["limit"] =
                    json!(v.parse::<u32>().map_err(|_| format!("--limit must be a whole number, not {v}"))?);
            }
            (method::CHANGES, params)
        }
        "undo" => {
            arity(1, 1)?;
            allowed(&[])?;
            (method::UNDO, json!({"change_set": change_set(&positional[0])?}))
        }
        other => return Err(format!("unknown db subcommand {other}")),
    };
    Ok(Command::Call { to: protocol::db::SERVICE.into(), method: method.into(), params, timeout: None })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse_str(line: &str) -> Result<(Option<PathBuf>, Command), String> {
        parse(line.split(' ').map(str::to_owned).collect())
    }

    #[test]
    fn parses_each_subcommand() {
        assert_eq!(parse_str("bus list").unwrap(), (None, Command::List));
        assert_eq!(parse_str("bus hello").unwrap().1, Command::Hello);
        assert_eq!(
            parse_str("--socket /tmp/b.sock bus call db query {\"a\":1} --timeout 250").unwrap(),
            (
                Some("/tmp/b.sock".into()),
                Command::Call {
                    to: "db".into(),
                    method: "query".into(),
                    params: json!({"a": 1}),
                    timeout: Some(Duration::from_millis(250)),
                }
            )
        );
        assert_eq!(
            parse_str("bus call echo ping").unwrap().1,
            Command::Call { to: "echo".into(), method: "ping".into(), params: json!(null), timeout: None }
        );
        assert_eq!(
            parse_str("bus pub a.b 3").unwrap().1,
            Command::Pub { topic: "a.b".into(), data: json!(3) }
        );
        assert_eq!(parse_str("bus sub a.*").unwrap().1, Command::Sub { pattern: "a.*".into() });
        assert_eq!(parse_str("bus tap").unwrap().1, Command::Tap);
        assert_eq!(parse_str("bus echo e").unwrap().1, Command::Echo { name: "e".into() });
        assert_eq!(parse_str("bus bench --size 1024").unwrap().1, Command::Bench { seconds: 2, size: 1024 });
    }

    fn db(method: &str, params: serde_json::Value) -> Command {
        Command::Call { to: "db".into(), method: method.into(), params, timeout: None }
    }

    #[test]
    fn db_subcommands_are_requests_to_the_db_service() {
        assert_eq!(parse_str("db create Note").unwrap().1, db("create", json!({"type": "Note", "data": {}})));
        assert_eq!(
            parse_str("db create Note {\"t\":1}").unwrap().1,
            db("create", json!({"type": "Note", "data": {"t": 1}}))
        );
        assert_eq!(parse_str("db get abc").unwrap().1, db("get", json!({"id": "abc"})));
        assert_eq!(parse_str("db delete abc").unwrap().1, db("delete", json!({"id": "abc"})));
        assert_eq!(
            parse_str("db update abc {\"t\":null}").unwrap().1,
            db("update", json!({"id": "abc", "patch": {"t": null}}))
        );
        assert_eq!(
            parse_str("db query {\"type\":\"Note\"}").unwrap().1,
            db("query", json!({"type": "Note"}))
        );
        assert_eq!(
            parse_str("db search quarterly report --type Email --limit 5").unwrap().1,
            db("search", json!({"text": "quarterly report", "type": "Email", "limit": 5}))
        );
        for bad in [
            "db",
            "db get",
            "db update abc",
            "db create Note [1]",
            "db search",
            "db search x --limit y",
            "db drop x",
            "db get a --type x",
        ] {
            assert!(parse_str(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn rejects_bad_usage() {
        for bad in [
            "",
            "db list",
            "bus",
            "bus frobnicate",
            "bus list extra",
            "bus call db",
            "bus call db query {not json",
            "bus call db query --timeout soon",
            "bus list --timeout 5",
            "bus bench --seconds",
            "--socket",
        ] {
            assert!(parse_str(bad).is_err(), "{bad:?} should be refused");
        }
    }
}
