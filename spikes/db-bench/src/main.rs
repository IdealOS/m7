//! Spike C: can Loro documents plus a SQLite JSON copy and FTS5 index keep up on a Pi 4?
//!
//! `cargo run --release -p db-bench -- --docs 10000` prints a Markdown report. Run the
//! same binary on the Mac, in QEMU and on the Pi and compare.

mod corpus;
mod store;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use corpus::Rng;
use store::{Result, Store};

struct Config {
    docs: usize,
    updates: usize,
    body_bytes: usize,
    path: PathBuf,
    keep: bool,
}

impl Config {
    fn from_args() -> Result<Config> {
        let mut c = Config {
            docs: 10_000,
            updates: 1_000,
            body_bytes: 2_000,
            path: std::env::temp_dir().join(format!("idealos-db-bench-{}.sqlite", std::process::id())),
            keep: false,
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
            match arg.as_str() {
                "--docs" => c.docs = value()?.parse()?,
                "--updates" => c.updates = value()?.parse()?,
                "--body-bytes" => c.body_bytes = value()?.parse()?,
                "--db" => c.path = value()?.into(),
                "--keep" => c.keep = true,
                "-h" | "--help" => {
                    println!("db-bench [--docs N] [--updates N] [--body-bytes N] [--db PATH] [--keep]");
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument {other}").into()),
            }
        }
        Ok(c)
    }
}

/// Latencies of repeated operations.
#[derive(Default)]
struct Samples(Vec<Duration>);

impl Samples {
    fn time<T>(&mut self, f: impl FnOnce() -> T) -> T {
        let start = Instant::now();
        let out = f();
        self.0.push(start.elapsed());
        out
    }

    fn pct(&self, p: f64) -> Duration {
        let mut v = self.0.clone();
        v.sort();
        v.get(((v.len().saturating_sub(1)) as f64 * p).round() as usize).copied().unwrap_or_default()
    }

    fn total(&self) -> Duration {
        self.0.iter().sum()
    }

