use super::*;
use protocol::db::{Direction, OrderBy};
use serde_json::json;

fn obj(v: Value) -> Map<String, Value> {
    v.as_object().expect("an object").clone()
}

fn query(doc_type: &str, filter: Value) -> Query {
    Query { filter: obj(filter), ..Query::all(doc_type) }
}

fn ids(docs: &[Doc]) -> Vec<String> {
    docs.iter().map(|d| d.id.clone()).collect()
}

#[test]
fn create_get_update_delete() {
    let mut s = Store::open_in_memory().unwrap();
    let note = s.create("Note", &obj(json!({"title": "hi", "tags": ["a"]})), "test").unwrap();
    assert_eq!(note.doc_type, "Note");
    assert_eq!(note.created, note.updated);
    assert_eq!(s.get(&note.id).unwrap(), note);

    let changed = s.update(&note.id, &obj(json!({"title": "hello", "tags": null, "n": 2})), "test").unwrap();
    assert_eq!(Value::Object(changed.data.clone()), json!({"title": "hello", "n": 2}));
    assert!(changed.updated >= note.updated);
    assert_eq!(s.get(&note.id).unwrap(), changed);

    s.delete(&note.id, "test").unwrap();
    assert!(matches!(s.get(&note.id), Err(Error::NotFound(_))));
    assert!(matches!(s.update(&note.id, &Map::new(), "test"), Err(Error::NotFound(_))));
    assert!(matches!(s.delete(&note.id, "test"), Err(Error::NotFound(_))));
    assert!(s.query(&Query::all("Note")).unwrap().is_empty());
    assert!(s.search("hello", None, 10).unwrap().is_empty());
    // The state stays, for history and undo.
    assert_eq!(loro_json::read(&s.loro(&note.id).unwrap())["title"], "hello");
}

#[test]
fn ids_sort_by_creation_and_bad_input_is_refused() {
    let mut s = Store::open_in_memory().unwrap();
    let a = s.create("Note", &Map::new(), "test").unwrap();
    let b = s.create("Note", &Map::new(), "test").unwrap();
    assert!(a.id < b.id, "UUIDv7 ids sort by creation");
    assert!(matches!(s.create("not a type", &Map::new(), "test"), Err(Error::Invalid(_))));
    let huge = obj(json!({"body": "x".repeat(MAX_DOC_BYTES)}));
    assert!(matches!(s.create("Note", &huge, "test"), Err(Error::Invalid(_))));
    assert!(matches!(s.update(&a.id, &huge, "test"), Err(Error::Invalid(_))));
    assert_eq!(s.get(&a.id).unwrap().data, Map::new(), "a refused update leaves the document alone");
}

#[test]
fn a_patch_that_changes_nothing_writes_nothing() {
    let mut s = Store::open_in_memory().unwrap();
    let doc = s.create("Note", &obj(json!({"a": 1})), "test").unwrap();
    let count = |s: &Store, table: &str| -> i64 {
        s.connection().query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)).unwrap()
    };
    let sets = count(&s, "change_sets");
    let again = s.update(&doc.id, &obj(json!({"a": 1, "missing": null})), "test").unwrap();
    assert_eq!(again.updated, doc.updated);
    assert_eq!(count(&s, "change_sets"), sets);
    assert_eq!(count(&s, "changes"), 0);
}

#[test]
fn updates_are_logged_then_compacted_without_losing_anything() {
    let mut s = Store::open_in_memory().unwrap();
    let doc = s.create("Counter", &obj(json!({"n": 0})), "test").unwrap();
    let logged = |s: &Store| -> i64 {
        s.connection().query_row("SELECT count(*) FROM changes", [], |r| r.get(0)).unwrap()
    };
    for n in 1..COMPACT_AFTER {
        s.update(&doc.id, &obj(json!({"n": n})), "test").unwrap();
    }
    assert_eq!(logged(&s), COMPACT_AFTER - 1);
    s.update(&doc.id, &obj(json!({"n": COMPACT_AFTER})), "test").unwrap();
    assert_eq!(logged(&s), 0, "folded into the snapshot");
    s.update(&doc.id, &obj(json!({"n": "last"})), "test").unwrap();
    assert_eq!(logged(&s), 1);

    let loro = s.loro(&doc.id).unwrap();
    assert_eq!(Value::Object(loro_json::read(&loro)), json!({"n": "last"}));
    assert_eq!(s.get(&doc.id).unwrap().data, loro_json::read(&loro), "the JSON copy matches the Loro doc");
    // Loro kept every operation, compacted or not: the create and 65 updates.
    assert_eq!(loro.oplog_vv().get(&s.peer).copied(), Some(COMPACT_AFTER as i32 + 2));
}

