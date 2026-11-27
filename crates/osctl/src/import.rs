//! `osctl import ...`: drive importd. A source is `smb://user@host[:port]/share`
//! or, where importd allows folders, `dir:/path`.

use std::io::{BufRead, Write};
use std::time::Duration;

use bus_client::Client;
use protocol::import::{BrowseResult, ImportStatus, Kind, SERVICE, Source, method};
use serde_json::{Value, json};

/// What to do.
#[derive(Debug, PartialEq)]
pub enum Import {
    /// Keep a password, read from standard input, for `user` on `host`.
    Login {
        host: String,
        user: String,
    },
    Browse {
        source: Source,
        path: String,
    },
    /// Start importing, then follow it to the end unless `detach`.
    Run {
        source: Source,
        paths: Vec<String>,
        detach: bool,
    },
    Status,
}

/// `osctl import SUB ARGS...`.
pub fn parse(sub: &str, positional: &[String], flags: &[(String, String)]) -> Result<Import, String> {
    let source = |s: &String| Source::parse(s);
    let detach = match flags.iter().find(|(f, _)| f == "detach") {
        Some((_, v)) => v == "yes" || v == "true",
        None => false,
    };
    if let Some((f, _)) = flags.iter().find(|(f, _)| !(sub == "run" && f == "detach")) {
        return Err(format!("import {sub} has no --{f}"));
    }
    match (sub, positional) {
        ("login", [host, user]) => Ok(Import::Login { host: host.clone(), user: user.clone() }),
        ("browse", [s]) => Ok(Import::Browse { source: source(s)?, path: String::new() }),
        ("browse", [s, path]) => Ok(Import::Browse { source: source(s)?, path: path.clone() }),
        ("run", [s, paths @ ..]) if !paths.is_empty() => {
            Ok(Import::Run { source: source(s)?, paths: paths.to_vec(), detach })
        }
        ("status", []) => Ok(Import::Status),
        ("login" | "browse" | "run" | "status", _) => Err(format!("wrong arguments for import {sub}")),
        (other, _) => Err(format!("unknown import subcommand {other}")),
    }
}

async fn call(bus: &Client, m: &str, params: Value) -> Result<Value, String> {
    bus.request(SERVICE, m, params).await.map_err(|e| e.to_string())
}

pub async fn run(bus: &Client, what: Import, out: &mut impl Write) -> Result<(), String> {
    let w = |e: std::io::Error| e.to_string();
    match what {
        Import::Login { host, user } => {
            eprint!("Password for {user} on {host}: ");
            let password = read_password().map_err(w)?;
            eprintln!();
            call(bus, method::LOGIN, json!({"host": host, "user": user, "password": password})).await?;
            writeln!(out, "kept for {user} on {host}").map_err(w)
        }
        Import::Browse { source, path } => {
            let v = call(bus, method::BROWSE, json!({"source": source, "path": path})).await?;
            let found: BrowseResult = serde_json::from_value(v).map_err(|e| e.to_string())?;
            for e in &found.entries {
                let size = if e.kind == Kind::Folder { String::new() } else { size(e.size) };
                writeln!(out, "{:<7} {:>9}  {}", kind(e.kind), size, e.name).map_err(w)?;
            }
            Ok(())
        }
        Import::Run { source, paths, detach } => {
            let v = call(bus, method::IMPORT, json!({"source": source, "paths": paths})).await?;
            let started: ImportStatus = serde_json::from_value(v).map_err(|e| e.to_string())?;
            writeln!(out, "importing {} files from {}", started.total, source.name()).map_err(w)?;
            if detach {
                return Ok(());
            }
            let mut last = 0;
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let s: ImportStatus = serde_json::from_value(call(bus, method::STATUS, Value::Null).await?)
                    .map_err(|e| e.to_string())?;
                if s.done != last || !s.running {
                    last = s.done;
                    writeln!(out, "{}", line(&s)).map_err(w)?;
                    out.flush().map_err(w)?;
                }
                if !s.running {
                    for e in &s.errors {
                        writeln!(out, "  failed: {e}").map_err(w)?;
                    }
                    return Ok(());
                }
            }
        }
        Import::Status => {
            let s: ImportStatus = serde_json::from_value(call(bus, method::STATUS, Value::Null).await?)
                .map_err(|e| e.to_string())?;
            writeln!(out, "{}", json!(s)).map_err(w)
        }
    }
}

/// A line from standard input, not shown as it's typed when it's a terminal.
fn read_password() -> std::io::Result<String> {
    let fd = libc::STDIN_FILENO;
    let mut was: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: `was` is a valid termios to fill; a failure just means not a terminal.
    let tty = unsafe { libc::tcgetattr(fd, &mut was) } == 0;
    if tty {
        let mut quiet = was;
        quiet.c_lflag &= !libc::ECHO;
        // SAFETY: a termios read from the same descriptor, with echo off.
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &quiet) };
    }
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line);
    if tty {
        // SAFETY: putting back what tcgetattr gave.
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &was) };
    }
    read?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

/// One progress line.
pub fn line(s: &ImportStatus) -> String {
    let mut l = format!(
        "{}/{}: {} imported, {} already there, {} failed, {} not imported",
        s.done, s.total, s.imported, s.skipped, s.failed, s.ignored
    );
    if let Some(c) = &s.current {
        l.push_str(&format!(" ({c})"));
    }
    l
}

fn kind(k: Kind) -> &'static str {
    match k {
        Kind::Folder => "folder",
        Kind::Text => "text",
        Kind::Image => "image",
        Kind::Audio => "audio",
        Kind::Other => "other",
    }
}

fn size(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.1} KB", b as f64 / 1024.0),
        b if b < 1024 * 1024 * 1024 => format!("{:.1} MB", b as f64 / 1048576.0),
        b => format!("{:.1} GB", b as f64 / 1073741824.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn import_commands_parse() {
        let share =
            Source::Share { host: "10.0.2.2".into(), share: "Music".into(), user: "me".into(), port: None };
        assert_eq!(
            parse("browse", &args(&["smb://me@10.0.2.2/Music", "Albums"]), &[]),
            Ok(Import::Browse { source: share.clone(), path: "Albums".into() })
        );
        assert_eq!(
            parse("run", &args(&["smb://me@10.0.2.2/Music", "a", "b"]), &[("detach".into(), "yes".into())]),
            Ok(Import::Run { source: share, paths: args(&["a", "b"]), detach: true })
        );
        assert_eq!(
            parse("login", &args(&["10.0.2.2", "me"]), &[]),
            Ok(Import::Login { host: "10.0.2.2".into(), user: "me".into() })
        );
        assert!(parse("run", &args(&["smb://me@10.0.2.2/Music"]), &[]).is_err(), "nothing to import");
        assert!(parse("browse", &args(&["smb://nobody/x"]), &[]).is_err());
        assert!(parse("status", &[], &[("detach".into(), "yes".into())]).is_err());
        assert_eq!(size(1536), "1.5 KB");
    }
}
