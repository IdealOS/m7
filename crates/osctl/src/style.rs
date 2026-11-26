//! `osctl style ...`: read and edit style sheets live.
//!
//! Edits to a sheet are a `StyleSheet` document (`target` is `system` or
//! `app:<id>`), which the compositor follows: setting one restyles the screen at
//! once. `set` checks the sheet with the compositor first and refuses one with a
//! mistake, saying where it is, so a broken sheet never reaches the database.
//! `FILE` may be `-` for standard input (as `tools/style-watch.ts` sends it).

use std::io::{Read, Write};

use bus_client::Client;
use protocol::scene::{CheckStyleResult, STYLE_SHEET_TYPE, SystemStyleResult, method};
use serde_json::{Value, json};

/// The sheet's text from `file`, or standard input for `-`.
pub fn read(file: &str) -> Result<String, String> {
    if file == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).map_err(|e| format!("standard input: {e}"))?;
        Ok(s)
    } else {
        std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))
    }
}

/// Whether `target` names a sheet: `system`, or `app:` and an app id.
pub fn valid_target(target: &str) -> bool {
    target == "system"
        || target.strip_prefix("app:").is_some_and(|id| {
            id.split('.').count() >= 2
                && id
                    .split('.')
                    .all(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        })
}

async fn call(bus: &Client, to: &str, method: &str, params: Value) -> Result<Value, String> {
    bus.request(to, method, params).await.map_err(|e| e.to_string())
}

/// The newest `StyleSheet` document for `target`, if any (by when it last changed).
async fn find(bus: &Client, target: &str) -> Result<Option<Value>, String> {
    let q = json!({"type": STYLE_SHEET_TYPE, "where": {"target": target}, "limit": 100});
    let found = call(bus, "db", "query", q).await?;
    let docs = found["docs"].as_array().cloned().unwrap_or_default();
    Ok(docs.into_iter().max_by_key(|d| d["updated"].as_i64().unwrap_or(0)))
}

/// Check `css` with the compositor; an error says where the mistake is.
pub async fn check(bus: &Client, css: &str) -> Result<(), String> {
    let r: CheckStyleResult =
        serde_json::from_value(call(bus, "compositor", method::CHECK_STYLE, json!({"css": css})).await?)
            .map_err(|e| format!("the compositor answered oddly: {e}"))?;
    match r.error {
        Some(e) => Err(format!("line {}, column {}: {}", e.line, e.col, e.message)),
        None => Ok(()),
    }
}

/// Print `target`'s edits (or, with `builtin`, the built-in system sheet).
pub async fn get(bus: &Client, target: &str, out: &mut impl Write) -> Result<(), String> {
    let css = if target == "builtin" {
        let r: SystemStyleResult =
            serde_json::from_value(call(bus, "compositor", method::SYSTEM_STYLE, json!({})).await?)
                .map_err(|e| e.to_string())?;
        r.css
    } else {
        find(bus, target)
            .await?
            .and_then(|d| d["data"]["css"].as_str().map(str::to_owned))
            .unwrap_or_default()
    };
    out.write_all(css.as_bytes()).map_err(|e| e.to_string())?;
    if !css.ends_with('\n') && !css.is_empty() {
        writeln!(out).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Set `target`'s edits to `css`, after checking it.
pub async fn set(bus: &Client, target: &str, css: &str, out: &mut impl Write) -> Result<(), String> {
    check(bus, css).await?;
    match find(bus, target).await? {
        Some(doc) => {
            let id = doc["id"].as_str().unwrap_or_default();
            call(bus, "db", "update", json!({"id": id, "patch": {"css": css}})).await?;
        }
        None => {
            call(
                bus,
                "db",
                "create",
                json!({"type": STYLE_SHEET_TYPE, "data": {"target": target, "css": css}}),
            )
            .await?;
        }
    }
    writeln!(out, "{target}: {} bytes of edits applied", css.len()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_are_the_system_or_an_app() {
        assert!(valid_target("system"));
        assert!(valid_target("app:org.idealos.notes"));
        for bad in ["", "app:", "app:notes", "app:org..x", "System", "app:org.x/y"] {
            assert!(!valid_target(bad), "{bad}");
        }
    }
}
