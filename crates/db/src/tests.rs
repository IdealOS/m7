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

    let deleted = s.delete(&note.id, "test").unwrap();
    assert!(matches!(s.get(&note.id), Err(Error::NotFound(_))));
    assert!(matches!(s.update(&note.id, &Map::new(), "test"), Err(Error::NotFound(_))));
    assert!(matches!(s.delete(&note.id, "test"), Err(Error::NotFound(_))));
    assert!(s.query(&Query::all("Note")).unwrap().is_empty());
    assert!(s.search("hello", None, 10).unwrap().is_empty());
    // The state stays, for history and undo; undoing the delete brings it back.
    assert_eq!(loro_json::read(&s.loro(&note.id).unwrap())["title"], "hello");
    s.undo(deleted, "test").unwrap();
    assert_eq!(s.get(&note.id).unwrap().data, changed.data);
}

#[test]
fn creations_can_join_a_change_set_and_undo_together() {
    let mut s = Store::open_in_memory().unwrap();
    let a = s.create("Photo", &obj(json!({"n": 1})), "import").unwrap();
    let set = s.history(&a.id).unwrap()[0].change_set;
    let b = s.create_in("Photo", &obj(json!({"n": 2})), "import", Some(set)).unwrap();
    let c = s.create_in("Song", &obj(json!({"n": 3})), "import", Some(set)).unwrap();
    assert_eq!(s.history(&c.id).unwrap()[0].change_set, set);
    // Only its maker may add to it, and not to an undo.
    assert!(s.create_in("Photo", &Map::new(), "someone", Some(set)).is_err());
    assert!(s.create_in("Photo", &Map::new(), "import", Some(set + 1000)).is_err());
    let undo = s.undo(set, "import").unwrap();
    for id in [&a.id, &b.id, &c.id] {
        assert!(matches!(s.get(id), Err(Error::NotFound(_))), "{id} went with the undo");
    }
    assert!(s.create_in("Photo", &Map::new(), "import", Some(undo)).is_err(), "an undo");
    assert!(s.create_in("Photo", &Map::new(), "import", Some(set)).is_err(), "undone");
}

