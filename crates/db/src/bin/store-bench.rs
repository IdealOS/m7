//! `store-bench`: the real document store at email scale, for the M3 benchmark.
//!
//! `store-bench [--docs N] [--dir DIR] [--keep]` fills a fresh database through
//! [`db::Store`] with N synthetic emails (default 10,000, ~2 KB bodies, each
//! linked to a Contact), then times what apps and services will do: writes,
//! queries through the filter language and the Email schema's indexes, search,
//! live query refreshes, history, undo, snapshots and blobs. It prints Markdown,
//! and checks along the way that the copies agree. Run it on the Mac, in QEMU and
//! on the Pi (`make pi-report` does) and compare.
//!
//! Spike C's `db-bench` measured the design before the store existed; this
//! measures the store.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use db::{Blobs, Snapshots, Store, Watches};
use protocol::db::{Direction, OrderBy, Query};
use serde_json::{Map, Value, json};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct Config {
    docs: usize,
    dir: PathBuf,
    keep: bool,
}

fn config() -> Result<Config> {
    let mut c = Config {
        docs: 10_000,
        dir: std::env::temp_dir().join(format!("idealos-store-bench-{}", std::process::id())),
        keep: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--docs" => c.docs = value()?.parse()?,
            "--dir" => c.dir = value()?.into(),
            "--keep" => c.keep = true,
            "-h" | "--help" => {
                println!("store-bench [--docs N] [--dir DIR] [--keep]");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    Ok(c)
}

/// xorshift64*: deterministic test data, so runs on different machines compare.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

#[rustfmt::skip]
const WORDS: &[&str] = &[
    "meeting", "report", "quarterly", "budget", "lunch", "garden", "invoice", "travel", "photos", "family",
    "project", "deadline", "review", "draft", "concert", "ticket", "weekend", "update", "schedule", "notes",
    "contract", "design", "release", "server", "backup", "recipe", "birthday", "school", "library", "music",
    "the", "a", "and", "to", "of", "for", "with", "on", "about", "please", "thanks", "tomorrow", "today",
];
const PEOPLE: &[&str] =
    &["ana", "bo", "chen", "dara", "eli", "fay", "gus", "hana", "ivo", "jo", "kai", "lena"];
const LABELS: &[&str] = &["IMPORTANT", "STARRED", "Family", "Work", "Receipts", "Travel"];
/// In about one body in fifty, for search.
const RARE_WORD: &str = "aardvark";

fn words(rng: &mut Rng, bytes: usize) -> String {
    let mut s = String::with_capacity(bytes + 16);
    while s.len() < bytes {
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(WORDS[rng.below(WORDS.len())]);
    }
    s
}

fn email(rng: &mut Rng, i: usize, contacts: &[String]) -> Map<String, Value> {
    let from = rng.below(PEOPLE.len());
    let mut labels = vec!["INBOX"];
    labels.extend(LABELS.iter().filter(|_| rng.chance(15)));
    let mut text = words(rng, 2_000);
    if rng.chance(2) {
        text.push(' ');
        text.push_str(RARE_WORD);
    }
    let to = PEOPLE[rng.below(PEOPLE.len())];
    let data = json!({
        "messageId": format!("<{i}@example.com>"),
        "subject": words(rng, 40),
        "from": {"name": PEOPLE[from], "address": format!("{}@example.com", PEOPLE[from]), "contact": {"$link": contacts[from]}},
        "to": [{"address": format!("{to}@example.com")}],
        // One every ten minutes from 2025-01-01.
        "date": 1_735_689_600_000i64 + i as i64 * 600_000,
        "text": text,
        "read": rng.chance(70),
        "labels": labels,
    });
    data.as_object().expect("an object").clone()
}

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
        v.get((v.len().saturating_sub(1) as f64 * p).round() as usize).copied().unwrap_or_default()
    }

    fn row(&self, name: &str) -> String {
        let total: Duration = self.0.iter().sum();
        let rate = self.0.len() as f64 / total.as_secs_f64().max(1e-9);
        format!(
            "| {name} | {} | {} | {} | {rate:.0}/s |",
            self.0.len(),
            ms(self.pct(0.5)),
            ms(self.pct(0.99))
        )
    }
}

fn ms(d: Duration) -> String {
    format!("{:.3} ms", d.as_secs_f64() * 1000.0)
}

fn query(filter: Value) -> Query {
    Query {
        filter: filter.as_object().expect("an object").clone(),
        order_by: vec![OrderBy("date".into(), Direction::Desc)],
        limit: Some(50),
        ..Query::all("Email")
    }
}

/// Median time of 20 runs of `f`, and what the last one returned.
fn median<T>(mut f: impl FnMut() -> Result<T>) -> Result<(Duration, T)> {
    let mut s = Samples::default();
    let mut last = None;
    for _ in 0..20 {
        last = Some(s.time(&mut f)?);
    }
    Ok((s.pct(0.5), last.expect("ran")))
}

