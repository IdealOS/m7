//! Grants and the audit log, in permd's own SQLite file.
//!
//! The file is permd's alone, outside the database, so apps can't read or change
//! it through `db`, and dbd can enforce grants without depending on itself.

use std::path::Path;

use protocol::perm::{AuditAction, AuditEntry, Grant, GrantsResult, Rule};
use rusqlite::{Connection, OptionalExtension, params};

const SCHEMA: &str = "
CREATE TABLE grants (
    id      INTEGER PRIMARY KEY,
    app     TEXT NOT NULL,
    rule    TEXT NOT NULL,          -- a Rule, as JSON
    granted INTEGER NOT NULL,       -- ms since the epoch
    by      TEXT NOT NULL
) STRICT;
CREATE INDEX grants_app ON grants (app);
CREATE TABLE audit (
    id     INTEGER PRIMARY KEY,
    at     INTEGER NOT NULL,
    app    TEXT NOT NULL,
    action TEXT NOT NULL,           -- grant, revoke or deny
    rule   TEXT NOT NULL,
    by     TEXT NOT NULL
) STRICT;
";

/// Most audit entries `audit` returns at once.
pub const MAX_AUDIT: u32 = 10_000;

pub struct Store {
    conn: Connection,
}

pub type Result<T> = rusqlite::Result<T>;

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Store::init(conn)
    }

    pub fn open_in_memory() -> Result<Store> {
        Store::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Store> {
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version == 0 {
            conn.execute_batch(SCHEMA)?;
            conn.pragma_update(None, "user_version", 1)?;
        }
        Ok(Store { conn })
    }

    /// Every grant, oldest first, with the table's version.
    pub fn all(&self) -> Result<GrantsResult> {
        Ok(GrantsResult { grants: self.select(None)?, version: self.version()? })
    }

    /// App `app`'s grants, oldest first.
    pub fn of(&self, app: &str) -> Result<GrantsResult> {
        Ok(GrantsResult { grants: self.select(Some(app))?, version: self.version()? })
    }

    /// Grows with every change: the newest audit entry.
    pub fn version(&self) -> Result<i64> {
        self.conn.query_row("SELECT IFNULL(max(id), 0) FROM audit", [], |r| r.get(0))
    }

    fn select(&self, app: Option<&str>) -> Result<Vec<Grant>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, app, rule, granted, by FROM grants WHERE ?1 IS NULL OR app = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([app], |r| {
            let rule: String = r.get(2)?;
            Ok(Grant {
                id: r.get(0)?,
                app: r.get(1)?,
                rule: parse_rule(&rule)?,
                granted: r.get(3)?,
                by: r.get(4)?,
            })
        })?;
        rows.collect()
    }

    /// Of `rules`, those app `app` doesn't hold yet (exactly as written).
    pub fn missing(&self, app: &str, rules: &[Rule]) -> Result<Vec<Rule>> {
        let held: Vec<Rule> = self.select(Some(app))?.into_iter().map(|g| g.rule).collect();
        let mut out: Vec<Rule> = Vec::new();
        for r in rules {
            if !held.contains(r) && !out.contains(r) {
                out.push(r.clone());
            }
        }
        Ok(out)
    }

    /// Give app `app` the rules it doesn't hold yet, recorded as granted by `by`.
    /// Returns the new grants.
    pub fn grant(&mut self, app: &str, rules: &[Rule], by: &str) -> Result<Vec<Grant>> {
        let new = self.missing(app, rules)?;
        let tx = self.conn.transaction()?;
        let now = now_ms();
        let mut made = Vec::new();
        for rule in new {
            let text = rule_json(&rule);
            tx.execute(
                "INSERT INTO grants (app, rule, granted, by) VALUES (?1, ?2, ?3, ?4)",
                params![app, text, now, by],
            )?;
            made.push(Grant {
                id: tx.last_insert_rowid(),
                app: app.into(),
                rule,
                granted: now,
                by: by.into(),
            });
            audit(&tx, now, app, AuditAction::Grant, &text, by)?;
        }
        tx.commit()?;
        Ok(made)
    }

    /// Remove the grants with ids `ids` and, if given, every grant of `app`.
    /// Returns how many went.
    pub fn revoke(&mut self, ids: &[i64], app: Option<&str>, by: &str) -> Result<u64> {
        let tx = self.conn.transaction()?;
        let now = now_ms();
        let mut gone = Vec::new();
        {
            let mut find = tx.prepare("SELECT app, rule FROM grants WHERE id = ?1")?;
            for id in ids {
                if let Some(row) = find.query_row([id], |r| Ok((*id, r.get(0)?, r.get(1)?))).optional()? {
                    gone.push(row);
                }
            }
            if let Some(app) = app {
                let mut all = tx.prepare("SELECT id, app, rule FROM grants WHERE app = ?1")?;
                for row in all.query_map([app], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))? {
                    let row: (i64, String, String) = row?;
                    if !gone.iter().any(|g: &(i64, String, String)| g.0 == row.0) {
                        gone.push(row);
                    }
                }
            }
        }
        for (id, app, rule) in &gone {
            tx.execute("DELETE FROM grants WHERE id = ?1", [id])?;
            audit(&tx, now, app, AuditAction::Revoke, rule, by)?;
        }
        tx.commit()?;
        Ok(gone.len() as u64)
    }

    /// Record that the user turned down app `app`'s request for `rules`.
    pub fn deny(&mut self, app: &str, rules: &[Rule]) -> Result<()> {
        let tx = self.conn.transaction()?;
        let now = now_ms();
        for rule in rules {
            audit(&tx, now, app, AuditAction::Deny, &rule_json(rule), "user")?;
        }
        tx.commit()
    }

    /// The newest `limit` audit entries, newest first.
    pub fn audit(&self, limit: u32) -> Result<Vec<AuditEntry>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id, at, app, action, rule, by FROM audit ORDER BY id DESC LIMIT ?1")?;
        let rows = stmt.query_map([limit.min(MAX_AUDIT)], |r| {
            let action: String = r.get(3)?;
            let rule: String = r.get(4)?;
            Ok(AuditEntry {
                id: r.get(0)?,
                at: r.get(1)?,
                app: r.get(2)?,
                action: match action.as_str() {
                    "grant" => AuditAction::Grant,
                    "revoke" => AuditAction::Revoke,
                    _ => AuditAction::Deny,
                },
                rule: parse_rule(&rule)?,
                by: r.get(5)?,
            })
        })?;
        rows.collect()
    }
}

