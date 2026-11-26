//! `osctl`: command-line tool for a running idealos system.
//!
//! ```text
//! osctl [--socket PATH] bus list                      services and connection count
//! osctl [--socket PATH] bus hello                     this connection's number and identity
//! osctl [--socket PATH] bus call TO METHOD [JSON] [--timeout MS]
//! osctl [--socket PATH] bus stream TO METHOD [JSON] [--count N]   a streamed request; parts as JSON lines
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
//! osctl [--socket PATH] db watch JSON [--count N]     a live query: its result, then changes, as JSON lines
//! osctl [--socket PATH] db diff ID FROM TO            fields changed between two change sets (FROM 0: creation)
//! osctl [--socket PATH] db link ID FIELD TARGET        set FIELD of ID to a link to TARGET
//! osctl [--socket PATH] db links ID                    links out of and into a document
//! osctl [--socket PATH] db schemas                     the system types' JSON Schemas
//! osctl [--socket PATH] db snapshot                    take a snapshot now
//! osctl [--socket PATH] db snapshots                   list snapshots, newest first
//! osctl [--socket PATH] db restore NAME                replace the database with a snapshot (saving the current state)
//! osctl [--socket PATH] db pin ID CHANGE_SET          keep a version through history compaction
//! osctl [--socket PATH] db unpin ID CHANGE_SET
//! osctl [--socket PATH] db compact                     compact history now (dbd does daily)
//! osctl [--socket PATH] db put-blob FILE               store a file as a blob; prints its hash
//! osctl [--socket PATH] db get-blob HASH [OUT]         write a blob to OUT (default: standard output)
//! osctl [--socket PATH] db blob-info HASH
//! osctl [--socket PATH] app install DIR [--watch]     build (if DIR is sources), install and launch an app
//! osctl [--socket PATH] app list                      installed apps, and which are running
//! osctl [--socket PATH] app launch ID                 (re)start an installed app through session
//! osctl [--socket PATH] app stop ID
//! osctl [--socket PATH] perm list [APP]              grants: every app's, or one app's
//! osctl [--socket PATH] perm grant APP RULE...       e.g. '{"type": "Note", "access": ["read"]}' '{"capability": "network"}'
//! osctl [--socket PATH] perm revoke ID...            remove grants by id
//! osctl [--socket PATH] perm revoke --app APP        remove every grant of an app
//! osctl [--socket PATH] perm audit [--limit N]       the audit log, newest first
//! osctl [--socket PATH] style get [TARGET]           a sheet's edits (TARGET: system, app:ID, or builtin)
//! osctl [--socket PATH] style set TARGET FILE         check FILE (- for standard input), then make it TARGET's edits
//! osctl [--socket PATH] style check FILE              check a sheet; says where any mistake is
//! osctl [--socket PATH] style reset TARGET            remove TARGET's edits
//! osctl [--socket PATH] backup status                 drives, and any backup or restore in progress
//! osctl [--socket PATH] backup setup DRIVE            make a drive (sda1) a backup drive
//! osctl [--socket PATH] backup now DRIVE              back up to it now
//! osctl [--socket PATH] backup list DRIVE             the backups on it, newest first
//! osctl [--socket PATH] backup restore DRIVE NAME     restore one (saving the current state as a snapshot)
//! ```
//!
//! Without `--socket`, osctl uses the bus connection session gave it, if it was
//! started as a `bus=` service, and otherwise the default socket.
//!
//! Exit status: 0 on success, 1 when the bus or a service answers with an error or
//! can't be reached, 2 for bad usage.

mod apps;
mod bench;
mod commands;
mod style;

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use commands::Command;

