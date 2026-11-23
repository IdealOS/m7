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
//!
//! A field whose value is `{"$link": "<id>"}` links to another document, and one
//! whose value is `{"$blob": "<sha-256>"}` refers to a stored blob. Filters follow
//! links: `{"from": {"$link": id}}` matches documents whose `from` field (or a field
//! under it, or an item of it) links to `id`; top-level `{"$linksTo": id}` matches
//! documents with any link to `id`, and `{"$linkedFrom": id}` the documents `id`
//! links to.
//!
//! Documents of a system type (`Email`, `Contact`, ...; see `schemas/`) must match
//! that type's schema; other types are free-form.

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
/// | `history` | [`IdParams`]    | [`HistoryResult`] |
/// | `at`     | [`AtParams`]     | [`AtResult`]   |
/// | `changes` | [`ChangesParams`] | [`ChangesResult`] |
/// | `undo`   | [`UndoParams`]   | [`UndoResult`] |
/// | `watch`  | [`Query`]        | streamed [`WatchEvent`]s |
/// | `links`  | [`IdParams`]     | [`LinksResult`] |
/// | `diff`   | [`DiffParams`]   | [`DiffResult`] |
/// | `schemas` | none            | [`SchemasResult`] |
/// | `snapshot` | none           | [`SnapshotInfo`] |
/// | `snapshots` | none          | [`SnapshotsResult`] |
/// | `restore` | [`RestoreParams`] | [`RestoreResult`] |
/// | `put_blob` | one descriptor | [`BlobInfo`] |
/// | `get_blob` | [`HashParams`] | [`BlobInfo`] and one descriptor |
/// | `blob_info` | [`HashParams`] | [`BlobInfo`] |
///
/// `put_blob` stores the contents of a regular file (or memfd) passed with the
/// request, and `get_blob` answers with a read-only descriptor for the blob.
///
/// `watch` is a live query, sent as a streamed request (`"stream": true`). Its
/// first part is the query's result; each later part is what changed since the
/// one before. Changes made close together arrive as one part. It stays open until
/// the caller cancels it or disconnects.
pub mod method {
    pub const CREATE: &str = "create";
    pub const GET: &str = "get";
    pub const UPDATE: &str = "update";
    pub const DELETE: &str = "delete";
    pub const QUERY: &str = "query";
    pub const SEARCH: &str = "search";
    pub const HISTORY: &str = "history";
    pub const AT: &str = "at";
    pub const CHANGES: &str = "changes";
    pub const UNDO: &str = "undo";
    pub const WATCH: &str = "watch";
    pub const LINKS: &str = "links";
    pub const DIFF: &str = "diff";
    pub const SCHEMAS: &str = "schemas";
    pub const SNAPSHOT: &str = "snapshot";
    pub const SNAPSHOTS: &str = "snapshots";
    pub const RESTORE: &str = "restore";
    pub const PUT_BLOB: &str = "put_blob";
    pub const GET_BLOB: &str = "get_blob";
    pub const BLOB_INFO: &str = "blob_info";
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

/// A part of `watch`'s streamed answer.
///
/// To keep a list up to date: drop the `removed` ids, replace the `changed`
/// documents, append the `added` ones, and then, if `order` is present, sort the
/// list to match it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum WatchEvent {
    /// The first part: every matching document, in order.
    Initial { docs: Vec<Doc> },
    /// What changed since the previous part.
    Diff {
        /// Documents that are now in the result and weren't before.
        added: Vec<Doc>,
        /// Documents still in the result whose contents changed.
        changed: Vec<Doc>,
        /// Ids of documents no longer in the result (deleted, no longer matching,
        /// or pushed past the limit).
        removed: Vec<String>,
        /// Every id in the result, in order. Present whenever the order isn't
        /// just the previous one with the removed ids taken out (so whenever
        /// something was added or moved).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        order: Option<Vec<String>>,
    },
}

/// What a change set did to a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Create,
    Update,
    Delete,
    /// Undoing a delete brought it back.
    Restore,
}

