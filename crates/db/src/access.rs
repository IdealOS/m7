//! What an app may see and change, as filters in the query language.
//!
//! An app may read and write the documents it created, plus whatever its grants
//! ([`protocol::perm`]) cover. [`Scope`] turns that into filters: a query from the
//! app runs with its read filter ANDed in (so it never even sees what it can't
//! read), and a write is checked against the write or create filter both before
//! it applies and, inside its transaction, after ([`crate::Store::guarded`]), so a
//! write can't move a document out of the app's reach or into another's.
//!
//! The documents an app created are matched with `$createdBy`, which compares the
//! creator the store recorded, not anything in the document's data. An app may
//! also read its own `App` document (the installer made it), which is how
//! app-host, running as the app, finds the app's code.

use protocol::db::Query;
use protocol::perm::{Access, AppGrants, DataRule};
use serde_json::{Map, Value, json};

/// One app's view of the database.
#[derive(Debug, Clone, PartialEq)]
pub struct Scope {
    pub app: String,
    pub grants: AppGrants,
}

impl Scope {
    pub fn new(app: impl Into<String>, grants: AppGrants) -> Scope {
        Scope { app: app.into(), grants }
    }

    /// Whether the app holds capability `name`.
    pub fn has(&self, name: &str) -> bool {
        self.grants.has(name)
    }

    /// The documents of `doc_type` the app may read: its own, and those a read
    /// or write grant covers.
    pub fn read(&self, doc_type: &str) -> Map<String, Value> {
        let mut f =
            self.own_or(doc_type, |r| r.access.contains(&Access::Read) || r.access.contains(&Access::Write));
        if doc_type == "App" && !f.is_empty() {
            f = one_of(vec![Value::Object(f), json!({"id": self.app})]);
        }
        f
    }

    /// The documents of `doc_type` the app may change or delete.
    pub fn write(&self, doc_type: &str) -> Map<String, Value> {
        self.own_or(doc_type, |r| r.access.contains(&Access::Write))
    }

    /// What a document of `doc_type` the app creates must match, or `None` if
    /// it may not create any.
    pub fn create(&self, doc_type: &str) -> Option<Map<String, Value>> {
        let filters: Vec<Value> = self
            .rules(doc_type, |r| r.access.contains(&Access::Create))
            .map(|r| Value::Object(r.filter.clone()))
            .collect();
        if filters.is_empty() {
            return None;
        }
        Some(one_of(filters))
    }

    /// `q`, limited to what the app may read.
    pub fn query(&self, q: &Query) -> Query {
        Query { filter: both(&q.filter, self.read(&q.doc_type)), ..q.clone() }
    }

    fn rules<'a>(
        &'a self,
        doc_type: &'a str,
        want: impl Fn(&DataRule) -> bool + 'a,
    ) -> impl Iterator<Item = &'a DataRule> + 'a {
        self.grants
            .data
            .iter()
            .filter(move |r| (r.doc_type == doc_type || r.doc_type == protocol::perm::ALL_TYPES) && want(r))
    }

    fn own_or(&self, doc_type: &str, want: impl Fn(&DataRule) -> bool) -> Map<String, Value> {
        let mut any = vec![json!({"$createdBy": self.app})];
        any.extend(self.rules(doc_type, want).map(|r| Value::Object(r.filter.clone())));
        one_of(any)
    }
}

/// A filter matching any of `filters`; an empty filter among them matches all.
fn one_of(filters: Vec<Value>) -> Map<String, Value> {
    if filters.iter().any(|f| f.as_object().is_some_and(Map::is_empty)) {
        return Map::new();
    }
    match <[Value; 1]>::try_from(filters) {
        Ok([Value::Object(only)]) => only,
        Ok([other]) => Map::from_iter([("$or".to_owned(), Value::Array(vec![other]))]),
        Err(filters) => Map::from_iter([("$or".to_owned(), Value::Array(filters))]),
    }
}

/// A filter matching both `a` and `b`.
pub fn both(a: &Map<String, Value>, b: Map<String, Value>) -> Map<String, Value> {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b,
        (_, true) => a.clone(),
        _ => Map::from_iter([(
            "$and".to_owned(),
            Value::Array(vec![Value::Object(a.clone()), Value::Object(b)]),
        )]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(rules: Vec<DataRule>) -> Scope {
        Scope::new("org.x.app", AppGrants { data: rules, capabilities: Vec::new() })
    }

    fn rule(doc_type: &str, access: &[Access], filter: Value) -> DataRule {
        DataRule {
            doc_type: doc_type.into(),
            access: access.to_vec(),
            filter: filter.as_object().unwrap().clone(),
        }
    }

    #[test]
    fn without_grants_an_app_sees_only_its_own_documents_and_creates_none() {
        let s = scope(Vec::new());
        assert_eq!(Value::Object(s.read("Note")), json!({"$createdBy": "org.x.app"}));
        assert_eq!(Value::Object(s.write("Note")), json!({"$createdBy": "org.x.app"}));
        assert_eq!(s.create("Note"), None);
        assert_eq!(
            Value::Object(s.read("App")),
            json!({"$or": [{"$createdBy": "org.x.app"}, {"id": "org.x.app"}]}),
            "its own App document, to find its code"
        );
        assert_eq!(Value::Object(s.write("App")), json!({"$createdBy": "org.x.app"}), "but not change it");
    }

    #[test]
    fn grants_widen_by_type_and_filter() {
        let s = scope(vec![
            rule("Image", &[Access::Read], json!({"album": "Family"})),
            rule("Note", &[Access::Write, Access::Create], json!({})),
        ]);
        assert_eq!(
            Value::Object(s.read("Image")),
            json!({"$or": [{"$createdBy": "org.x.app"}, {"album": "Family"}]})
        );
        assert_eq!(Value::Object(s.write("Image")), json!({"$createdBy": "org.x.app"}), "read isn't write");
        assert_eq!(Value::Object(s.read("Note")), json!({}), "write covers reading, and {{}} is everything");
        assert_eq!(s.create("Note"), Some(Map::new()));
        assert_eq!(s.create("Image"), None);
        let all = scope(vec![rule("*", &[Access::Read], json!({}))]);
        assert_eq!(Value::Object(all.read("Anything")), json!({}), "* covers every type");
        assert_eq!(Value::Object(all.write("Anything")), json!({"$createdBy": "org.x.app"}));
        let q =
            s.query(&Query { filter: json!({"x": 1}).as_object().unwrap().clone(), ..Query::all("Image") });
        assert_eq!(
            Value::Object(q.filter),
            json!({"$and": [{"x": 1}, {"$or": [{"$createdBy": "org.x.app"}, {"album": "Family"}]}]})
        );
    }
}
