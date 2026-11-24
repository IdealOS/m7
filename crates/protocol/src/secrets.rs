//! Secrets: OAuth tokens, passwords and keys, kept by `secretsd`.
//!
//! secretsd claims `secrets`. Each secret belongs to whoever stored it: the
//! service name or app id the broker stamped on the request, so nobody can read or
//! replace another's secrets, and there is no way to list anyone else's. They live
//! encrypted in their own file, outside the database, so they never appear in
//! queries, history, snapshots or sync.

use serde::{Deserialize, Serialize};

/// The name secretsd claims.
pub const SERVICE: &str = "secrets";

/// Longest secret name, in bytes.
pub const MAX_NAME_BYTES: usize = 256;
/// Largest secret, in bytes.
pub const MAX_VALUE_BYTES: usize = 64 * 1024;
/// Most secrets one owner may keep.
pub const MAX_PER_OWNER: usize = 1024;

/// secretsd's methods, all on the caller's own secrets.
///
/// | method   | params         | result |
/// |----------|----------------|--------|
/// | `put`    | [`PutParams`]  | null |
/// | `get`    | [`NameParams`] | [`GetResult`] (`not_found` if there's none) |
/// | `delete` | [`NameParams`] | null |
/// | `list`   | none           | [`ListResult`] |
pub mod method {
    pub const PUT: &str = "put";
    pub const GET: &str = "get";
    pub const DELETE: &str = "delete";
    pub const LIST: &str = "list";
}

/// Params of `put`: store `value` as `name`, replacing what was there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PutParams {
    pub name: String,
    pub value: String,
}

/// Params of `get` and `delete`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NameParams {
    pub name: String,
}

/// Result of `get`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GetResult {
    pub value: String,
}

/// Result of `list`: the names of the caller's secrets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ListResult {
    pub names: Vec<String>,
}