const USAGE: &str = "usage: osctl [--socket PATH] bus (list | hello | call TO METHOD [JSON] [--timeout MS] \
| stream TO METHOD [JSON] [--count N] | pub TOPIC [JSON] \
| sub PATTERN | tap | echo NAME | bench [--seconds N] [--size BYTES])
       osctl [--socket PATH] db (create TYPE [JSON] | get ID | update ID JSON | delete ID | query JSON \
| search WORDS... [--type TYPE] [--limit N] | history ID | at ID CHANGE_SET | changes [--limit N] | undo CHANGE_SET \
| watch JSON [--count N] | diff ID FROM TO | link ID FIELD TARGET | links ID | schemas | snapshot | snapshots \
| restore NAME | pin ID CHANGE_SET | unpin ID CHANGE_SET | compact | put-blob FILE | get-blob HASH [OUT] | blob-info HASH)
       osctl [--socket PATH] app (install DIR [--watch] | list | launch ID | stop ID)
       osctl [--socket PATH] perm (list [APP] | grant APP RULE... | revoke ID... | revoke --app APP | audit [--limit N])
       osctl [--socket PATH] backup (status | setup DRIVE | now DRIVE | list DRIVE | restore DRIVE NAME)
       osctl [--socket PATH] style (get [TARGET] | set TARGET FILE | check FILE | reset TARGET)";

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
        Some(g) if ["bus", "db", "app", "perm", "backup", "style"].contains(&g.as_str()) => g,
        Some(other) => return Err(format!("unknown command {other}")),
        None => return Err("missing command".into()),
    };
    let sub = args.next().ok_or(format!("missing {group} subcommand"))?;
    // `--watch` is the one flag that takes no value.
    let rest: Vec<String> = args.collect();
    let watch = rest.iter().any(|a| a == "--watch");
    let mut args = rest.into_iter().filter(|a| a != "--watch");
    if watch && !(group == "app" && sub == "install") {
        return Err("only app install takes --watch".into());
    }
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
    let count = flags.iter().any(|(f, _)| f == "count").then(|| number("count", 0)).transpose()?;
    if group == "app" {
        allowed(&[])?;
        let command = match sub.as_str() {
            "install" => {
                arity(1, 1)?;
                Command::AppInstall { dir: positional[0].clone().into(), watch }
            }
            "list" => {
                arity(0, 0)?;
                Command::AppList
            }
            "launch" | "stop" => {
                arity(1, 1)?;
                let method = if sub == "launch" {
                    protocol::session::method::LAUNCH
                } else {
                    protocol::session::method::STOP
                };
                Command::AppControl { method, id: positional[0].clone() }
            }
            other => return Err(format!("unknown app subcommand {other}")),
        };
        return Ok((socket, command));
    }
    if group == "perm" {
        return Ok((socket, perm_command(&sub, &positional, &flags, arity, allowed, json)?));
    }
    if group == "style" {
        allowed(&[])?;
        let target = |t: &String| {
            if style::valid_target(t) || (sub == "get" && t == "builtin") {
                Ok(t.clone())
            } else {
                Err(format!("a style target is system or app:ID, not {t}"))
            }
        };
        let command = match sub.as_str() {
            "get" => {
                arity(0, 1)?;
                Command::StyleGet { target: positional.first().map_or(Ok("system".into()), target)? }
            }
            "set" => {
                arity(2, 2)?;
                Command::StyleSet { target: Some(target(&positional[0])?), file: positional[1].clone() }
            }
            "check" => {
                arity(1, 1)?;
                Command::StyleSet { target: None, file: positional[0].clone() }
            }
            "reset" => {
                arity(1, 1)?;
                Command::StyleSet { target: Some(target(&positional[0])?), file: String::new() }
            }
            other => return Err(format!("unknown style subcommand {other}")),
        };
        return Ok((socket, command));
    }
    if group == "backup" {
        allowed(&[])?;
        return Ok((socket, backup_command(&sub, &positional, arity)?));
    }
    if group == "db" && sub == "watch" {
        arity(1, 1)?;
        allowed(&["count"])?;
        let params = json(positional.first())?;
        if !params.is_object() {
            return Err("db watch takes a query: a JSON object".into());
        }
        let (to, method) = (protocol::db::SERVICE.into(), protocol::db::method::WATCH.into());
        return Ok((socket, Command::Stream { to, method, params, count }));
    }
    if group == "db" && (sub == "put-blob" || sub == "get-blob") {
        allowed(&[])?;
        let command = if sub == "put-blob" {
            arity(1, 1)?;
            Command::PutBlob { file: positional[0].clone().into() }
        } else {
            arity(1, 2)?;
            Command::GetBlob { hash: positional[0].clone(), out: positional.get(1).map(PathBuf::from) }
        };
        return Ok((socket, command));
    }
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
        "stream" => {
            arity(2, 3)?;
            allowed(&["count"])?;
            Command::Stream {
                to: positional[0].clone(),
                method: positional[1].clone(),
                params: json(positional.get(2))?,
                count,
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

/// `osctl backup ...` is shorthand for a request to backupd. Backing up and
/// restoring wait up to an hour: the first backup copies every blob.
fn backup_command(
    sub: &str,
    positional: &[String],
    arity: impl Fn(usize, usize) -> Result<(), String>,
) -> Result<Command, String> {
    use protocol::backup::method;
    use serde_json::json;
    let long = Some(Duration::from_secs(60 * 60));
    let (method, params, timeout) = match sub {
        "status" => {
            arity(0, 0)?;
            (method::STATUS, json!({}), None)
        }
        "setup" | "now" | "list" => {
            arity(1, 1)?;
            let m = match sub {
                "setup" => method::SETUP,
                "now" => method::BACK_UP,
                _ => method::BACKUPS,
            };
            (m, json!({"drive": positional[0]}), if sub == "now" { long } else { None })
        }
        "restore" => {
            arity(2, 2)?;
            (method::RESTORE, json!({"drive": positional[0], "name": positional[1]}), long)
        }
        other => return Err(format!("unknown backup subcommand {other}")),
    };
    Ok(Command::Call { to: protocol::backup::SERVICE.into(), method: method.into(), params, timeout })
}

fn change_set(text: &str) -> Result<i64, String> {
    text.parse().map_err(|_| format!("a change set is a number, not {text}"))
}

/// `osctl db ...` is shorthand for a request to the db service.
fn perm_command(
    sub: &str,
    positional: &[String],
    flags: &[(String, String)],
    arity: impl Fn(usize, usize) -> Result<(), String>,
    allowed: impl Fn(&[&str]) -> Result<(), String>,
    json: impl Fn(Option<&String>) -> Result<serde_json::Value, String>,
) -> Result<Command, String> {
    use protocol::perm::method;
    use serde_json::json;
    let flag = |name: &str| flags.iter().find(|(f, _)| f == name).map(|(_, v)| v.clone());
    let (method, params) = match sub {
        "list" => {
            arity(0, 1)?;
            allowed(&[])?;
            (method::GRANTS, positional.first().map_or(json!({}), |app| json!({"app": app})))
        }
        "grant" => {
            arity(2, usize::MAX)?;
            allowed(&[])?;
            let rules = positional[1..].iter().map(|r| json(Some(r))).collect::<Result<Vec<_>, _>>()?;
            if !rules.iter().all(serde_json::Value::is_object) {
                return Err("each rule is a JSON object".into());
            }
            (method::GRANT, json!({"app": positional[0], "rules": rules}))
        }
        "revoke" => {
            allowed(&["app"])?;
            let ids = positional
                .iter()
                .map(|id| id.parse::<i64>().map_err(|_| format!("a grant id is a number, not {id}")))
                .collect::<Result<Vec<_>, _>>()?;
            match (flag("app"), ids.is_empty()) {
                (None, true) => return Err("perm revoke takes grant ids or --app APP".into()),
                (Some(app), _) => (method::REVOKE, json!({"ids": ids, "app": app})),
                (None, false) => (method::REVOKE, json!({"ids": ids})),
            }
        }
        "audit" => {
            arity(0, 0)?;
            allowed(&["limit"])?;
            let limit = match flag("limit") {
                Some(v) => {
                    json!({"limit": v.parse::<u32>().map_err(|_| format!("--limit must be a whole number, not {v}"))?})
                }
                None => json!({}),
            };
            (method::AUDIT, limit)
        }
        other => return Err(format!("unknown perm subcommand {other}")),
    };
    Ok(Command::Call { to: protocol::perm::SERVICE.into(), method: method.into(), params, timeout: None })
}

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
        "diff" => {
            arity(3, 3)?;
            allowed(&[])?;
            let (from, to) = (change_set(&positional[1])?, change_set(&positional[2])?);
            (method::DIFF, json!({"id": positional[0], "from": from, "to": to}))
        }
        "link" => {
            arity(3, 3)?;
            allowed(&[])?;
            // A dotted field becomes nested objects, as a merge patch needs.
            let mut patch = json!({"$link": positional[2]});
            for key in positional[1].rsplit('.') {
                patch = json!({ key: patch });
            }
            (method::UPDATE, json!({"id": positional[0], "patch": patch}))
        }
        "links" | "blob-info" => {
            arity(1, 1)?;
            allowed(&[])?;
            match sub {
                "links" => (method::LINKS, json!({"id": positional[0]})),
                _ => (method::BLOB_INFO, json!({"hash": positional[0]})),
            }
        }
        "schemas" | "snapshot" | "snapshots" | "compact" => {
            arity(0, 0)?;
            allowed(&[])?;
            let m = match sub {
                "schemas" => method::SCHEMAS,
                "snapshot" => method::SNAPSHOT,
                "compact" => method::COMPACT,
                _ => method::SNAPSHOTS,
            };
            (m, json!({}))
        }
        "pin" | "unpin" => {
            arity(2, 2)?;
            allowed(&[])?;
            let set = change_set(&positional[1])?;
            (method::PIN, json!({"id": positional[0], "change_set": set, "pinned": sub == "pin"}))
        }
        "restore" => {
            arity(1, 1)?;
            allowed(&[])?;
            (method::RESTORE, json!({"name": positional[0]}))
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
        assert_eq!(
            parse_str("db link a from.contact b").unwrap().1,
            db("update", json!({"id": "a", "patch": {"from": {"contact": {"$link": "b"}}}}))
        );
        assert_eq!(parse_str("db diff a 0 7").unwrap().1, db("diff", json!({"id": "a", "from": 0, "to": 7})));
        assert_eq!(parse_str("db links a").unwrap().1, db("links", json!({"id": "a"})));
        assert_eq!(parse_str("db snapshots").unwrap().1, db("snapshots", json!({})));
        assert_eq!(parse_str("db compact").unwrap().1, db("compact", json!({})));
        assert_eq!(
            parse_str("db unpin abc 12").unwrap().1,
            db("pin", json!({"id": "abc", "change_set": 12, "pinned": false}))
        );
        assert_eq!(
            parse_str("db restore snapshot-1-2.db").unwrap().1,
            db("restore", json!({"name": "snapshot-1-2.db"}))
        );
        assert_eq!(parse_str("db put-blob /tmp/x").unwrap().1, Command::PutBlob { file: "/tmp/x".into() });
        assert_eq!(
            parse_str("app install apps/notes --watch").unwrap().1,
            Command::AppInstall { dir: "apps/notes".into(), watch: true }
        );
        assert_eq!(parse_str("app list").unwrap().1, Command::AppList);
        assert_eq!(
            parse_str("app stop org.idealos.notes").unwrap().1,
            Command::AppControl { method: "stop", id: "org.idealos.notes".into() }
        );
        assert_eq!(
            parse_str("db get-blob abc out.bin").unwrap().1,
            Command::GetBlob { hash: "abc".into(), out: Some("out.bin".into()) }
        );
        let watch = |count| Command::Stream {
            to: "db".into(),
            method: "watch".into(),
            params: json!({"type": "Note"}),
            count,
        };
        assert_eq!(parse_str("db watch {\"type\":\"Note\"}").unwrap().1, watch(None));
        assert_eq!(parse_str("db watch {\"type\":\"Note\"} --count 2").unwrap().1, watch(Some(2)));
        assert_eq!(parse_str("bus stream db watch {\"type\":\"Note\"} --count 2").unwrap().1, watch(Some(2)));
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
            "db watch",
            "app install",
            "app list x",
            "app reboot",
            "db get a --watch",
            "db link a b",
            "db diff a b c",
            "db put-blob",
            "db get-blob",
            "db snapshot now",
            "db watch [1]",
            "db watch {} --count x",
            "db watch {} --limit 1",
            "bus stream db",
            "db get a --type x",
        ] {
            assert!(parse_str(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn parses_style_commands() {
        assert_eq!(parse_str("style get").unwrap().1, Command::StyleGet { target: "system".into() });
        assert_eq!(parse_str("style get builtin").unwrap().1, Command::StyleGet { target: "builtin".into() });
        assert_eq!(
            parse_str("style set app:org.idealos.notes -").unwrap().1,
            Command::StyleSet { target: Some("app:org.idealos.notes".into()), file: "-".into() }
        );
        assert_eq!(
            parse_str("style reset system").unwrap().1,
            Command::StyleSet { target: Some("system".into()), file: String::new() }
        );
        for bad in ["style set builtin x.css", "style set notes x.css", "style check", "style paint system"] {
            assert!(parse_str(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn parses_backup_commands() {
        let backup = |method: &str, params: serde_json::Value, timeout: Option<Duration>| Command::Call {
            to: "backup".into(),
            method: method.into(),
            params,
            timeout,
        };
        let hour = Some(Duration::from_secs(3600));
        assert_eq!(parse_str("backup status").unwrap().1, backup("status", json!({}), None));
        assert_eq!(
            parse_str("backup now sda1").unwrap().1,
            backup("back_up", json!({"drive": "sda1"}), hour)
        );
        assert_eq!(
            parse_str("backup restore sda1 0123456789abcdef/backup-1-2.db").unwrap().1,
            backup("restore", json!({"drive": "sda1", "name": "0123456789abcdef/backup-1-2.db"}), hour)
        );
        for bad in ["backup now", "backup list a b", "backup eject sda1", "backup status --limit 2"] {
            assert!(parse_str(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn parses_perm_commands() {
        let perm = |method: &str, params: serde_json::Value| Command::Call {
            to: "perm".into(),
            method: method.into(),
            params,
            timeout: None,
        };
        assert_eq!(parse_str("perm list").unwrap().1, perm("grants", json!({})));
        assert_eq!(parse_str("perm list org.x.a").unwrap().1, perm("grants", json!({"app": "org.x.a"})));
        assert_eq!(
            parse_str(
                "perm grant org.x.a {\"capability\":\"network\"} {\"type\":\"Note\",\"access\":[\"read\"]}"
            )
            .unwrap()
            .1,
            perm(
                "grant",
                json!({"app": "org.x.a", "rules": [{"capability": "network"}, {"type": "Note", "access": ["read"]}]})
            )
        );
        assert_eq!(parse_str("perm revoke 3 4").unwrap().1, perm("revoke", json!({"ids": [3, 4]})));
        assert_eq!(
            parse_str("perm revoke --app org.x.a").unwrap().1,
            perm("revoke", json!({"ids": [], "app": "org.x.a"}))
        );
        assert_eq!(parse_str("perm audit --limit 5").unwrap().1, perm("audit", json!({"limit": 5})));
        for bad in [
            "perm",
            "perm grant org.x.a",
            "perm grant org.x.a [1]",
            "perm revoke",
            "perm revoke x",
            "perm audit --limit x",
            "perm forget",
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
