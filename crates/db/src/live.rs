//! Live queries: queries whose results are kept up to date as documents change.
//!
//! [`Watches`] holds each live query with the result it last reported. After
//! writes, [`Watches::refresh`] asks the store which document types changed since
//! it last looked, re-runs only the queries of those types, and reports a
//! [`WatchEvent::Diff`] for each whose result changed. Calling `refresh` once after
//! a batch of writes, rather than after each, folds them into one diff per query.
//!
//! Re-running is the simple approach the plan starts with: a query is as cheap as
//! its indexes make it, and only queries on a changed type run. Skipping re-runs
//! that a change can't affect (by testing the changed documents against the
//! filter) is a later optimization.

use std::collections::{BTreeSet, HashMap, HashSet};

use protocol::db::{Doc, Query, WatchEvent};

use crate::{Result, Store};

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
}

impl Watches {
    /// No live queries yet; changes already in `store` are taken as seen.
    pub fn new(store: &Store) -> Result<Watches> {
        Ok(Watches { watches: HashMap::new(), seen: store.newest_change_set()? })
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
        let types = store.types_changed_between(self.seen, newest)?;
        self.seen = newest;
        let mut keys: Vec<u64> =
            self.watches.iter().filter(|(_, w)| types.contains(&w.query.doc_type)).map(|(&k, _)| k).collect();
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
        Ok(out)
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