#[test]
fn reopening_keeps_documents_and_the_peer_id() {
    let dir = std::env::temp_dir().join(format!("db-reopen-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("test.db");
    let (id, peer) = {
        let mut s = Store::open(&path).unwrap();
        (s.create("Note", &obj(json!({"t": 1})), "test").unwrap().id, s.peer)
    };
    let mut s = Store::open(&path).unwrap();
    assert_eq!(s.peer, peer);
    s.update(&id, &obj(json!({"t": 2})), "test").unwrap();
    assert_eq!(s.get(&id).unwrap().data["t"], 2);
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn origins_are_recorded_per_change_set() {
    let mut s = Store::open_in_memory().unwrap();
    let doc = s.create("Note", &Map::new(), "org.example.notes").unwrap();
    s.update(&doc.id, &obj(json!({"x": 1})), "db-test").unwrap();
    let origins: Vec<String> = s
        .connection()
        .prepare("SELECT origin FROM change_sets ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(origins, ["org.example.notes", "db-test"]);
}

/// The plan's inbox query, end to end.
#[test]
fn queries_filter_order_and_limit() {
    let mut s = Store::open_in_memory().unwrap();
    for (i, (read, labels)) in [
        (false, json!(["INBOX"])),
        (true, json!(["INBOX"])),
        (false, json!(["Spam"])),
        (false, json!(["INBOX", "Work"])),
    ]
    .into_iter()
    .enumerate()
    {
        s.create(
            "Mail",
            &obj(json!({"subject": format!("m{i}"), "date": i, "read": read, "labels": labels})),
            "t",
        )
        .unwrap();
    }
    s.create("Note", &obj(json!({"read": false, "labels": ["INBOX"]})), "t").unwrap();
    let mut q = query("Mail", json!({"read": false, "labels": {"$contains": "INBOX"}}));
    q.order_by = vec![OrderBy("date".into(), Direction::Desc)];
    let subjects: Vec<Value> = s.query(&q).unwrap().iter().map(|d| d.data["subject"].clone()).collect();
    assert_eq!(subjects, [json!("m3"), json!("m0")]);
    q.limit = Some(1);
    assert_eq!(s.query(&q).unwrap().len(), 1);
    q.offset = Some(1);
    assert_eq!(s.query(&q).unwrap()[0].data["subject"], "m0");
    assert_eq!(s.query(&Query::all("Mail")).unwrap().len(), 4);
}

#[test]
fn bad_queries_are_refused_before_reaching_sqlite() {
    let s = Store::open_in_memory().unwrap();
    for filter in [
        json!({"$where": "1"}),
        json!({"a": {"$regex": "x"}}),
        json!({"a": {"b": 1}}),
        json!({"a": [1]}),
        json!({"a": {"$in": 1}}),
        json!({"a": {"$gt": true}}),
        json!({"a": {"$exists": 1}}),
        json!({"a": {"$contains": [1]}}),
        json!({"$or": {}}),
        json!({"$not": []}),
        json!({"a'); DROP TABLE documents; --": 1}),
        json!({"a..b": 1}),
        json!({"": 1}),
        json!({"a-b": 1}),
    ] {
        assert!(matches!(s.query(&query("Note", filter.clone())), Err(Error::Invalid(_))), "{filter}");
    }
    assert!(matches!(
        s.query(&Query { limit: Some(MAX_QUERY_LIMIT_PLUS), ..Query::all("Note") }),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(s.query(&Query::all("Note'--")), Err(Error::Invalid(_))));
    let mut deep = json!({"a": 1});
    for _ in 0..40 {
        deep = json!({"$not": deep});
    }
    assert!(matches!(s.query(&query("Note", deep)), Err(Error::Invalid(_))));
    let many: Map<String, Value> = (0..300).map(|i| (format!("f{i}"), json!(i))).collect();
    assert!(matches!(s.query(&Query { filter: many, ..Query::all("Note") }), Err(Error::Invalid(_))));
    // Bound values carry anything, including SQL.
    assert!(s.query(&query("Note", json!({"a": "'); DROP TABLE documents; --"}))).unwrap().is_empty());
    assert_eq!(s.query(&Query::all("Note")).unwrap().len(), 0);
}

const MAX_QUERY_LIMIT_PLUS: u32 = protocol::db::MAX_QUERY_LIMIT + 1;

/// A query on a field with an expression index uses it, so the compiler's SQL
/// matches index expressions exactly (Spike C's 18.8 ms -> 0.21 ms).
#[test]
fn compiled_paths_match_expression_indexes() {
    let s = Store::open_in_memory().unwrap();
    s.connection()
        .execute("CREATE INDEX email_date ON doc_json (type, json_extract(json, '$.date'))", [])
        .unwrap();
    let mut q = query("Mail", json!({"date": {"$gt": 5}}));
    q.order_by = vec![OrderBy("date".into(), Direction::Desc)];
    let compiled = query::compile(&q).unwrap();
    let plan: Vec<String> = s
        .connection()
        .prepare(&format!("EXPLAIN QUERY PLAN {}", compiled.sql))
        .unwrap()
        .query_map(rusqlite::params_from_iter(&compiled.params), |r| r.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(plan.iter().any(|step| step.contains("email_date")), "{plan:?}");
}

#[test]
fn search_finds_words_anywhere_and_treats_syntax_as_text() {
    let mut s = Store::open_in_memory().unwrap();
    let report = s
        .create("Mail", &obj(json!({"subject": "Quarterly report", "body": {"text": "numbers inside"}})), "t")
        .unwrap();
    s.create("Mail", &obj(json!({"subject": "Lunch"})), "t").unwrap();
    s.create("Note", &obj(json!({"text": "quarterly plans"})), "t").unwrap();
    assert_eq!(ids(&s.search("quarterly report", None, 10).unwrap()), std::slice::from_ref(&report.id));
    assert_eq!(ids(&s.search("NUMBERS", Some("Mail"), 10).unwrap()), std::slice::from_ref(&report.id));
    assert_eq!(s.search("quarterly", None, 10).unwrap().len(), 2);
    assert_eq!(s.search("quarterly", Some("Note"), 10).unwrap().len(), 1);
    for syntax in ["\"", "AND", "report*", "subject:x", "(", "NEAR(a b)", "   "] {
        assert!(s.search(syntax, None, 10).is_ok(), "{syntax}");
    }
    // An update re-indexes.
    s.update(&report.id, &obj(json!({"subject": "Annual summary"})), "t").unwrap();
    assert!(s.search("report", None, 10).unwrap().is_empty());
    assert_eq!(ids(&s.search("annual", None, 10).unwrap()), [report.id]);
}

// ---- The compiled SQL agrees with a plain evaluation of the filter language ----

/// The filter language, evaluated directly on a document's JSON.
fn matches(filter: &Map<String, Value>, doc: &Value) -> bool {
    filter.iter().all(|(key, v)| match key.as_str() {
        "$and" => v.as_array().unwrap().iter().all(|f| matches(f.as_object().unwrap(), doc)),
        "$or" => v.as_array().unwrap().iter().any(|f| matches(f.as_object().unwrap(), doc)),
        "$not" => !matches(v.as_object().unwrap(), doc),
        field => {
            let found = lookup(doc, field);
            match v {
                Value::Object(ops) => ops.iter().all(|(op, arg)| op_holds(found, op, arg)),
                scalar => equals(found, scalar),
            }
        }
    })
}

fn lookup<'a>(doc: &'a Value, field: &str) -> Option<&'a Value> {
    field.split('.').try_fold(doc, |v, seg| match seg.parse::<usize>() {
        Ok(i) => v.as_array()?.get(i),
        Err(_) => v.as_object()?.get(seg),
    })
}

fn equals(found: Option<&Value>, want: &Value) -> bool {
    match (found, want) {
        (None | Some(Value::Null), Value::Null) => true,
        (Some(Value::Number(a)), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Some(Value::String(a)), Value::String(b)) => a == b,
        (Some(Value::Bool(a)), Value::Bool(b)) => a == b,
        _ => false,
    }
}

fn op_holds(found: Option<&Value>, op: &str, arg: &Value) -> bool {
    use std::cmp::Ordering;
    let order = || match (found, arg) {
        (Some(Value::Number(a)), Value::Number(b)) => a.as_f64().unwrap().partial_cmp(&b.as_f64().unwrap()),
        (Some(Value::String(a)), Value::String(b)) => Some(a.as_str().cmp(b.as_str())),
        _ => None,
    };
    match op {
        "$eq" => equals(found, arg),
        "$ne" => !equals(found, arg),
        "$gt" => order() == Some(Ordering::Greater),
        "$gte" => matches!(order(), Some(Ordering::Greater | Ordering::Equal)),
        "$lt" => order() == Some(Ordering::Less),
        "$lte" => matches!(order(), Some(Ordering::Less | Ordering::Equal)),
        "$in" => arg.as_array().unwrap().iter().any(|a| equals(found, a)),
        "$nin" => !arg.as_array().unwrap().iter().any(|a| equals(found, a)),
        "$exists" => found.is_some() == arg.as_bool().unwrap(),
        "$contains" => match found {
            Some(Value::Array(items)) => items.iter().any(|item| match arg {
                Value::Null => item.is_null(),
                _ => equals(Some(item), arg),
            }),
            _ => false,
        },
        _ => unreachable!("{op}"),
    }
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) % n
    }

    fn pick<T: Clone>(&mut self, items: &[T]) -> T {
        items[self.below(items.len() as u64) as usize].clone()
    }
}

fn random_scalar(rng: &mut Rng) -> Value {
    rng.pick(&[
        json!(0),
        json!(1),
        json!(1.0),
        json!(2.5),
        json!(-3),
        json!("a"),
        json!("b"),
        json!("1"),
        json!(""),
        json!(true),
        json!(false),
        json!(null),
    ])
}

fn random_doc(rng: &mut Rng) -> Map<String, Value> {
    let mut m = Map::new();
    for field in ["a", "b", "c"] {
        if rng.below(4) > 0 {
            m.insert(field.into(), random_scalar(rng));
        }
    }
    if rng.below(3) > 0 {
        let items = (0..rng.below(4)).map(|_| random_scalar(rng)).collect();
        m.insert("arr".into(), Value::Array(items));
    }
    if rng.below(3) > 0 {
        m.insert("obj".into(), json!({"x": random_scalar(rng)}));
    }
    m
}

fn random_filter(rng: &mut Rng, depth: u32) -> Map<String, Value> {
    let mut f = Map::new();
    for _ in 0..1 + rng.below(2) {
        let choice = if depth > 2 { 3 + rng.below(7) } else { rng.below(10) };
        match choice {
            0 | 1 => {
                let key = if choice == 0 { "$and" } else { "$or" };
                let subs = (0..rng.below(3)).map(|_| Value::Object(random_filter(rng, depth + 1))).collect();
                f.insert(key.into(), Value::Array(subs));
            }
            2 => {
                f.insert("$not".into(), Value::Object(random_filter(rng, depth + 1)));
            }
            _ => {
                let field = rng.pick(&["a", "b", "c", "arr", "obj.x", "arr.0", "missing", "obj"]);
                let cond = match rng.below(11) {
                    0 => random_scalar(rng),
                    1 => json!({"$eq": random_scalar(rng)}),
                    2 => json!({"$ne": random_scalar(rng)}),
                    3 | 4 => {
                        let op = rng.pick(&["$gt", "$gte", "$lt", "$lte"]);
                        let arg = rng.pick(&[json!(0), json!(1), json!(2.5), json!("a"), json!("1")]);
                        json!({ op: arg })
                    }
                    5 => json!({"$in": (0..rng.below(3)).map(|_| random_scalar(rng)).collect::<Vec<_>>()}),
                    6 => json!({"$nin": (0..rng.below(3)).map(|_| random_scalar(rng)).collect::<Vec<_>>()}),
                    7 => json!({"$exists": rng.below(2) == 0}),
                    8 | 9 => json!({"$contains": random_scalar(rng)}),
                    _ => json!({"$gte": 0, "$lt": 2}),
                };
                f.insert(field.into(), cond);
            }
        }
    }
    f
}

/// Random documents and random filters: the SQL returns exactly the documents the
/// plain evaluator matches. `DB_FUZZ_SEEDS=n` runs more.
#[test]
fn compiled_queries_agree_with_the_filter_language() {
    let seeds: u64 = std::env::var("DB_FUZZ_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(20);
    for seed in 0..seeds {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut s = Store::open_in_memory().unwrap();
        let mut docs = Vec::new();
        for _ in 0..60 {
            let d = s.create("T", &random_doc(&mut rng), "fuzz").unwrap();
            docs.push((d.id, Value::Object(d.data)));
        }
        for _ in 0..200 {
            let filter = random_filter(&mut rng, 0);
            let q = Query { filter: filter.clone(), limit: Some(1000), ..Query::all("T") };
            let got = ids(&s.query(&q).unwrap_or_else(|e| panic!("seed {seed}: {e}: {filter:?}")));
            let want: Vec<String> =
                docs.iter().filter(|(_, json)| matches(&filter, json)).map(|(id, _)| id.clone()).collect();
            assert_eq!(got, want, "seed {seed}: filter {}", Value::Object(filter));
        }
    }
}

// ---- History and undo ----

fn data(s: &Store, id: &str) -> Value {
    Value::Object(s.get(id).unwrap().data)
}

#[test]
fn history_lists_every_change_and_reads_old_versions() {
    let mut s = Store::open_in_memory().unwrap();
    let d = s.create("Note", &obj(json!({"title": "one"})), "app.a").unwrap();
    s.update(&d.id, &obj(json!({"title": "two"})), "app.b").unwrap();
    s.update(&d.id, &obj(json!({"title": "two"})), "app.b").unwrap(); // no change, no version
    s.delete(&d.id, "app.c").unwrap();
    let h = s.history(&d.id).unwrap();
    let kinds: Vec<(Kind, &str)> = h.iter().map(|v| (v.kind, v.origin.as_str())).collect();
    assert_eq!(kinds, [(Kind::Create, "app.a"), (Kind::Update, "app.b"), (Kind::Delete, "app.c")]);
    assert_eq!(Value::Object(s.at(&d.id, h[0].change_set).unwrap()), json!({"title": "one"}));
    assert_eq!(Value::Object(s.at(&d.id, h[1].change_set).unwrap()), json!({"title": "two"}));
    assert_eq!(Value::Object(s.at(&d.id, h[2].change_set).unwrap()), json!({"title": "two"}));
    let other = s.create("Note", &Map::new(), "t").unwrap();
    assert!(matches!(s.at(&other.id, h[0].change_set), Err(Error::Invalid(_))));
    assert!(matches!(s.history("nope"), Err(Error::NotFound(_))));
}

#[test]
fn old_versions_survive_compaction() {
    let mut s = Store::open_in_memory().unwrap();
    let d = s.create("Counter", &obj(json!({"n": 0})), "t").unwrap();
    for n in 1..=(COMPACT_AFTER * 2) {
        s.update(&d.id, &obj(json!({"n": n})), "t").unwrap();
    }
    let h = s.history(&d.id).unwrap();
    assert_eq!(h.len() as i64, COMPACT_AFTER * 2 + 1);
    assert_eq!(s.at(&d.id, h[5].change_set).unwrap()["n"], 5);
}

#[test]
fn undo_reverts_one_change_and_keeps_later_ones() {
    let mut s = Store::open_in_memory().unwrap();
    let d = s.create("Note", &obj(json!({"title": "a", "body": "x", "tags": ["t"]})), "t").unwrap();
    s.update(&d.id, &obj(json!({"title": "b", "tags": null})), "t").unwrap();
    let edit = s.history(&d.id).unwrap().last().unwrap().change_set;
    s.update(&d.id, &obj(json!({"body": "y"})), "t").unwrap(); // later, a different field

    let undo = s.undo(edit, "user").unwrap();
    assert_eq!(data(&s, &d.id), json!({"title": "a", "body": "y", "tags": ["t"]}));
    let last = s.history(&d.id).unwrap().pop().unwrap();
    assert_eq!((last.change_set, last.undoes, last.origin.as_str()), (undo, Some(edit), "user"));
    // The JSON copy, search and queries follow.
    assert_eq!(s.search("a", Some("Note"), 10).unwrap().len(), 1);
    assert_eq!(s.query(&query("Note", json!({"tags": {"$contains": "t"}}))).unwrap().len(), 1);

    // Undoing the undo redoes the edit.
    s.undo(undo, "user").unwrap();
    assert_eq!(data(&s, &d.id), json!({"title": "b", "body": "y"}));
    let loro = s.loro(&d.id).unwrap();
    assert_eq!(s.get(&d.id).unwrap().data, loro_json::read(&loro), "the JSON copy matches the Loro doc");
}

#[test]
fn undo_of_a_field_changed_later_restores_the_old_value() {
    let mut s = Store::open_in_memory().unwrap();
    let d = s.create("Note", &obj(json!({"title": "a"})), "t").unwrap();
    s.update(&d.id, &obj(json!({"title": "b"})), "t").unwrap();
    let edit = s.history(&d.id).unwrap()[1].change_set;
    s.update(&d.id, &obj(json!({"title": "c"})), "t").unwrap();
    s.undo(edit, "t").unwrap();
    assert_eq!(data(&s, &d.id), json!({"title": "a"}));
}

#[test]
fn undoing_creates_and_deletes() {
    let mut s = Store::open_in_memory().unwrap();
    let d = s.create("Note", &obj(json!({"title": "keep me"})), "t").unwrap();
    let created = s.history(&d.id).unwrap()[0].change_set;
    s.delete(&d.id, "t").unwrap();
    let deleted = s.history(&d.id).unwrap()[1].change_set;

    let restore = s.undo(deleted, "t").unwrap();
    assert_eq!(data(&s, &d.id), json!({"title": "keep me"}));
    assert_eq!(s.search("keep", None, 10).unwrap().len(), 1);
    // A second undo of the same delete has nothing left to do.
    assert!(matches!(s.undo(deleted, "t"), Err(Error::Invalid(_))));
    s.undo(restore, "t").unwrap();
    assert!(matches!(s.get(&d.id), Err(Error::NotFound(_))));
    s.undo(deleted, "t").unwrap();
    s.undo(created, "t").unwrap();
    assert!(matches!(s.get(&d.id), Err(Error::NotFound(_))));
    assert!(s.query(&Query::all("Note")).unwrap().is_empty());
    let kinds: Vec<Kind> = s.history(&d.id).unwrap().iter().map(|v| v.kind).collect();
    use Kind::*;
    assert_eq!(kinds, [Create, Delete, Restore, Delete, Restore, Delete]);
}

#[test]
fn undo_refuses_what_it_cannot_do() {
    let mut s = Store::open_in_memory().unwrap();
    let d = s.create("Note", &obj(json!({"n": 1})), "t").unwrap();
    s.update(&d.id, &obj(json!({"n": 2})), "t").unwrap();
    let edit = s.history(&d.id).unwrap()[1].change_set;
    s.delete(&d.id, "t").unwrap();
    assert!(matches!(s.undo(edit, "t"), Err(Error::Invalid(_))), "edits of a deleted document");
    assert!(matches!(s.undo(9999, "t"), Err(Error::Invalid(_))));
    let sets_before = s.change_sets(100).unwrap().len();
    let _ = s.undo(edit, "t");
    assert_eq!(s.change_sets(100).unwrap().len(), sets_before, "a refused undo leaves no change set");
}

#[test]
fn change_sets_list_recent_writes() {
    let mut s = Store::open_in_memory().unwrap();
    let a = s.create("Note", &Map::new(), "one").unwrap();
    s.update(&a.id, &obj(json!({"x": 1})), "two").unwrap();
    let sets = s.change_sets(10).unwrap();
    assert_eq!(sets.len(), 2);
    assert_eq!((sets[0].origin.as_str(), &sets[0].docs), ("two", &vec![a.id.clone()]));
    assert_eq!(s.change_sets(1).unwrap().len(), 1);
}

/// A database made by the first schema (before history) opens, migrates and works;
/// its old documents just have no history from before.
#[test]
fn version_1_databases_migrate() {
    let dir = std::env::temp_dir().join(format!("db-migrate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("v1.db");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute("INSERT INTO meta (key, value) VALUES ('peer', 7)", []).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
    }
    let mut s = Store::open(&path).unwrap();
    assert_eq!(s.peer, 7);
    let v: i64 = s.connection().pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
    assert_eq!(v, SCHEMA_VERSION);
    let d = s.create("Note", &obj(json!({"a": 1})), "t").unwrap();
    s.update(&d.id, &obj(json!({"a": 2})), "t").unwrap();
    s.undo(s.history(&d.id).unwrap()[1].change_set, "t").unwrap();
    assert_eq!(data(&s, &d.id), json!({"a": 1}));
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A key a change touched: its name, value before and value after (None = absent).
type KeyChange = (String, Option<Value>, Option<Value>);

/// Random edits and undos of flat documents, against a model of the rule: undoing
/// a change set puts each key it touched back to its value before (removing it if
/// it was absent) and leaves other keys alone. The JSON copy always matches the
/// Loro document. `DB_FUZZ_SEEDS=n` runs more.
#[test]
fn random_undos_follow_the_rule() {
    let seeds: u64 = std::env::var("DB_FUZZ_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(20);
    let keys = ["a", "b", "c", "d"];
    for seed in 0..seeds {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut s = Store::open_in_memory().unwrap();
        let d = s.create("T", &Map::new(), "t").unwrap();
        let mut model = Map::new();
        // Each update's change set and, per key it touched, (before, after).
        let mut edits: Vec<(i64, Vec<KeyChange>)> = Vec::new();
        for step in 0..60 {
            if edits.is_empty() || rng.below(3) > 0 {
                let mut patch = Map::new();
                for _ in 0..1 + rng.below(3) {
                    let key = rng.pick(&keys).to_string();
                    let v = if rng.below(4) == 0 { Value::Null } else { json!(rng.below(5)) };
                    patch.insert(key, v);
                }
                let mut touched = Vec::new();
                for (k, v) in &patch {
                    let before = model.get(k).cloned();
                    let after = (!v.is_null()).then(|| v.clone());
                    if before != after {
                        touched.push((k.clone(), before, after.clone()));
                        match after {
                            Some(v) => model.insert(k.clone(), v),
                            None => model.remove(k),
                        };
                    }
                }
                s.update(&d.id, &patch, "t").unwrap();
                if !touched.is_empty() {
                    let set = s.history(&d.id).unwrap().last().unwrap().change_set;
                    edits.push((set, touched));
                }
            } else {
                let (set, touched) = edits[rng.below(edits.len() as u64) as usize].clone();
                let mut changes = Vec::new();
                for (k, before, _) in &touched {
                    let now = model.get(k).cloned();
                    if now != *before {
                        changes.push((k.clone(), now, before.clone()));
                        match before {
                            Some(v) => model.insert(k.clone(), v.clone()),
                            None => model.remove(k),
                        };
                    }
                }
                match s.undo(set, "t") {
                    Ok(undo) => {
                        assert!(
                            !changes.is_empty(),
                            "seed {seed} step {step}: undo of {set} changed nothing"
                        );
                        edits.push((undo, changes));
                    }
                    Err(Error::Invalid(_)) => {
                        assert!(changes.is_empty(), "seed {seed} step {step}: refused undo")
                    }
                    Err(e) => panic!("{e}"),
                }
            }
            assert_eq!(s.get(&d.id).unwrap().data, model, "seed {seed} step {step}");
            assert_eq!(loro_json::read(&s.loro(&d.id).unwrap()), model, "seed {seed} step {step}: Loro");
        }
    }
}

/// Apply a live query's part to a client's copy of its result, the way
/// [`WatchEvent`]'s docs say to.
fn apply(list: &mut Vec<Doc>, event: WatchEvent) {
    match event {
        WatchEvent::Initial { docs } => *list = docs,
        WatchEvent::Diff { added, changed, removed, order } => {
            assert!(
                !(added.is_empty() && changed.is_empty() && removed.is_empty() && order.is_none()),
                "an empty diff was sent"
            );
            list.retain(|d| !removed.contains(&d.id));
            for doc in changed {
                let slot = list.iter_mut().find(|d| d.id == doc.id).expect("a changed doc is in the list");
                *slot = doc;
            }
            list.extend(added);
            if let Some(order) = order {
                assert_eq!(order.len(), list.len(), "order lists every document");
                list.sort_by_key(|d| order.iter().position(|id| *id == d.id).expect("in order"));
            }
        }
    }
}

use protocol::db::WatchEvent;

#[test]
fn live_queries_report_what_changed() {
    let mut s = Store::open_in_memory().unwrap();
    let mut w = Watches::new(&s).unwrap();
    let a = s.create("Task", &obj(json!({"n": 1, "done": false})), "t").unwrap();
    let q = Query {
        order_by: vec![OrderBy("n".into(), Direction::Asc)],
        limit: Some(2),
        ..query("Task", json!({"done": false}))
    };
    let mut list = Vec::new();
    apply(&mut list, w.add(&s, 7, q.clone()).unwrap());
    assert_eq!(ids(&list), [a.id.as_str()]);
    assert!(w.refresh(&s).unwrap().is_empty(), "the create before the watch was already in its result");

    // Added: the order says where.
    let b = s.create("Task", &obj(json!({"n": 0, "done": false})), "t").unwrap();
    let parts = w.refresh(&s).unwrap();
    let [(7, Ok(WatchEvent::Diff { added, order: Some(order), .. }))] = &parts[..] else {
        panic!("{parts:?}")
    };
    assert_eq!(ids(added), [b.id.as_str()]);
    assert_eq!(order, &[b.id.clone(), a.id.clone()]);
    for (_, part) in parts {
        apply(&mut list, part.unwrap());
    }

    // Changed in place: no order.
    s.update(&a.id, &obj(json!({"title": "x"})), "t").unwrap();
    let parts = w.refresh(&s).unwrap();
    let [(7, Ok(WatchEvent::Diff { changed, order: None, added, removed }))] = &parts[..] else {
        panic!("{parts:?}")
    };
    assert!(added.is_empty() && removed.is_empty());
    assert_eq!(changed[0].data["title"], "x");
    apply(&mut list, parts.into_iter().next().unwrap().1.unwrap());

    // Several writes between refreshes become one part: a third task pushes `a`
    // past the limit, then `b` stops matching, so `a` is back and `c` is in.
    let c = s.create("Task", &obj(json!({"n": -1, "done": false})), "t").unwrap();
    s.update(&b.id, &obj(json!({"done": true})), "t").unwrap();
    let parts = w.refresh(&s).unwrap();
    assert_eq!(parts.len(), 1);
    let part = parts.into_iter().next().unwrap().1.unwrap();
    let WatchEvent::Diff { added, removed, .. } = part.clone() else { panic!() };
    assert_eq!((ids(&added), removed), (vec![c.id.clone()], vec![b.id.clone()]));
    apply(&mut list, part);
    assert_eq!(list, s.query(&q).unwrap());

    // Deleting, and undoing the delete.
    s.delete(&c.id, "t").unwrap();
    let parts = w.refresh(&s).unwrap();
    let [(7, Ok(WatchEvent::Diff { removed, order: None, .. }))] = &parts[..] else { panic!("{parts:?}") };
    assert_eq!(removed, &[c.id.as_str()]);
    let deleted = s.history(&c.id).unwrap().last().unwrap().change_set;
    s.undo(deleted, "t").unwrap();
    let parts = w.refresh(&s).unwrap();
    let [(7, Ok(WatchEvent::Diff { added, .. }))] = &parts[..] else { panic!("{parts:?}") };
    assert_eq!(ids(added), [c.id.as_str()]);

    // Stopped: nothing more.
    assert!(w.remove(7));
    s.create("Task", &obj(json!({"n": -5, "done": false})), "t").unwrap();
    assert!(w.refresh(&s).unwrap().is_empty());
    assert!(w.is_empty());
}

#[test]
fn live_queries_rerun_only_for_changed_types_and_refuse_bad_queries() {
    let mut s = Store::open_in_memory().unwrap();
    let mut w = Watches::new(&s).unwrap();
    w.add(&s, 1, Query::all("Note")).unwrap();
    s.create("Task", &obj(json!({})), "t").unwrap();
    assert!(w.refresh(&s).unwrap().is_empty());
    assert_eq!(
        s.types_changed_between(0, s.newest_change_set().unwrap()).unwrap().into_iter().collect::<Vec<_>>(),
        ["Task"]
    );
    assert!(matches!(w.add(&s, 2, query("Note", json!({"a": {"$regex": "x"}}))), Err(Error::Invalid(_))));
    assert_eq!(w.len(), 1, "a refused query isn't added");
}

/// Random writes to random live queries (filters, orders, limits), refreshing
/// after random batches: applying every part to a client's copy always gives
/// what running the query again gives. `DB_FUZZ_SEEDS=n` runs more.
#[test]
fn live_query_parts_rebuild_the_result() {
    let seeds: u64 = std::env::var("DB_FUZZ_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(20);
    for seed in 0..seeds {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut s = Store::open_in_memory().unwrap();
        let mut w = Watches::new(&s).unwrap();
        let mut queries = Vec::new();
        let mut lists: Vec<Vec<Doc>> = Vec::new();
        for key in 0..6u64 {
            let order_by = match rng.below(3) {
                0 => Vec::new(),
                _ => vec![OrderBy(
                    rng.pick(&["a", "b", "obj.x"]).into(),
                    rng.pick(&[Direction::Asc, Direction::Desc]),
                )],
            };
            let q = Query {
                filter: if rng.below(3) == 0 { Map::new() } else { random_filter(&mut rng, 1) },
                order_by,
                limit: (rng.below(2) == 0).then(|| 1 + rng.below(6) as u32),
                ..Query::all("T")
            };
            let mut list = Vec::new();
            apply(&mut list, w.add(&s, key, q.clone()).unwrap());
            queries.push(q);
            lists.push(list);
        }
        let mut live: Vec<String> = Vec::new();
        let mut sets: Vec<i64> = Vec::new();
        for step in 0..150 {
            match rng.below(10) {
                0..=3 => {
                    let doc_type = if rng.below(5) == 0 { "Other" } else { "T" };
                    let d = s.create(doc_type, &random_doc(&mut rng), "t").unwrap();
                    live.push(d.id);
                }
                4..=6 if !live.is_empty() => {
                    let id = rng.pick(&live);
                    let mut patch = random_doc(&mut rng);
                    if rng.below(3) == 0 {
                        patch.insert(rng.pick(&["a", "b", "c"]).into(), Value::Null);
                    }
                    s.update(&id, &patch, "t").unwrap();
                }
                7 if !live.is_empty() => {
                    let id = live.remove(rng.below(live.len() as u64) as usize);
                    s.delete(&id, "t").unwrap();
                }
                8 if !sets.is_empty() => {
                    // Undo something; refusals are fine, the result just mustn't drift.
                    let _ = s.undo(rng.pick(&sets), "t");
                    live = s
                        .connection()
                        .prepare("SELECT d.id FROM documents d JOIN doc_json j ON j.doc = d.doc")
                        .unwrap()
                        .query_map([], |r| r.get(0))
                        .unwrap()
                        .map(|r| r.unwrap())
                        .collect();
                }
                _ => {}
            }
            sets.push(s.newest_change_set().unwrap());
            if rng.below(4) == 0 {
                for (key, part) in w.refresh(&s).unwrap() {
                    apply(&mut lists[key as usize], part.unwrap());
                }
                for (key, q) in queries.iter().enumerate() {
                    assert_eq!(lists[key], s.query(q).unwrap(), "seed {seed} step {step} query {key}: {q:?}");
                }
            }
        }
    }
}

// --- Links, blobs, schemas, snapshots, diffs ----------------------------------

/// A fresh directory under the system temp dir, removed when dropped.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(name: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!("db-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn link(id: &str) -> Value {
    json!({ "$link": id })
}

#[test]
fn links_are_followed_both_ways() {
    let mut s = Store::open_in_memory().unwrap();
    let c = s.create("Contact", &obj(json!({"name": "Ada"})), "t").unwrap();
    let e = s
        .create("Email", &obj(json!({"from": {"address": "ada@x", "contact": link(&c.id)}, "date": 1})), "t")
        .unwrap();
    let n = s
        .create("Note", &obj(json!({"refs": [link(&c.id), link(&e.id)], "loose": link("nobody")})), "t")
        .unwrap();

    let (out, back) = s.links(&c.id).unwrap();
    assert!(out.is_empty());
    let backs: Vec<(String, String)> = back.iter().map(|b| (b.id.clone(), b.path.clone())).collect();
    assert_eq!(backs, [(e.id.clone(), "from.contact".into()), (n.id.clone(), "refs.0".into())]);
    let (out, _) = s.links(&n.id).unwrap();
    let outs: Vec<(&str, &str, bool)> =
        out.iter().map(|l| (l.path.as_str(), l.id.as_str(), l.exists)).collect();
    assert_eq!(
        outs,
        [("loose", "nobody", false), ("refs.0", c.id.as_str(), true), ("refs.1", e.id.as_str(), true)]
    );

    // Queries: a field (or anything under it) linking to a document, any link to
    // it, and the documents another links to.
    let q = |t: &str, f: Value| ids(&s.query(&query(t, f)).unwrap());
    assert_eq!(q("Email", json!({"from": {"$link": c.id}})), [e.id.as_str()]);
    assert_eq!(q("Email", json!({"from.contact": {"$link": c.id}})), [e.id.as_str()]);
    assert!(q("Email", json!({"to": {"$link": c.id}})).is_empty());
    assert_eq!(q("Note", json!({"$linksTo": c.id})), [n.id.as_str()]);
    assert_eq!(q("Note", json!({"refs": {"$link": e.id}})), [n.id.as_str()]);
    assert_eq!(q("Email", json!({"$linkedFrom": n.id})), [e.id.as_str()]);
    assert_eq!(q("Contact", json!({"$not": {"$linkedFrom": n.id}})), Vec::<String>::new());
    // A field named like a prefix doesn't match: `ref` isn't `refs`.
    assert!(q("Note", json!({"ref": {"$link": c.id}})).is_empty());

    // Changing a field moves its link; deleting a document takes its links out of
    // both directions, and undoing the delete brings them back.
    s.update(&n.id, &obj(json!({"refs": [link(&e.id)]})), "t").unwrap();
    assert_eq!(s.links(&c.id).unwrap().1.len(), 1);
    s.delete(&e.id, "t").unwrap();
    assert!(s.links(&c.id).unwrap().1.is_empty());
    assert!(!s.links(&n.id).unwrap().0.iter().find(|l| l.id == e.id).unwrap().exists);
    let deleted = s.history(&e.id).unwrap().last().unwrap().change_set;
    s.undo(deleted, "t").unwrap();
    assert_eq!(s.links(&c.id).unwrap().1.len(), 1);

    for bad in [json!({"x": {"$link": 5}}), json!({"x": {"$link": "a", "y": 1}}), json!({"x": {"$link": ""}})]
    {
        assert!(matches!(s.create("Note", &obj(bad.clone()), "t"), Err(Error::Invalid(_))), "{bad}");
    }
    assert!(matches!(s.query(&query("Note", json!({"$linksTo": 3}))), Err(Error::Invalid(_))));
    assert!(matches!(s.links("nope"), Err(Error::NotFound(_))));
}

/// The links and blob_refs tables always match the links and blob references in
/// live documents' data, through random creates, updates, deletes and undos.
#[test]
fn link_and_blob_tables_follow_the_documents() {
    let seeds: u64 = std::env::var("DB_FUZZ_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(20);
    let hashes = [refs_hash(b"one"), refs_hash(b"two")];
    for seed in 0..seeds {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut s = Store::open_in_memory().unwrap();
        for h in &hashes {
            s.add_blob(h, 3).unwrap();
        }
        let mut docs: Vec<String> = Vec::new();
        let mut sets: Vec<i64> = Vec::new();
        for step in 0..80 {
            let value = |rng: &mut Rng, docs: &[String]| match rng.below(5) {
                0 if !docs.is_empty() => link(&rng.pick(docs)),
                1 => link("missing"),
                2 => json!({ "$blob": rng.pick(&hashes) }),
                3 => json!([link("x"), {"$blob": rng.pick(&hashes)}]),
                _ => json!(rng.below(3)),
            };
            match rng.below(6) {
                0 | 1 => {
                    let mut data = Map::new();
                    for k in ["a", "b"] {
                        data.insert(k.into(), value(&mut rng, &docs));
                    }
                    docs.push(s.create("T", &data, "t").unwrap().id);
                }
                2 | 3 if !docs.is_empty() => {
                    let id = rng.pick(&docs);
                    let patch = obj(json!({ rng.pick(&["a", "b", "c"]): value(&mut rng, &docs) }));
                    let _ = s.update(&id, &patch, "t");
                }
                4 if !docs.is_empty() => {
                    let _ = s.delete(&rng.pick(&docs), "t");
                }
                5 if !sets.is_empty() => {
                    let _ = s.undo(rng.pick(&sets), "t");
                }
                _ => {}
            }
            sets.push(s.newest_change_set().unwrap());
            check_ref_tables(&s, &format!("seed {seed} step {step}"));
        }
    }
}

fn refs_hash(data: &[u8]) -> String {
    crate::sha256::hex(data)
}

fn check_ref_tables(s: &Store, at: &str) {
    let conn = s.connection();
    let live: Vec<(i64, String)> = conn
        .prepare("SELECT doc, json FROM doc_json ORDER BY doc")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    let mut want_links = Vec::new();
    let mut want_blobs = Vec::new();
    for (row, json) in &live {
        for r in refs::find(&serde_json::from_str(json).unwrap()).unwrap() {
            match r {
                refs::Ref::Link { path, id } => want_links.push((*row, path, id)),
                refs::Ref::Blob { path, hash } => want_blobs.push((*row, path, hash)),
            }
        }
    }
    let rows = |sql: &str| -> Vec<(i64, String, String)> {
        conn.prepare(sql)
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    want_links.sort();
    want_blobs.sort();
    assert_eq!(rows("SELECT src, path, dst FROM links ORDER BY src, path, dst"), want_links, "{at}: links");
    assert_eq!(
        rows("SELECT doc, path, hash FROM blob_refs ORDER BY doc, path, hash"),
        want_blobs,
        "{at}: blobs"
    );
}

#[test]
fn blobs_are_stored_once_by_hash_and_counted() {
    let dir = TempDir::new("blobs");
    let blobs = Blobs::new(dir.0.join("blobs")).unwrap();
    let mut s = Store::open_in_memory().unwrap();
    let bytes = b"hello, blob".repeat(100_000);
    let (hash, size) = blobs.write(&mut &bytes[..], blobs::MAX_BLOB_BYTES).unwrap();
    assert_eq!((hash.as_str(), size), (crate::sha256::hex(&bytes).as_str(), bytes.len() as u64));
    let path = blobs.path(&hash).unwrap();
    assert!(path.ends_with(format!("{}/{}", &hash[..2], &hash[2..])));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o444, "read-only");
    // The same bytes again: same blob, nothing new on disk.
    assert_eq!(blobs.write(&mut &bytes[..], blobs::MAX_BLOB_BYTES).unwrap().0, hash);
    let mut back = Vec::new();
    std::io::Read::read_to_end(&mut blobs.open(&hash).unwrap(), &mut back).unwrap();
    assert_eq!(back, bytes);

    // Documents may only refer to recorded blobs; references are counted.
    let doc = obj(json!({"file": {"$blob": hash}}));
    assert!(matches!(s.create("Note", &doc, "t"), Err(Error::Invalid(_))));
    assert_eq!(s.add_blob(&hash, size).unwrap().refs, 0);
    let d = s.create("Note", &doc, "t").unwrap();
    s.create("Note", &obj(json!({"files": [{"$blob": hash}, {"$blob": hash}]})), "t").unwrap();
    assert_eq!(s.blob_info(&hash).unwrap().refs, 3);
    s.delete(&d.id, "t").unwrap();
    assert_eq!(s.blob_info(&hash).unwrap().refs, 2);
    assert_eq!(s.add_blob(&hash, size).unwrap().size, size, "recording twice is harmless");

    // Too big: refused, and nothing is left behind.
    assert!(matches!(blobs.write(&mut &bytes[..], 10), Err(Error::Invalid(_))));
    let leftovers = std::fs::read_dir(blobs.dir())
        .unwrap()
        .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().starts_with(".incoming"));
    assert_eq!(leftovers.count(), 0);
    for bad in ["", "AB", "../etc/passwd", &"g".repeat(64)] {
        assert!(matches!(blobs.open(bad), Err(Error::Invalid(_))), "{bad}");
    }
    assert!(matches!(blobs.open(&"0".repeat(64)), Err(Error::NotFound(_))));
    assert!(matches!(s.blob_info(&"0".repeat(64)), Err(Error::NotFound(_))));
}

#[test]
fn system_schemas_load_and_check_writes() {
    let all = schemas::all();
    assert_eq!(all.len(), 9);
    let mut s = Store::open_in_memory().unwrap();
    // Required fields, types and nested shapes are checked; unknown fields pass.
    let err = |r: Result<Doc>| match r {
        Err(Error::Invalid(why)) => why,
        other => panic!("{other:?}"),
    };
    assert!(
        err(s.create("Email", &obj(json!({"from": {"address": "a@x"}})), "t")).contains("date is required")
    );
    assert!(
        err(s.create("Email", &obj(json!({"from": {"name": "A"}, "date": 1})), "t")).contains("from.address")
    );
    assert!(
        err(s.create("Email", &obj(json!({"from": {"address": "a"}, "date": "today"})), "t"))
            .contains("date")
    );
    let ok =
        s.create("Email", &obj(json!({"from": {"address": "a"}, "date": 1, "x-mailer": "me"})), "t").unwrap();
    assert!(err(s.update(&ok.id, &obj(json!({"read": "yes"})), "t")).contains("read must be boolean"));
    assert!(
        err(s.create("TextDocument", &obj(json!({"format": "rtf"})), "t")).contains("format must be one of")
    );
    assert!(
        err(s.create("Playlist", &obj(json!({"name": "p", "tracks": ["t1"]})), "t"))
            .contains("tracks.0 must be a link")
    );
    s.create("Playlist", &obj(json!({"name": "p", "tracks": [link("t1")]})), "t").unwrap();
    assert!(err(s.create("Contact", &obj(json!({"name": ""})), "t")).contains("too short"));
    // Types without a schema take anything.
    s.create("Whatever", &obj(json!({"date": "today"})), "t").unwrap();

    // Each x-indexes field has an expression index the compiled query uses.
    let index_names: Vec<String> = s
        .connection()
        .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND name LIKE 'schema_%'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(index_names.contains(&"schema_Email_date".to_string()), "{index_names:?}");
    let q = Query { order_by: vec![OrderBy("date".into(), Direction::Desc)], ..query("Email", json!({})) };
    let compiled = query::compile(&q).unwrap();
    let plan: Vec<String> = s
        .connection()
        .prepare(&format!("EXPLAIN QUERY PLAN {}", compiled.sql))
        .unwrap()
        .query_map(rusqlite::params_from_iter(&compiled.params), |r| r.get::<_, String>(3))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(plan.iter().any(|p| p.contains("schema_Email_date")), "{plan:?}");

    // A schema using a keyword the checker doesn't implement doesn't load.
    let unsupported =
        r#"{"title": "X", "type": "object", "properties": {"a": {"type": "string", "pattern": "^a"}}}"#;
    assert!(schemas::load("X", unsupported).unwrap_err().contains("pattern"));
    assert!(schemas::load("X", r#"{"title": "Y", "type": "object"}"#).is_err());
}

#[test]
fn snapshots_restore_and_can_be_undone() {
    let dir = TempDir::new("snapshots");
    let mut s = Store::open(&dir.0.join("live.db")).unwrap();
    let snaps = Snapshots::new(dir.0.join("snapshots")).unwrap();
    let d = s.create("Note", &obj(json!({"v": 1})), "t").unwrap();
    let first = snaps.take(&s).unwrap();
    assert_eq!(snaps.take(&s).unwrap(), first, "nothing changed, so no new snapshot");
    s.update(&d.id, &obj(json!({"v": 2})), "t").unwrap();
    let e = s.create("Note", &obj(json!({"v": 3})), "t").unwrap();
    let second = snaps.take(&s).unwrap();
    assert_ne!(second, first);
    assert_eq!(snaps.list().unwrap(), [second.clone(), first.clone()]);

    // Restoring brings the old state back, history included, and saves the state
    // it replaced.
    let saved = snaps.restore(&mut s, &first.name).unwrap();
    assert_eq!(data(&s, &d.id), json!({"v": 1}));
    assert!(matches!(s.get(&e.id), Err(Error::NotFound(_))));
    assert_eq!(s.history(&d.id).unwrap().len(), 1);
    // A write after the restore reuses the change set number the saved state had;
    // the next snapshot still sees it's different.
    s.update(&d.id, &obj(json!({"v": 9})), "t").unwrap();
    s.create("Note", &obj(json!({})), "t").unwrap();
    assert_eq!(s.newest_change_set().unwrap(), saved.change_set);
    let after = snaps.take(&s).unwrap();
    assert_ne!(after.name, saved.name);
    // Undo the restore by restoring what it saved.
    snaps.restore(&mut s, &saved.name).unwrap();
    assert_eq!(data(&s, &d.id), json!({"v": 2}));
    assert_eq!(data(&s, &e.id), json!({"v": 3}));
    s.update(&e.id, &obj(json!({"v": 4})), "t").unwrap();
    drop(s);
    // The live database still opens normally, in WAL mode, with the restored data.
    let s = Store::open(&dir.0.join("live.db")).unwrap();
    assert_eq!(data(&s, &e.id), json!({"v": 4}));
    let mode: String = s.connection().pragma_query_value(None, "journal_mode", |r| r.get(0)).unwrap();
    assert_eq!(mode, "wal");

    for bad in ["../live.db", "live.db", "snapshot-1-2.db.tmp", "snapshot--1.db"] {
        assert!(
            matches!(snaps.restore(&mut Store::open_in_memory().unwrap(), bad), Err(Error::Invalid(_))),
            "{bad}"
        );
    }
    assert!(matches!(
        snaps.restore(&mut Store::open_in_memory().unwrap(), "snapshot-1-2.db"),
        Err(Error::NotFound(_))
    ));
    // Another device's snapshot (a different Loro peer) is refused.
    let mut other = Store::open(&dir.0.join("other.db")).unwrap();
    assert!(
        matches!(snaps.restore(&mut other, &first.name), Err(Error::Invalid(why)) if why.contains("another device"))
    );

    let count = snaps.list().unwrap().len();
    assert_eq!(snaps.prune(2).unwrap(), count - 2);
    assert_eq!(snaps.list().unwrap().len(), 2);
}

#[test]
fn diffs_between_versions_list_changed_fields() {
    let mut s = Store::open_in_memory().unwrap();
    let d = s.create("Note", &obj(json!({"a": 1, "o": {"x": 1}, "arr": [1, 2]})), "t").unwrap();
    s.update(&d.id, &obj(json!({"a": 2, "o": {"y": 2}, "b": 3, "arr": [1]})), "t").unwrap();
    let h = s.history(&d.id).unwrap();
    let changes = s.diff(&d.id, h[0].change_set, h[1].change_set).unwrap();
    let got: Vec<(&str, Option<Value>, Option<Value>)> =
        changes.iter().map(|c| (c.path.as_str(), c.before.clone(), c.after.clone())).collect();
    assert_eq!(
        got,
        [
            ("a", Some(json!(1)), Some(json!(2))),
            ("arr", Some(json!([1, 2])), Some(json!([1]))),
            ("b", None, Some(json!(3))),
            ("o.y", None, Some(json!(2))),
        ]
    );
    assert_eq!(s.diff(&d.id, 0, h[0].change_set).unwrap().len(), 3, "everything is new at creation");
    assert!(s.diff(&d.id, h[1].change_set, h[1].change_set).unwrap().is_empty());
    assert!(matches!(s.diff(&d.id, 999, h[1].change_set), Err(Error::Invalid(_))));
}

/// A version 2 database (before links) migrates, and links already in documents'
/// data are found.
#[test]
fn version_2_databases_migrate_and_backfill_links() {
    let dir = TempDir::new("migrate-v2");
    let path = dir.0.join("v2.db");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.execute("INSERT INTO meta (key, value) VALUES ('peer', 7)", []).unwrap();
        conn.execute("INSERT INTO documents (id, type, created, updated) VALUES ('n1', 'Note', 1, 1)", [])
            .unwrap();
        conn.execute(
            r#"INSERT INTO doc_json (doc, type, json) VALUES (1, 'Note', '{"to": {"$link": "n2"}}')"#,
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO documents (id, type, created, updated) VALUES ('n2', 'Note', 1, 1)", [])
            .unwrap();
        conn.execute(r#"INSERT INTO doc_json (doc, type, json) VALUES (2, 'Note', '{"to": {"$link": "n1"}, "bad": {"$link": 1}}')"#, []).unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();
    }
    let s = Store::open(&path).unwrap();
    let rows: Vec<(String, String)> = s
        .connection()
        .prepare("SELECT path, dst FROM links")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    // A document with a malformed link keeps its data but gets no links.
    assert_eq!(rows, [("to".to_string(), "n2".to_string())]);
    assert_eq!(s.links("n2").unwrap().1[0].id, "n1");
}