#[test]
fn queries_can_order_by_when_documents_changed_and_were_made() {
    let mut s = Store::open_in_memory().unwrap();
    // Apart in time, so neither creation nor the update ties (ties keep creation order).
    let pause = || std::thread::sleep(std::time::Duration::from_millis(3));
    let a = s.create("Note", &obj(json!({"n": 1})), "t").unwrap();
    pause();
    let b = s.create("Note", &obj(json!({"n": 2})), "t").unwrap();
    pause();
    s.update(&a.id, &obj(json!({"n": 3})), "t").unwrap();
    let ordered = |s: &Store, key: &str, dir| {
        let mut q = Query::all("Note");
        q.order_by = vec![OrderBy(key.into(), dir)];
        ids(&s.query(&q).unwrap())
    };
    assert_eq!(ordered(&s, "$updated", Direction::Desc), [a.id.clone(), b.id.clone()]);
    assert_eq!(ordered(&s, "$updated", Direction::Asc), [b.id.clone(), a.id.clone()]);
    assert_eq!(ordered(&s, "$created", Direction::Desc), [b.id.clone(), a.id.clone()]);
    let mut q = Query::all("Note");
    q.order_by = vec![OrderBy("$deleted".into(), Direction::Asc)];
    assert!(s.query(&q).is_err(), "other $ names aren't sort keys");
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

/// Following one document by id, as a bound text field does: its own changes
/// arrive, a sibling's don't.
#[test]
fn a_live_query_can_follow_one_document() {
    let mut s = Store::open_in_memory().unwrap();
    let mut w = Watches::new(&s).unwrap();
    let a = s.create("Note", &obj(json!({"text": "one"})), "t").unwrap();
    let b = s.create("Note", &obj(json!({"text": "two"})), "t").unwrap();
    let WatchEvent::Initial { docs } = w.add(&s, 1, query("Note", json!({"$id": a.id}))).unwrap() else {
        panic!()
    };
    assert_eq!(ids(&docs), [a.id.as_str()]);
    s.update(&b.id, &obj(json!({"text": "other"})), "t").unwrap();
    assert!(w.refresh(&s).unwrap().is_empty(), "another document");
    s.update(&a.id, &obj(json!({"text": "uno"})), "t").unwrap();
    let parts = w.refresh(&s).unwrap();
    let [(1, Ok(WatchEvent::Diff { changed, .. }))] = &parts[..] else { panic!("{parts:?}") };
    assert_eq!(changed[0].data["text"], "uno");
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
                offset: (rng.below(4) == 0).then(|| rng.below(3) as u32),
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
    // A half-written upload left by a crash is cleaned up when the store opens.
    std::fs::create_dir_all(dir.0.join("blobs")).unwrap();
    std::fs::write(dir.0.join("blobs").join(".incoming-crashed"), b"half").unwrap();
    let blobs = Blobs::new(dir.0.join("blobs")).unwrap();
    assert!(!dir.0.join("blobs").join(".incoming-crashed").exists());
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
    assert_eq!(all.len(), 10);
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
        err(s.create("TextDocument", &obj(json!({"tags": ["ok", 3]})), "t"))
            .contains("tags.1 must be string")
    );
    s.create("TextDocument", &obj(json!({"title": "t", "format": "markdown", "tags": ["draft"]})), "t")
        .unwrap();
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

#[test]
fn splicing_text_edits_a_field_as_collaborative_text() {
    let mut s = Store::open_in_memory().unwrap();
    let d = s.create("Note", &obj(json!({"title": "hello", "meta": {"n": 1}})), "t").unwrap();
    // A plain string becomes text on the first splice; positions are characters.
    s.splice(&d.id, "title", 5, 0, " wörld", "t").unwrap();
    assert_eq!(data(&s, &d.id)["title"], "hello wörld");
    s.splice(&d.id, "title", 6, 5, "there", "t").unwrap();
    assert_eq!(data(&s, &d.id)["title"], "hello there");
    // Missing fields and maps are created.
    s.splice(&d.id, "meta.body", 0, 0, "x", "t").unwrap();
    s.splice(&d.id, "draft.text", 0, 0, "y", "t").unwrap();
    assert_eq!(
        data(&s, &d.id),
        json!({"title": "hello there", "meta": {"n": 1, "body": "x"}, "draft": {"text": "y"}})
    );
    // Searchable, in history, and undoable like any change.
    assert_eq!(ids(&s.search("there", None, 5).unwrap()), [d.id.as_str()]);
    let last = s.history(&d.id).unwrap().last().unwrap().change_set;
    s.undo(last, "t").unwrap();
    assert!(data(&s, &d.id).get("draft").and_then(|v| v.get("text")).is_none());
    // An edit that changes nothing writes nothing; bad ranges and paths are refused.
    assert_eq!(s.splice(&d.id, "title", 0, 0, "", "t").unwrap(), 0);
    assert!(matches!(s.splice(&d.id, "title", 50, 0, "x", "t"), Err(Error::Invalid(_))));
    assert!(
        matches!(s.splice(&d.id, "title", 1, usize::MAX, "", "t"), Err(Error::Invalid(_))),
        "no overflow"
    );
    assert!(matches!(s.splice(&d.id, "meta.n", 0, 0, "x", "t"), Err(Error::Invalid(_))));
    assert!(matches!(s.splice(&d.id, "title..x", 0, 0, "x", "t"), Err(Error::Invalid(_))));
    assert!(matches!(s.splice("nope", "title", 0, 0, "x", "t"), Err(Error::NotFound(_))));
    // A merge patch can still replace the whole field.
    s.update(&d.id, &obj(json!({"title": "fresh"})), "t").unwrap();
    assert_eq!(data(&s, &d.id)["title"], "fresh");
    assert!(s.verify(&d.id).unwrap());
}

#[test]
fn creators_are_recorded_and_backfilled_by_the_migration() {
    let dir = std::env::temp_dir().join(format!("db-creator-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("v3.db");
    let old = {
        let mut s = Store::open(&path).unwrap();
        let d = s.create("Note", &obj(json!({"a": 1})), "org.x.old").unwrap();
        // Make it look like a version 3 database, from before creators (and before
        // version 5's pins and version blobs).
        s.connection()
            .execute_batch(
                "DROP INDEX documents_creator; ALTER TABLE documents DROP COLUMN creator;
                 DROP TABLE version_blobs; ALTER TABLE doc_versions DROP COLUMN pinned;",
            )
            .unwrap();
        s.connection().pragma_update(None, "user_version", 3).unwrap();
        d.id
    };
    let mut s = Store::open(&path).unwrap();
    assert_eq!(s.creator(&old).unwrap().as_deref(), Some("org.x.old"));
    let new = s.create("Note", &obj(json!({"a": 2})), "org.x.new").unwrap();
    s.update(&new.id, &obj(json!({"a": 3})), "someone.else").unwrap();
    assert_eq!(s.creator(&new.id).unwrap().as_deref(), Some("org.x.new"), "updates don't change it");
    let mine = s.query(&query("Note", json!({"$createdBy": "org.x.new"}))).unwrap();
    // One document by its id, as a bound field follows it.
    assert_eq!(ids(&s.query(&query("Note", json!({"$id": new.id}))).unwrap()), std::slice::from_ref(&new.id));
    assert!(s.query(&query("Note", json!({"$id": 7}))).is_err());
    assert_eq!(ids(&mine), std::slice::from_ref(&new.id));
    assert!(s.query(&query("Note", json!({"$createdBy": 3}))).is_err());
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn guarded_writes_roll_back_when_the_result_is_out_of_reach() {
    let mut s = Store::open_in_memory().unwrap();
    let family = obj(json!({"album": "Family"}));
    // A create outside the guard isn't made at all.
    let refused = s.guarded(family.clone(), |s| s.create("Photo", &obj(json!({"album": "Work"})), "app"));
    assert!(matches!(refused, Err(Error::Denied(_))), "{refused:?}");
    assert!(s.query(&Query::all("Photo")).unwrap().is_empty());
    let d =
        s.guarded(family.clone(), |s| s.create("Photo", &obj(json!({"album": "Family"})), "app")).unwrap();
    // Nor is an update that moves it out, or a splice.
    let moved = s.guarded(family.clone(), |s| s.update(&d.id, &obj(json!({"album": "Work"})), "app"));
    assert!(matches!(moved, Err(Error::Denied(_))));
    let spliced = s.guarded(family.clone(), |s| s.splice(&d.id, "album", 0, 6, "Work", "app"));
    assert!(matches!(spliced, Err(Error::Denied(_))));
    assert_eq!(data(&s, &d.id), json!({"album": "Family"}));
    assert_eq!(s.history(&d.id).unwrap().len(), 1, "nothing refused was recorded");
    assert!(s.verify(&d.id).unwrap());
    // Inside it is fine, and without a guard anything goes.
    s.guarded(family.clone(), |s| s.update(&d.id, &obj(json!({"year": 2024})), "app")).unwrap();
    s.update(&d.id, &obj(json!({"album": "Work"})), "system").unwrap();
    assert!(!s.matches(&d.id, &family).unwrap());
    assert!(s.matches(&d.id, &obj(json!({"year": 2024}))).unwrap());
    assert!(matches!(s.matches("nope", &family), Err(Error::NotFound(_))));
}

// --- History compaction ------------------------------------------------------

/// The time compaction runs at in these tests: noon UTC on day 20601 of the epoch
/// (2026-05-28, a Thursday). Days with `d % 7 == 4` are Mondays.
const NOW: i64 = 20601 * retention::DAY_MS + 12 * HOUR;
const HOUR: i64 = 60 * 60 * 1000;

/// Noon (plus `hour`) on epoch day `day`.
fn day(day: i64, hour: i64) -> i64 {
    day * retention::DAY_MS + hour * HOUR
}

/// Date the newest change set `at`, and return it.
fn dated(s: &Store, at: i64) -> i64 {
    let set = s.newest_change_set().unwrap();
    s.conn.execute("UPDATE change_sets SET at = ?2 WHERE id = ?1", rusqlite::params![set, at]).unwrap();
    set
}

/// Patch document `id` with `patch`, dated `at`; returns the change set.
fn edit_at(s: &mut Store, id: &str, patch: Value, at: i64) -> i64 {
    s.update(id, &obj(patch), "t").unwrap();
    dated(s, at)
}

fn kept(s: &Store, id: &str) -> Vec<i64> {
    s.history(id).unwrap().iter().map(|v| v.change_set).collect()
}

#[test]
fn compaction_keeps_recent_history_then_the_last_of_each_day_then_of_each_week() {
    let mut s = Store::open_in_memory().unwrap();
    let id = s.create("Note", &obj(json!({"n": 0})), "t").unwrap().id;
    let created = dated(&s, day(20000, 0));
    // Over a year old, so weekly. Days 20227 to 20233 are one week, 20234 (a Monday)
    // to 20240 the next.
    let a = edit_at(&mut s, &id, json!({"n": 1}), day(20228, 0));
    let b = edit_at(&mut s, &id, json!({"n": 2}), day(20230, 0));
    let c = edit_at(&mut s, &id, json!({"n": 3}), day(20234, 0));
    let d = edit_at(&mut s, &id, json!({"n": 4}), day(20235, 0));
    // A hundred days old, so daily; f is pinned.
    let e = edit_at(&mut s, &id, json!({"n": 5}), day(20500, 1));
    let f = edit_at(&mut s, &id, json!({"n": 6}), day(20500, 2));
    let g = edit_at(&mut s, &id, json!({"n": 7}), day(20500, 3));
    let h = edit_at(&mut s, &id, json!({"n": 8}), day(20501, 0));
    s.pin(&id, f, true).unwrap();
    // Recent: all kept.
    let i = edit_at(&mut s, &id, json!({"n": 9}), day(20590, 1));
    let j = edit_at(&mut s, &id, json!({"n": 10}), day(20590, 2));
    assert!(matches!(s.pin(&id, 999, true), Err(Error::NotFound(_))));

    let (versions, _) = s.thin_history(NOW, &Retention::default()).unwrap();
    assert_eq!(versions, 3);
    assert_eq!(kept(&s, &id), vec![created, b, d, f, g, h, i, j]);
    assert!(!kept(&s, &id).iter().any(|v| [a, c, e].contains(v)));
    assert!(s.history(&id).unwrap().iter().find(|v| v.change_set == f).unwrap().pinned);
    // Kept versions still read back, and diffs span the gaps.
    assert_eq!(s.at(&id, d).unwrap()["n"], 4);
    assert_eq!(s.diff(&id, b, d).unwrap().len(), 1);
    // A second pass finds nothing more.
    assert_eq!(s.thin_history(NOW, &Retention::default()).unwrap().0, 0);
}

#[test]
fn compaction_drops_change_sets_once_nothing_needs_them() {
    let mut s = Store::open_in_memory().unwrap();
    let id = s.create("Note", &obj(json!({"n": 0})), "t").unwrap().id;
    let gone = edit_at(&mut s, &id, json!({"n": 1}), day(20500, 1));
    edit_at(&mut s, &id, json!({"n": 2}), day(20500, 2));
    // An undone change set stays while the undo points at it.
    let undone = edit_at(&mut s, &id, json!({"n": 3}), day(20501, 1));
    s.undo(undone, "t").unwrap();
    dated(&s, day(20501, 2));
    let r = Retention::default();
    // The thinned change set's update is still logged, unfolded, so it stays.
    assert_eq!(s.thin_history(NOW, &r).unwrap(), (2, 0));
    // Fold the log (every COMPACT_AFTER updates), then it can go.
    for n in 0..COMPACT_AFTER {
        s.update(&id, &obj(json!({"m": n})), "t").unwrap();
    }
    let (_, change_sets) = s.thin_history(NOW, &r).unwrap();
    assert_eq!(change_sets, 1);
    assert!(matches!(s.change_set(gone), Err(Error::NotFound(_))));
    assert!(s.change_set(undone).is_ok());
}

#[test]
fn compaction_leaves_history_dated_while_the_clock_was_unset() {
    let mut s = Store::open_in_memory().unwrap();
    let id = s.create("Note", &obj(json!({"n": 0})), "t").unwrap().id;
    // 1970: written before the network set the clock.
    let a = edit_at(&mut s, &id, json!({"n": 1}), 1_000);
    let b = edit_at(&mut s, &id, json!({"n": 2}), 2_000);
    s.thin_history(NOW, &Retention::default()).unwrap();
    assert!(kept(&s, &id).contains(&a) && kept(&s, &id).contains(&b));
}

#[test]
fn compaction_deletes_only_the_blobs_nothing_needs() {
    let dir = TempDir::new("sweep");
    let blobs = Blobs::new(dir.0.join("blobs")).unwrap();
    let mut s = Store::open_in_memory().unwrap();
    let put = |s: &mut Store, bytes: &[u8], created: i64| {
        let (hash, size) = blobs.write(&mut &bytes[..], blobs::MAX_BLOB_BYTES).unwrap();
        s.add_blob(&hash, size).unwrap();
        s.conn
            .execute("UPDATE blobs SET created = ?2 WHERE hash = ?1", rusqlite::params![hash, created])
            .unwrap();
        hash
    };
    let old = day(20100, 0);
    let live = put(&mut s, b"live", old);
    let thinned = put(&mut s, b"thinned", old);
    let in_kept = put(&mut s, b"in a kept version", old);
    let deleted = put(&mut s, b"of a deleted doc", old);
    let unused = put(&mut s, b"never used", old);
    let fresh = put(&mut s, b"just uploaded", NOW - HOUR);
    let unset = put(&mut s, b"stored in 1970", 1_000);
    let blob = |h: &str| json!({"$blob": h});

    s.create("Note", &obj(json!({"f": blob(&live)})), "t").unwrap();
    // Used only by a version thinned out: later that day the field changed.
    let b = s.create("Note", &obj(json!({})), "t").unwrap().id;
    dated(&s, day(20100, 0));
    edit_at(&mut s, &b, json!({"f": blob(&thinned)}), day(20500, 1));
    edit_at(&mut s, &b, json!({"f": null}), day(20500, 2));
    // Used by the last version of a day, which is kept.
    let c = s.create("Note", &obj(json!({})), "t").unwrap().id;
    dated(&s, day(20100, 0));
    edit_at(&mut s, &c, json!({"f": blob(&in_kept)}), day(20500, 1));
    edit_at(&mut s, &c, json!({"f": null}), day(20501, 1));
    // A deleted document keeps its blobs, so undoing the delete works.
    let d = s.create("Note", &obj(json!({"f": blob(&deleted)})), "t").unwrap().id;
    dated(&s, day(20100, 0));
    s.delete(&d, "t").unwrap();
    dated(&s, day(20100, 1));
    // A file the database never recorded (a crash before add_blob), long ago,
    // and one written just now.
    let (stray, stray_size) = blobs.write(&mut &b"stray"[..], blobs::MAX_BLOB_BYTES).unwrap();
    let long_ago =
        std::time::UNIX_EPOCH + std::time::Duration::from_millis((NOW - 2 * retention::DAY_MS) as u64);
    std::fs::File::open(blobs.path(&stray).unwrap()).unwrap().set_modified(long_ago).unwrap();
    let (uploading, _) = blobs.write(&mut &b"being uploaded"[..], blobs::MAX_BLOB_BYTES).unwrap();

    let done = compact_history(&mut s, &blobs, NOW, &Retention::default()).unwrap();
    assert_eq!(done.blobs, 3, "{done:?}");
    assert_eq!(done.blob_bytes, (b"thinned".len() + b"never used".len()) as u64 + stray_size);
    for h in [&thinned, &unused, &stray] {
        assert!(matches!(blobs.open(h), Err(Error::NotFound(_))), "{h} should be gone");
        assert!(matches!(s.blob_info(h), Err(Error::NotFound(_))));
    }
    for h in [&live, &in_kept, &deleted, &fresh, &unset] {
        assert!(blobs.open(h).is_ok() && s.blob_info(h).is_ok(), "{h} should stay");
    }
    assert!(blobs.open(&uploading).is_ok(), "a fresh file the database doesn't know yet stays");
    // Undoing the delete still finds its blob.
    let delete = s.history(&d).unwrap().last().unwrap().change_set;
    s.undo(delete, "t").unwrap();
    assert_eq!(s.get(&d).unwrap().data["f"], blob(&deleted));
    assert_eq!(compact_history(&mut s, &blobs, NOW, &Retention::default()).unwrap().blobs, 0);
}

#[test]
fn upgrading_records_the_blobs_of_existing_versions() {
    let dir = TempDir::new("version-blobs");
    let path = dir.0.join("test.db");
    let blobs = Blobs::new(dir.0.join("blobs")).unwrap();
    let rows = |s: &Store| -> Vec<(i64, i64, String)> {
        s.conn
            .prepare("SELECT doc, change_set, hash FROM version_blobs ORDER BY doc, change_set, hash")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    let before = {
        let mut s = Store::open(&path).unwrap();
        let mut hashes = Vec::new();
        for bytes in [&b"one"[..], b"two"] {
            let (h, n) = blobs.write(&mut &bytes[..], blobs::MAX_BLOB_BYTES).unwrap();
            s.add_blob(&h, n).unwrap();
            hashes.push(h);
        }
        let id = s.create("Note", &obj(json!({"a": {"$blob": hashes[0]}})), "t").unwrap().id;
        s.update(&id, &obj(json!({"b": {"$blob": hashes[1]}})), "t").unwrap();
        s.update(&id, &obj(json!({"a": null})), "t").unwrap();
        s.delete(&id, "t").unwrap();
        s.create("Note", &obj(json!({"none": 1})), "t").unwrap();
        let rows = rows(&s);
        assert_eq!(rows.len(), 1 + 2 + 1 + 1, "create, update, update, delete");
        // Back to schema version 4, as a database from before compaction.
        s.conn
            .execute_batch(
                "DROP TABLE version_blobs; ALTER TABLE doc_versions DROP COLUMN pinned; PRAGMA user_version = 4;",
            )
            .unwrap();
        rows
    };
    let s = Store::open(&path).unwrap();
    assert_eq!(rows(&s), before);
}