/// One change set's effect on one document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct VersionInfo {
    pub change_set: i64,
    /// When, in milliseconds since the Unix epoch.
    pub at: i64,
    /// Who: the service name or app id that made the change.
    pub origin: String,
    pub kind: ChangeKind,
    /// The change set this one undid, if it was an undo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undoes: Option<i64>,
}

/// Result of `history`: every change to the document, oldest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HistoryResult {
    pub versions: Vec<VersionInfo>,
}

/// Params of `at`: document `id` as it was just after change set `change_set`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AtParams {
    pub id: String,
    pub change_set: i64,
}

/// Result of `at`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AtResult {
    pub data: Map<String, Value>,
}

/// Params of `changes`: the most recent change sets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChangesParams {
    /// At most this many (default 100, at most 10,000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// One write, possibly to several documents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChangeSetInfo {
    pub id: i64,
    pub at: i64,
    pub origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undoes: Option<i64>,
    /// Ids of the documents it touched.
    pub docs: Vec<String>,
}

/// Result of `changes`, newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ChangesResult {
    pub change_sets: Vec<ChangeSetInfo>,
}

/// Params of `undo`: revert change set `change_set` with a new change set. Undoing
/// an undo redoes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UndoParams {
    pub change_set: i64,
}

/// Result of `undo`: the change set the undo made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UndoResult {
    pub change_set: i64,
}

/// A link out of a document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LinkInfo {
    /// The field holding the link, such as `from.contact` or `tracks.3`.
    pub path: String,
    /// The target's id.
    pub id: String,
    /// Whether the target exists (and isn't deleted).
    pub exists: bool,
}

/// A link into a document from another live document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BacklinkInfo {
    /// The linking document.
    pub id: String,
    #[serde(rename = "type")]
    pub doc_type: String,
    /// Its field holding the link.
    pub path: String,
}

/// Result of `links`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LinksResult {
    pub outgoing: Vec<LinkInfo>,
    pub incoming: Vec<BacklinkInfo>,
}

/// Params of `diff`: document `id` from just after change set `from` (0 for
/// before it was created) to just after change set `to`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DiffParams {
    pub id: String,
    pub from: i64,
    pub to: i64,
}

/// One field that differs. Objects are compared field by field, arrays and other
/// values as a whole.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FieldChangeInfo {
    pub path: String,
    /// Absent when the field didn't exist before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<Value>,
    /// Absent when the field doesn't exist after.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Value>,
}

/// Result of `diff`, sorted by path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DiffResult {
    pub changes: Vec<FieldChangeInfo>,
}

/// Result of `schemas`: each system type's JSON Schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SchemasResult {
    pub schemas: std::collections::BTreeMap<String, Value>,
}

/// A snapshot of the database.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SnapshotInfo {
    /// Its name, which `restore` takes.
    pub name: String,
    /// When it was taken, in milliseconds since the Unix epoch.
    pub taken: i64,
    /// The newest change set it holds.
    pub change_set: i64,
    pub bytes: u64,
}

/// Result of `snapshots`, newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SnapshotsResult {
    pub snapshots: Vec<SnapshotInfo>,
}

/// Params of `restore`: replace the database with snapshot `name`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RestoreParams {
    pub name: String,
}

/// Result of `restore`: the snapshot of the state it replaced, so restoring that
/// one undoes the restore.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RestoreResult {
    pub saved: SnapshotInfo,
}

/// Params of `get_blob` and `blob_info`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HashParams {
    /// The blob's SHA-256, lowercase hex.
    pub hash: String,
}

/// A stored blob.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BlobInfo {
    pub hash: String,
    pub size: u64,
    /// When it was first stored, in milliseconds since the Unix epoch.
    pub created: i64,
    /// How many fields of live documents refer to it.
    pub refs: u64,
}

/// Largest blob `put_blob` takes.
pub const MAX_BLOB_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Most documents one query returns; a larger `limit` is an error.
pub const MAX_QUERY_LIMIT: u32 = 10_000;
/// The `limit` a query gets when it names none.
pub const DEFAULT_QUERY_LIMIT: u32 = 100;

/// Most live queries (`watch`) the service keeps open at once, across every caller.
pub const MAX_WATCHES: usize = 4096;

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