    fn row(&self, name: &str) -> String {
        let per_sec = self.0.len() as f64 / self.total().as_secs_f64().max(1e-9);
        format!(
            "| {name} | {} | {} | {} | {:.0}/s |",
            self.0.len(),
            ms(self.pct(0.5)),
            ms(self.pct(0.99)),
            per_sec
        )
    }
}

fn ms(d: Duration) -> String {
    format!("{:.3} ms", d.as_secs_f64() * 1000.0)
}

fn time_query(conn: &rusqlite::Connection, sql: &str, arg: Option<&str>) -> Result<(Duration, usize)> {
    let mut s = Samples::default();
    let mut rows = 0;
    for _ in 0..20 {
        rows = s.time(|| store::count_rows(conn, sql, arg))?;
    }
    Ok((s.pct(0.5), rows))
}

/// Everything the report shows; tests check the correctness fields.
struct Report {
    lines: Vec<String>,
    mismatches: usize,
    search_rows: usize,
    rare_bodies: usize,
}

fn run(c: &Config) -> Result<Report> {
    let mut out = Vec::new();
    let mut store = Store::create(&c.path)?;
    let mut rng = Rng::new(0x01de_a105);

    out.push(format!(
        "SQLite {}, {} docs with ~{} byte bodies, {} updates, {} {}",
        Store::sqlite_version(),
        c.docs,
        c.body_bytes,
        c.updates,
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    out.push(String::new());
    out.push("| Operation | Count | p50 | p99 | Throughput |".into());
    out.push("| --- | --- | --- | --- | --- |".into());

    // 1. Inserts, one transaction each: the durable write path an app sees.
    let mut inserts = Samples::default();
    let mut rows = Vec::with_capacity(c.docs);
    for i in 0..c.docs {
        let email = corpus::email(&mut rng, i, c.body_bytes);
        let row = inserts.time(|| -> Result<i64> {
            let tx = store.conn.transaction()?;
            let row = store::insert(&tx, &email)?;
            tx.commit()?;
            Ok(row)
        })?;
        rows.push(row);
    }
    out.push(inserts.row("Insert (Loro doc + snapshot + JSON + FTS, 1 txn each)"));

    // 2. Updates: mark read and add a label, logging the incremental Loro update.
    let mut updates = Samples::default();
    let mut update_bytes = 0;
    for _ in 0..c.updates {
        let row = rows[rng.below(rows.len())];
        update_bytes += updates.time(|| -> Result<usize> {
            let tx = store.conn.transaction()?;
            let n = store::mark_read(&tx, row, "Archived")?;
            tx.commit()?;
            Ok(n)
        })?;
    }
    out.push(updates.row("Update (load, edit, log update, rewrite JSON + FTS)"));

    // 3. Cold loads: rebuild the Loro doc from snapshot plus logged updates.
    let mut loads = Samples::default();
    {
        let tx = store.conn.transaction()?;
        for _ in 0..c.updates.max(100) {
            let row = rows[rng.below(rows.len())];
            loads.time(|| store::load(&tx, row))?;
        }
    }
    out.push(loads.row("Load doc from snapshot + updates"));

    // 4. Compaction: fold the update log back into snapshots.
    let mut compacts = Samples::default();
    let dirty: Vec<i64> = {
        let mut stmt = store.conn.prepare("SELECT DISTINCT doc FROM changes")?;
        stmt.query_map([], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?
    };
    {
        let tx = store.conn.transaction()?;
        for row in &dirty {
            compacts.time(|| store::compact(&tx, *row))?;
        }
        tx.commit()?;
    }
    out.push(compacts.row("Compact one doc"));
    out.push(String::new());
    out.push(format!("Average Loro update: {} bytes", update_bytes.checked_div(c.updates).unwrap_or(0)));

    // 5. Queries, median of 20 runs each.
    out.push(String::new());
    out.push("| Query | Rows | Median |".into());
    out.push("| --- | --- | --- |".into());
    let (t, n) = time_query(&store.conn, store::INBOX_QUERY, None)?;
    out.push(format!("| Inbox: unread, label INBOX, newest 50 (no index) | {n} | {} |", ms(t)));
    let started = Instant::now();
    store.conn.execute_batch(store::DATE_INDEX)?;
    let index_time = started.elapsed();
    let (t, n) = time_query(&store.conn, store::INBOX_QUERY, None)?;
    out.push(format!("| Inbox, with (type, date) expression index | {n} | {} |", ms(t)));
    let (t, n) = time_query(&store.conn, store::SENDER_QUERY, None)?;
    out.push(format!("| From ana, label Family (full scan) | {n} | {} |", ms(t)));
    let (t, search_rows) = time_query(&store.conn, store::SEARCH_QUERY, Some(corpus::RARE_WORD))?;
    out.push(format!("| FTS5: one rare word | {search_rows} | {} |", ms(t)));
    let (t, n) = time_query(&store.conn, store::SEARCH_QUERY, Some("quarterly AND budget"))?;
    out.push(format!("| FTS5: two common words, top 50 by rank | {n} | {} |", ms(t)));
    out.push(String::new());
    out.push(format!("Building the expression index took {}.", ms(index_time)));

    // 6. Consistency: every JSON copy must equal its Loro document.
    let mut mismatches = 0;
    {
        let tx = store.conn.transaction()?;
        let mut stmt = tx.prepare("SELECT doc, json FROM doc_json")?;
        let pairs: Vec<(i64, String)> =
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        for (row, json) in pairs {
            if store::json_of(&store::load(&tx, row)?)? != json {
                mismatches += 1;
            }
        }
    }
    let rare_bodies: i64 = store.conn.query_row(
        "SELECT count(*) FROM doc_json WHERE json_extract(json, '$.body') LIKE ?1",
        [format!("%{}%", corpus::RARE_WORD)],
        |r| r.get(0),
    )?;
    out.push(format!(
        "Consistency: {mismatches} JSON copies differ from their Loro doc; FTS finds {search_rows} of {rare_bodies} bodies with the rare word (capped at 50)."
    ));

    // 7. Size on disk.
    store.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    let size = |sql: &str| -> Result<i64> { Ok(store.conn.query_row(sql, [], |r| r.get(0))?) };
    let file = std::fs::metadata(&c.path)?.len();
    let snap = size("SELECT CAST(coalesce(avg(length(snapshot)), 0) AS INTEGER) FROM doc_state")?;
    let json = size("SELECT CAST(coalesce(avg(length(json)), 0) AS INTEGER) FROM doc_json")?;
    out.push(format!(
        "Database file: {:.1} MB ({} bytes per doc). Average snapshot {snap} bytes, JSON copy {json} bytes.",
        file as f64 / 1e6,
        file / c.docs.max(1) as u64
    ));

    drop(store);
    if !c.keep {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", c.path.display()));
        }
    }
    Ok(Report { lines: out, mismatches, search_rows, rare_bodies: rare_bodies as usize })
}

fn main() {
    let result = Config::from_args().and_then(|c| run(&c));
    match result {
        Ok(report) => {
            println!("{}", report.lines.join("\n"));
            if report.mismatches > 0 || report.search_rows != report.rare_bodies.min(50) {
                eprintln!("db-bench: consistency check failed");
                std::process::exit(2);
            }
        }
        Err(e) => {
            eprintln!("db-bench: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_run_is_consistent() {
        let c = Config {
            docs: 300,
            updates: 200,
            body_bytes: 300,
            path: std::env::temp_dir().join(format!("idealos-db-bench-test-{}.sqlite", std::process::id())),
            keep: false,
        };
        let report = run(&c).unwrap();
        assert_eq!(report.mismatches, 0);
        assert!(report.rare_bodies > 0, "corpus should contain the rare word");
        assert_eq!(report.search_rows, report.rare_bodies.min(50));
    }

    #[test]
    fn update_survives_reload() {
        let path =
            std::env::temp_dir().join(format!("idealos-db-bench-reload-{}.sqlite", std::process::id()));
        let mut store = Store::create(&path).unwrap();
        let mut rng = Rng::new(7);
        let mut email = corpus::email(&mut rng, 0, 100);
        email.read = false;
        let tx = store.conn.transaction().unwrap();
        let row = store::insert(&tx, &email).unwrap();
        store::mark_read(&tx, row, "Archived").unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&store::json_of(&store::load(&tx, row).unwrap()).unwrap()).unwrap();
        tx.commit().unwrap();
        drop(store);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        assert_eq!(json["read"], true);
        assert_eq!(json["labels"].as_array().unwrap().last().unwrap(), "Archived");
    }

    /// Regression: a bare `json` inside `json_each`'s subquery made every query return 0 rows.
    #[test]
    fn queries_match_in_memory_filter() {
        let path = std::env::temp_dir().join(format!("idealos-db-bench-query-{}.sqlite", std::process::id()));
        let mut store = Store::create(&path).unwrap();
        let mut rng = Rng::new(42);
        let emails: Vec<_> = (0..400).map(|i| corpus::email(&mut rng, i, 50)).collect();
        let tx = store.conn.transaction().unwrap();
        let rows: Vec<i64> = emails.iter().map(|e| store::insert(&tx, e).unwrap()).collect();
        tx.commit().unwrap();

        let sql = |q: &str| -> Vec<i64> {
            let mut stmt = store.conn.prepare(q).unwrap();
            stmt.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
        };
        let mut sender: Vec<i64> = sql(store::SENDER_QUERY);
        sender.sort();
        let expected: Vec<i64> = rows
            .iter()
            .zip(&emails)
            .filter(|(_, e)| e.from == "ana@example.com" && e.labels.contains(&"Family"))
            .map(|(r, _)| *r)
            .collect();
        assert!(!expected.is_empty());
        assert_eq!(sender, expected);

        let mut unread: Vec<(i64, i64)> =
            rows.iter().zip(&emails).filter(|(_, e)| !e.read).map(|(r, e)| (e.date, *r)).collect();
        unread.sort_by(|a, b| b.cmp(a));
        let expected: Vec<i64> = unread.iter().take(50).map(|(_, r)| *r).collect();
        assert_eq!(sql(store::INBOX_QUERY), expected);

        drop(store);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }
}
