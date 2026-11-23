//! Live queries: queries whose results are kept up to date as documents change.
//!
//! [`Watches`] holds each live query with the result it last reported. After
//! writes, [`Watches::refresh`] asks the store which documents changed since it
//! last looked, re-runs the queries those changes could affect, and reports a
//! [`WatchEvent::Diff`] for each whose result changed. Calling `refresh` once after
//! a batch of writes, rather than after each, folds them into one diff per query.
//!
//! A change can only affect a query's result if the changed document was in the
//! last result or matches the filter now: otherwise it was outside the result
//! before and still is. "Matches now" is the query's own SQL restricted to the
//! changed rows ([`crate::query::compile_probe`]), a few primary-key lookups
//! instead of a scan, with exactly the query's semantics. A query with an offset
//! always re-runs, since removing a document before its window shifts it.

use std::collections::{BTreeSet, HashMap, HashSet};

use protocol::db::{Doc, Query, WatchEvent};

use crate::{Result, Store};

/// The documents of one type that changed: their row numbers and ids.
pub type Changed = (Vec<i64>, HashSet<String>);

/// One live query and the result it last reported.
struct Watch {
    query: Query,
    last: Vec<Doc>,
}

/// Live queries, keyed by whatever the caller uses to tell them apart (dbd uses
/// the bus id of the `watch` request).
pub struct Watches {
    watches: HashMap<u64, Watch>,
    /// The newest change set `refresh` has looked at.
    seen: i64,
    /// Live queries refresh has considered, and how many of those it re-ran.
    checked: u64,
    reran: u64,
}

impl Watches {
    /// No live queries yet; changes already in `store` are taken as seen.
    pub fn new(store: &Store) -> Result<Watches> {
        Ok(Watches { watches: HashMap::new(), seen: store.newest_change_set()?, checked: 0, reran: 0 })
    }

    pub fn len(&self) -> usize {
        self.watches.len()
    }

    pub fn is_empty(&self) -> bool {
        self.watches.is_empty()
    }

    /// Start live query `key`, returning its first part: the whole result. A bad
    /// query is refused (and not added).
    pub fn add(&mut self, store: &Store, key: u64, query: Query) -> Result<WatchEvent> {
        let docs = store.query(&query)?;
        self.watches.insert(key, Watch { query, last: docs.clone() });
        Ok(WatchEvent::Initial { docs })
    }

    /// How many times a refresh considered a live query on a changed type, and how
    /// many of those it re-ran (the rest were skipped as unaffected).
    pub fn stats(&self) -> (u64, u64) {
        (self.checked, self.reran)
    }

    /// Stop live query `key`. Returns whether it was running.
    pub fn remove(&mut self, key: u64) -> bool {
        self.watches.remove(&key).is_some()
    }

    /// Re-run the live queries that changes since the last refresh could affect,
    /// and return a diff for each whose result changed. A query that fails is
    /// stopped and returned with its error.
    pub fn refresh(&mut self, store: &Store) -> Result<Vec<(u64, Result<WatchEvent>)>> {
        let newest = store.newest_change_set()?;
        if newest == self.seen || self.watches.is_empty() {
            self.seen = newest;
            return Ok(Vec::new());
        }
        let changed = store.docs_changed_between(self.seen, newest)?;
        self.seen = newest;
        let mut keys = Vec::new();
        for (&key, w) in &self.watches {
            let Some((rows, ids)) = changed.get(&w.query.doc_type) else { continue };
            self.checked += 1;
            let affected = w.query.offset.unwrap_or(0) > 0
                || w.last.iter().any(|d| ids.contains(&d.id))
                // If the probe fails, re-run: that reports the error.
                || store.any_match(&w.query, rows).unwrap_or(true);
            if affected {
                keys.push(key);
            }
        }
        self.reran += keys.len() as u64;
        Ok(self.rerun(store, keys))
    }

