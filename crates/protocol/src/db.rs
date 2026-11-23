//! The database's wire types: documents and queries, as the `db` service takes
//! and returns them.
//!
//! A document is a JSON object of a named type. Its id is a UUIDv7 string, so ids
//! sort by creation time. A query names a type, a filter (`where`) in a small
//! Mongo-style language, an order and a limit:
//!
//! ```json
//! {"type": "Email", "where": {"read": false, "labels": {"$contains": "INBOX"}},
//!  "orderBy": [["date", "desc"]], "limit": 50}
//! ```
//!
//! A filter is an object whose keys are field paths (`from.name`, `items.0`) or
//! `$and`, `$or` and `$not`. A field's value is a JSON scalar to compare equal to,
//! or an object of operators: `$eq`, `$ne`, `$gt`, `$gte`, `$lt`, `$lte`, `$in`,
//! `$nin`, `$exists` and `$contains` (array membership). Comparisons are typed: a
//! number never equals a string or a boolean. `{"field": null}` matches documents
//! where the field is null or missing, and `$ne` matches documents without the field.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The service name `dbd` claims.
pub const SERVICE: &str = "db";

/// The `db` service's methods.
///
/// | method   | params           | result         |
/// |----------|------------------|----------------|
/// | `create` | [`CreateParams`] | [`Doc`]        |
/// | `get`    | [`IdParams`]     | [`Doc`]        |
/// | `update` | [`UpdateParams`] | [`Doc`]        |
/// | `delete` | [`IdParams`]     | null           |
/// | `query`  | [`Query`]        | [`DocsResult`] |
/// | `search` | [`SearchParams`] | [`DocsResult`] |
pub mod method {
    pub const CREATE: &str = "create";
    pub const GET: &str = "get";
    pub const UPDATE: &str = "update";
    pub const DELETE: &str = "delete";
    pub const QUERY: &str = "query";
    pub const SEARCH: &str = "search";
}

/// Error codes the `db` service sends, besides the broker's `bad_request`.
pub mod code {
    /// No live document has the id.
    pub const NOT_FOUND: &str = "not_found";
    /// The database itself failed (disk, corruption); the request may be fine.
    pub const INTERNAL: &str = "internal";
}

/// Params of `create`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CreateParams {
    #[serde(rename = "type")]
    pub doc_type: String,
    #[serde(default)]
    pub data: Map<String, Value>,
}

/// Params of `get` and `delete`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct IdParams {
    pub id: String,
}

/// Params of `update`: a JSON merge patch (RFC 7396) for document `id`. A key set
/// to null is removed, an object merges into the object there, and anything else
/// replaces the old value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UpdateParams {
    pub id: String,
    pub patch: Map<String, Value>,
}

/// Params of `search`: documents containing every word of `text`, best first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SearchParams {
    pub text: String,
    /// Only documents of this type.
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub doc_type: Option<String>,
    /// At most this many (default 100, at most 10,000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// Result of `query` and `search`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DocsResult {
    pub docs: Vec<Doc>,
}

/// Most documents one query returns; a larger `limit` is an error.
pub const MAX_QUERY_LIMIT: u32 = 10_000;
/// The `limit` a query gets when it names none.
pub const DEFAULT_QUERY_LIMIT: u32 = 100;

/// A stored document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Doc {
    /// A UUIDv7, so ids sort by creation time.
    pub id: String,
    /// The document's type, such as `Email`.
    #[serde(rename = "type")]
    pub doc_type: String,
    /// When it was created, in milliseconds since the Unix epoch.
    pub created: i64,
    /// When it last changed, in milliseconds since the Unix epoch.
    pub updated: i64,
    pub data: Map<String, Value>,
}

/// A query: documents of one type that match a filter, in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Query {
    #[serde(rename = "type")]
    pub doc_type: String,
    /// Field paths mapped to a value to equal or an object of operators (`$eq`,
    /// `$ne`, `$gt`, `$gte`, `$lt`, `$lte`, `$in`, `$nin`, `$exists`, `$contains`),
    /// plus `$and`, `$or` and `$not`. Empty matches every document of the type.
    #[serde(default, rename = "where", skip_serializing_if = "Map::is_empty")]
    pub filter: Map<String, Value>,
    /// Sort keys, most significant first. Ties keep creation order.
    #[serde(default, rename = "orderBy", skip_serializing_if = "Vec::is_empty")]
    pub order_by: Vec<OrderBy>,
    /// At most this many documents (default 100, at most 10,000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Skip this many matching documents first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
}

impl Query {
    /// Every document of `doc_type`, in creation order.
    pub fn all(doc_type: impl Into<String>) -> Query {
        Query {
            doc_type: doc_type.into(),
            filter: Map::new(),
            order_by: Vec::new(),
            limit: None,
            offset: None,
        }
    }
}

/// A sort key: a field path and a direction, written `["date", "desc"]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct OrderBy(pub String, pub Direction);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Asc,
    Desc,
}

/// Whether `name` can be a document type: a letter, then letters, digits, `_` and
/// `.` (for namespaced types such as `org.example.Note`), at most 128 bytes.
pub fn valid_type(name: &str) -> bool {
    name.len() <= 128
        && name.starts_with(|c: char| c.is_ascii_alphabetic())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_plans_query_parses() {
        let q: Query = serde_json::from_value(json!({
            "type": "Email",
            "where": {"read": false, "labels": {"$contains": "INBOX"}},
            "orderBy": [["date", "desc"]],
            "limit": 50
        }))
        .unwrap();
        assert_eq!(q.doc_type, "Email");
        assert_eq!(q.order_by, [OrderBy("date".into(), Direction::Desc)]);
        assert_eq!(q.limit, Some(50));
        assert_eq!(serde_json::to_value(Query::all("Note")).unwrap(), json!({"type": "Note"}));
    }

    #[test]
    fn type_names() {
        for good in ["Email", "org.example.Note", "A_1"] {
            assert!(valid_type(good), "{good}");
        }
        for bad in ["", "1Email", "Em ail", "E'mail", "é", &"x".repeat(129)] {
            assert!(!valid_type(bad), "{bad}");
        }
    }
}
