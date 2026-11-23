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
            "Email",
            &obj(json!({"subject": format!("m{i}"), "date": i, "read": read, "labels": labels})),
            "t",
        )
        .unwrap();
    }
    s.create("Note", &obj(json!({"read": false, "labels": ["INBOX"]})), "t").unwrap();
    let mut q = query("Email", json!({"read": false, "labels": {"$contains": "INBOX"}}));
    q.order_by = vec![OrderBy("date".into(), Direction::Desc)];
    let subjects: Vec<Value> = s.query(&q).unwrap().iter().map(|d| d.data["subject"].clone()).collect();
    assert_eq!(subjects, [json!("m3"), json!("m0")]);
    q.limit = Some(1);
    assert_eq!(s.query(&q).unwrap().len(), 1);
    q.offset = Some(1);
    assert_eq!(s.query(&q).unwrap()[0].data["subject"], "m0");
    assert_eq!(s.query(&Query::all("Email")).unwrap().len(), 4);
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
    let mut q = query("Email", json!({"date": {"$gt": 5}}));
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
        .create(
            "Email",
            &obj(json!({"subject": "Quarterly report", "body": {"text": "numbers inside"}})),
            "t",
        )
        .unwrap();
    s.create("Email", &obj(json!({"subject": "Lunch"})), "t").unwrap();
    s.create("Note", &obj(json!({"text": "quarterly plans"})), "t").unwrap();
    assert_eq!(ids(&s.search("quarterly report", None, 10).unwrap()), std::slice::from_ref(&report.id));
    assert_eq!(ids(&s.search("NUMBERS", Some("Email"), 10).unwrap()), std::slice::from_ref(&report.id));
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