    /// Re-run every live query, as after a restore replaced the database under
    /// them, and take everything now in the store as seen.
    pub fn reset(&mut self, store: &Store) -> Result<Vec<(u64, Result<WatchEvent>)>> {
        self.seen = store.newest_change_set()?;
        let keys = self.watches.keys().copied().collect();
        Ok(self.rerun(store, keys))
    }

    fn rerun(&mut self, store: &Store, mut keys: Vec<u64>) -> Vec<(u64, Result<WatchEvent>)> {
        keys.sort_unstable(); // deterministic order for tests
        let mut out = Vec::new();
        for key in keys {
            let watch = self.watches.get_mut(&key).expect("listed above");
            match store.query(&watch.query) {
                Ok(docs) => {
                    if let Some(event) = diff(&watch.last, &docs) {
                        watch.last = docs;
                        out.push((key, Ok(event)));
                    }
                }
                Err(e) => {
                    self.watches.remove(&key);
                    out.push((key, Err(e)));
                }
            }
        }
        out
    }
}

/// What changed from result `old` to result `new`, or `None` if nothing did.
pub fn diff(old: &[Doc], new: &[Doc]) -> Option<WatchEvent> {
    let before: HashMap<&str, &Doc> = old.iter().map(|d| (d.id.as_str(), d)).collect();
    let now: HashSet<&str> = new.iter().map(|d| d.id.as_str()).collect();
    let removed: Vec<String> =
        old.iter().filter(|d| !now.contains(d.id.as_str())).map(|d| d.id.clone()).collect();
    let mut added = Vec::new();
    let mut changed = Vec::new();
    for doc in new {
        match before.get(doc.id.as_str()) {
            None => added.push(doc.clone()),
            Some(was) if *was != doc => changed.push(doc.clone()),
            Some(_) => {}
        }
    }
    let kept = old.iter().map(|d| d.id.as_str()).filter(|id| now.contains(id));
    let order =
        (!kept.eq(new.iter().map(|d| d.id.as_str()))).then(|| new.iter().map(|d| d.id.clone()).collect());
    if added.is_empty() && changed.is_empty() && removed.is_empty() && order.is_none() {
        return None;
    }
    Some(WatchEvent::Diff { added, changed, removed, order })
}

impl Store {
    /// The id of the newest change set, or 0 if there are none.
    pub fn newest_change_set(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT IFNULL(MAX(id), 0) FROM change_sets", [], |r| r.get(0))?)
    }

    /// The documents touched by change sets after `after`, up to and including
    /// `upto`, by type: their row numbers and ids.
    pub fn docs_changed_between(&self, after: i64, upto: i64) -> Result<HashMap<String, Changed>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT DISTINCT d.type, d.doc, d.id FROM doc_versions v JOIN documents d ON d.doc = v.doc
             WHERE v.change_set > ?1 AND v.change_set <= ?2",
        )?;
        let mut out: HashMap<String, Changed> = HashMap::new();
        let rows = stmt.query_map([after, upto], |r| Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?)))?;
        for row in rows {
            let (doc_type, doc, id) = row?;
            let entry = out.entry(doc_type).or_default();
            entry.0.push(doc);
            entry.1.insert(id);
        }
        Ok(out)
    }

    /// Whether any of documents `rows` matches `q`'s type and filter now.
    pub fn any_match(&self, q: &Query, rows: &[i64]) -> Result<bool> {
        let probe = crate::query::compile_probe(q, rows)?;
        let mut stmt = self.conn.prepare_cached(&probe.sql)?;
        Ok(stmt.exists(rusqlite::params_from_iter(&probe.params))?)
    }

    /// The types of documents touched by change sets after `after`, up to and
    /// including `upto`.
    pub fn types_changed_between(&self, after: i64, upto: i64) -> Result<BTreeSet<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT DISTINCT d.type FROM doc_versions v JOIN documents d ON d.doc = v.doc
             WHERE v.change_set > ?1 AND v.change_set <= ?2",
        )?;
        let types = stmt.query_map([after, upto], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(types)
    }
}