struct Report {
    lines: Vec<String>,
    problems: Vec<String>,
}

fn run(c: &Config) -> Result<Report> {
    std::fs::create_dir_all(&c.dir)?;
    let mut store = Store::open(&c.dir.join("bench.db"))?;
    let snapshots = Snapshots::new(c.dir.join("snapshots"))?;
    let blobs = Blobs::new(c.dir.join("blobs"))?;
    let mut rng = Rng(0x005e_ed0f_d0c5);
    let mut out = Vec::new();
    let mut problems = Vec::new();
    out.push(format!(
        "{} Email documents (~2 KB bodies, each linked to one of {} Contacts), {} {}",
        c.docs,
        PEOPLE.len(),
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    out.push(String::new());
    out.push("| Store operation | Count | p50 | p99 | Throughput |".into());
    out.push("| --- | --- | --- | --- | --- |".into());

    // Writes: each a change set of its own, as apps make them.
    let contacts: Vec<String> = PEOPLE
        .iter()
        .map(|p| {
            store.create("Contact", json!({"name": p}).as_object().expect("object"), "bench").map(|d| d.id)
        })
        .collect::<db::Result<_>>()?;
    let mut creates = Samples::default();
    let mut ids = Vec::with_capacity(c.docs);
    for i in 0..c.docs {
        let data = email(&mut rng, i, &contacts);
        ids.push(creates.time(|| store.create("Email", &data, "bench"))?.id);
    }
    out.push(creates.row("Create (schema check, Loro, JSON, FTS, links)"));
    // dbd refreshes the planner's statistics hourly; do it now, after the bulk load.
    let start = Instant::now();
    store.optimize()?;
    let optimized = start.elapsed();
    let mut updates = Samples::default();
    let mut updated = Vec::new();
    for _ in 0..1_000.min(c.docs) {
        let id = ids[rng.below(ids.len())].clone();
        let patch = json!({"read": true, "labels": ["INBOX", "Archived"]});
        updates.time(|| store.update(&id, patch.as_object().expect("object"), "bench"))?;
        updated.push(id);
    }
    out.push(updates.row("Update (mark read, relabel)"));

    // History and undo on documents that have changed.
    let mut reads = Samples::default();
    let mut undos = Samples::default();
    for id in updated.iter().take(100) {
        let history = store.history(id)?;
        let first = history[0].change_set;
        reads.time(|| store.at(id, first))?;
        let last = history.last().expect("updated").change_set;
        // Several updates can hit one document; a later one may already have
        // undone this one's effect, which undo refuses. That's fine here.
        let _ = undos.time(|| store.undo(last, "bench"));
    }
    out.push(reads.row("Read an old version (Loro checkout)"));
    out.push(undos.row("Undo an update"));

    // Live queries: 20 on Email, refreshed after each write and after batches.
    let mut watches = Watches::new(&store)?;
    for k in 0..20u64 {
        let who = PEOPLE[k as usize % PEOPLE.len()];
        watches.add(
            &store,
            k,
            query(json!({"from.address": format!("{who}@example.com"), "read": false})),
        )?;
    }
    let mut each = Samples::default();
    for _ in 0..200 {
        let id = &ids[rng.below(ids.len())];
        store.update(id, json!({"read": rng.chance(50)}).as_object().expect("object"), "bench")?;
        each.time(|| watches.refresh(&store))?;
    }
    out.push(each.row("Refresh 20 live queries after 1 write"));
    let mut batched = Samples::default();
    for _ in 0..20 {
        for _ in 0..50 {
            let id = &ids[rng.below(ids.len())];
            store.update(id, json!({"read": rng.chance(50)}).as_object().expect("object"), "bench")?;
        }
        batched.time(|| watches.refresh(&store))?;
    }
    out.push(batched.row("Refresh 20 live queries after 50 writes"));
    let (checked, reran) = watches.stats();
    let live_note = format!(
        "Live queries: {reran} of {checked} candidate re-runs were needed; the rest were skipped because no changed document was in the result or matched the filter."
    );

    // Queries, median of 20 runs.
    out.push(String::new());
    out.push("| Query | Rows | Median |".into());
    out.push("| --- | --- | --- |".into());
    let inbox = query(json!({"labels": {"$contains": "INBOX"}, "read": false}));
    let (t, docs) = median(|| Ok(store.query(&inbox)?))?;
    out.push(format!(
        "| Inbox: unread, label INBOX, newest 50 (schema index on date) | {} | {} |",
        docs.len(),
        ms(t)
    ));
    let sender = Query { limit: Some(10_000), ..query(json!({"from.address": "ana@example.com"})) };
    let (t, docs) = median(|| Ok(store.query(&sender)?))?;
    out.push(format!("| From one sender, all | {} | {} |", docs.len(), ms(t)));
    let linked = Query { limit: Some(10_000), ..query(json!({"from": {"$link": contacts[0]}})) };
    let (t, by_link) = median(|| Ok(store.query(&linked)?))?;
    out.push(format!("| Linked from one Contact ($link), all | {} | {} |", by_link.len(), ms(t)));
    if by_link.len() != docs.len() {
        problems.push(format!("{} emails link to ana but {} are from her", by_link.len(), docs.len()));
    }
    let (t, n) = median(|| Ok(store.links(&contacts[0])?.1.len()))?;
    out.push(format!("| Backlinks of one Contact | {n} | {} |", ms(t)));
    let (t, hits) = median(|| Ok(store.search(RARE_WORD, Some("Email"), 50)?))?;
    out.push(format!("| Search: one rare word | {} | {} |", hits.len(), ms(t)));
    let (t, hits2) = median(|| Ok(store.search("quarterly budget", Some("Email"), 50)?))?;
    out.push(format!("| Search: two common words, top 50 | {} | {} |", hits2.len(), ms(t)));

    // Snapshots and blobs.
    out.push(String::new());
    let start = Instant::now();
    let snap = snapshots.take(&store)?;
    let took = start.elapsed();
    let start = Instant::now();
    snapshots.restore(&mut store, &snap.name)?;
    let restored = start.elapsed();
    out.push(format!(
        "Snapshot: {} to take, {:.1} MB; restore (which first snapshots the current state): {}.",
        ms(took),
        snap.bytes as f64 / 1e6,
        ms(restored)
    ));
    let blob: Vec<u8> =
        (0..32 * 1024 * 1024u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
    let start = Instant::now();
    let (hash, size) = blobs.write(&mut &blob[..], db::blobs::MAX_BLOB_BYTES)?;
    let wrote = start.elapsed();
    store.add_blob(&hash, size)?;
    let start = Instant::now();
    let digest = db::sha256::hex(&blob);
    let hashed = start.elapsed();
    if digest != hash {
        problems.push("the blob's name isn't its hash".into());
    }
    let mb = size as f64 / 1e6;
    out.push(format!(
        "Blob: {mb:.0} MB stored at {:.0} MB/s (hash, write, fsync); SHA-256 alone {:.0} MB/s.",
        mb / wrote.as_secs_f64(),
        mb / hashed.as_secs_f64()
    ));

    out.push(live_note);
    out.push(format!("Refreshing query statistics after the load (PRAGMA optimize) took {}.", ms(optimized)));
    // Consistency.
    let mut differ = 0;
    for _ in 0..500.min(ids.len()) {
        if !store.verify(&ids[rng.below(ids.len())])? {
            differ += 1;
        }
    }
    if differ > 0 {
        problems.push(format!("{differ} of 500 sampled JSON copies differ from their Loro document"));
    }
    let rare: i64 = store.connection().query_row(
        "SELECT count(*) FROM doc_json WHERE type = 'Email' AND json_extract(json, '$.text') LIKE ?1",
        [format!("%{RARE_WORD}%")],
        |r| r.get(0),
    )?;
    if hits.len() != (rare as usize).min(50) {
        problems.push(format!("search found {} of {rare} bodies with the rare word", hits.len()));
    }
    store.connection().execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    let file = std::fs::metadata(c.dir.join("bench.db"))?.len();
    out.push(format!(
        "Database file: {:.1} MB ({} bytes per document). Consistency: {differ} of 500 sampled JSON copies differ from Loro; search finds {} of {rare} rare-word bodies (capped at 50).",
        file as f64 / 1e6,
        file / (c.docs + PEOPLE.len()).max(1) as u64,
        hits.len()
    ));
    drop(store);
    if !c.keep {
        std::fs::remove_dir_all(&c.dir)?;
    }
    Ok(Report { lines: out, problems })
}

fn main() {
    match config().and_then(|c| run(&c)) {
        Ok(report) => {
            println!("{}", report.lines.join("\n"));
            if !report.problems.is_empty() {
                for p in &report.problems {
                    eprintln!("store-bench: {p}");
                }
                std::process::exit(2);
            }
        }
        Err(e) => {
            eprintln!("store-bench: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_small_run_finds_no_problems() {
        let dir = std::env::temp_dir().join(format!("idealos-store-bench-test-{}", std::process::id()));
        let report = run(&Config { docs: 300, dir: dir.clone(), keep: false }).unwrap();
        assert!(report.problems.is_empty(), "{:?}", report.problems);
        assert!(report.lines.iter().any(|l| l.starts_with("| Store operation |")));
        assert!(!dir.exists(), "cleaned up");
    }
}