fn audit(
    tx: &rusqlite::Transaction,
    at: i64,
    app: &str,
    action: AuditAction,
    rule: &str,
    by: &str,
) -> Result<()> {
    let action = match action {
        AuditAction::Grant => "grant",
        AuditAction::Revoke => "revoke",
        AuditAction::Deny => "deny",
    };
    tx.execute(
        "INSERT INTO audit (at, app, action, rule, by) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![at, app, action, rule, by],
    )?;
    Ok(())
}

fn rule_json(rule: &Rule) -> String {
    serde_json::to_string(rule).expect("rules serialize")
}

fn parse_rule(text: &str) -> rusqlite::Result<Rule> {
    serde_json::from_str(text)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::perm::Access;

    #[test]
    fn grants_are_kept_audited_and_revoked() {
        let mut s = Store::open_in_memory().unwrap();
        assert_eq!(s.all().unwrap(), GrantsResult::default());
        let read = Rule::data("Note", &[Access::Read]);
        let net = Rule::capability("network");
        let made = s.grant("org.x.a", &[read.clone(), net.clone(), read.clone()], "osctl").unwrap();
        assert_eq!(made.len(), 2, "duplicates are dropped");
        assert!(s.grant("org.x.a", std::slice::from_ref(&read), "osctl").unwrap().is_empty(), "held already");
        s.grant("org.x.b", std::slice::from_ref(&net), "user").unwrap();
        assert_eq!(s.of("org.x.a").unwrap().grants.len(), 2);
        assert_eq!(s.missing("org.x.b", &[read.clone(), net.clone()]).unwrap(), std::slice::from_ref(&read));
        let v = s.version().unwrap();

        assert_eq!(s.revoke(&[made[0].id, 999], None, "settings").unwrap(), 1);
        assert_eq!(s.of("org.x.a").unwrap().grants.iter().map(|g| &g.rule).collect::<Vec<_>>(), [&net]);
        assert_eq!(s.revoke(&[], Some("org.x.b"), "osctl").unwrap(), 1);
        s.deny("org.x.c", std::slice::from_ref(&read)).unwrap();
        assert!(s.version().unwrap() > v, "every change moves the version");

        let log = s.audit(100).unwrap();
        let actions: Vec<_> = log.iter().map(|e| (e.action, e.app.as_str(), e.by.as_str())).collect();
        assert_eq!(
            actions,
            [
                (AuditAction::Deny, "org.x.c", "user"),
                (AuditAction::Revoke, "org.x.b", "osctl"),
                (AuditAction::Revoke, "org.x.a", "settings"),
                (AuditAction::Grant, "org.x.b", "user"),
                (AuditAction::Grant, "org.x.a", "osctl"),
                (AuditAction::Grant, "org.x.a", "osctl"),
            ]
        );
    }
}
