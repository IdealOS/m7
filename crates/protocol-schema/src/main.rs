//! Prints the protocol crate's wire types as one JSON Schema document (draft
//! 2020-12). `make protocol` writes it to `crates/protocol/schema.json` and
//! generates `sdk/src/protocol.ts` from it with `tools/gen-protocol.ts`.
//!
//! Besides the types under `$defs`, the document carries the protocol's constants
//! under `x-constants`, which JSON Schema has no keyword for.

use protocol::bus::{self, code};
use protocol::{backup, broker, db, net, perm, pty, secrets};
use serde_json::{Value, json};

fn document() -> Value {
    let mut g = schemars::generate::SchemaSettings::draft2020_12().into_generator();
    g.subschema_for::<protocol::Message>();
    g.subschema_for::<protocol::Commit>();
    g.subschema_for::<protocol::OpenWindowParams>();
    g.subschema_for::<protocol::WindowEvent>();
    g.subschema_for::<protocol::Clipboard>();
    g.subschema_for::<db::SpliceParams>();
    g.subschema_for::<protocol::session::AppParams>();
    g.subschema_for::<protocol::session::AppsResult>();
    g.subschema_for::<broker::HelloResult>();
    g.subschema_for::<broker::NameParams>();
    g.subschema_for::<broker::ListResult>();
    g.subschema_for::<broker::AdoptParams>();
    g.subschema_for::<broker::AdoptResult>();
    g.subschema_for::<broker::TapEvent>();
    g.subschema_for::<db::Doc>();
    g.subschema_for::<db::Query>();
    g.subschema_for::<db::CreateParams>();
    g.subschema_for::<db::IdParams>();
    g.subschema_for::<db::UpdateParams>();
    g.subschema_for::<db::SearchParams>();
    g.subschema_for::<db::DocsResult>();
    g.subschema_for::<db::HistoryResult>();
    g.subschema_for::<db::AtParams>();
    g.subschema_for::<db::AtResult>();
    g.subschema_for::<db::ChangesParams>();
    g.subschema_for::<db::ChangesResult>();
    g.subschema_for::<db::UndoParams>();
    g.subschema_for::<db::UndoResult>();
    g.subschema_for::<db::WatchEvent>();
    g.subschema_for::<db::LinksResult>();
    g.subschema_for::<db::DiffParams>();
    g.subschema_for::<db::DiffResult>();
    g.subschema_for::<db::SchemasResult>();
    g.subschema_for::<db::SnapshotInfo>();
    g.subschema_for::<db::SnapshotsResult>();
    g.subschema_for::<db::RestoreParams>();
    g.subschema_for::<db::RestoreResult>();
    g.subschema_for::<db::PinParams>();
    g.subschema_for::<db::CompactResult>();
    g.subschema_for::<db::HashParams>();
    g.subschema_for::<db::BlobInfo>();
    g.subschema_for::<protocol::scene::PromptParams>();
    g.subschema_for::<protocol::scene::PromptResult>();
    g.subschema_for::<perm::Grant>();
    g.subschema_for::<perm::GrantsParams>();
    g.subschema_for::<perm::GrantsResult>();
    g.subschema_for::<perm::RequestParams>();
    g.subschema_for::<perm::RequestResult>();
    g.subschema_for::<perm::GrantParams>();
    g.subschema_for::<perm::RevokeParams>();
    g.subschema_for::<perm::RevokeResult>();
    g.subschema_for::<perm::AuditParams>();
    g.subschema_for::<perm::AuditResult>();
    g.subschema_for::<secrets::PutParams>();
    g.subschema_for::<secrets::NameParams>();
    g.subschema_for::<secrets::GetResult>();
    g.subschema_for::<secrets::ListResult>();
    g.subschema_for::<pty::OpenParams>();
    g.subschema_for::<pty::SessionEvent>();
    g.subschema_for::<pty::IdParams>();
    g.subschema_for::<pty::WriteParams>();
    g.subschema_for::<pty::ResizeParams>();
    g.subschema_for::<pty::ScreenFrame>();
    g.subschema_for::<net::NetStatus>();
    g.subschema_for::<net::NetworksParams>();
    g.subschema_for::<net::NetworksResult>();
    g.subschema_for::<net::ConnectParams>();
    g.subschema_for::<net::SsidParams>();
    g.subschema_for::<net::WifiParams>();
    g.subschema_for::<backup::BackupStatus>();
    g.subschema_for::<backup::DriveParams>();
    g.subschema_for::<backup::BackedUp>();
    g.subschema_for::<backup::BackupsResult>();
    g.subschema_for::<backup::RestoreBackupParams>();
    g.subschema_for::<protocol::session::InfoResult>();
    g.subschema_for::<protocol::scene::ScreenInfo>();
    g.subschema_for::<db::TypesResult>();
    g.subschema_for::<db::StatsResult>();
    let mut defs = g.take_definitions(true);

    // Broker error codes are string constants (services may send their own), so
    // they are listed here rather than derived.
    let codes: Vec<Value> =
        code::ALL.iter().map(|(value, doc)| json!({"const": value, "description": doc_text(doc)})).collect();
    defs.insert(
        "BrokerErrorCode".into(),
        json!({"description": "Error codes the broker itself sends. Services may send their own.", "oneOf": codes}),
    );

    let constant = |value: Value, description: &str| json!({"value": value, "description": description});
    let constants = json!({
        "BROKER": constant(bus::BROKER.into(), "The broker's own name: `hello`, `claim`, `release` and `list` go to it, and `adopt`, with which session hands it connections for services and apps."),
        "TAP_TOPIC": constant(bus::TAP_TOPIC.into(), "The broker's trace of every message; only trusted connections may subscribe."),
        "DEFAULT_TIMEOUT_MS": constant(bus::DEFAULT_TIMEOUT_MS.into(), "How long the broker waits for a response before answering `timeout` itself, unless the request sets `timeout_ms`."),
        "MAX_TIMEOUT_MS": constant(bus::MAX_TIMEOUT_MS.into(), "Longest `timeout_ms` the broker honors; longer ones are shortened to this."),
        "MAX_PENDING_REQUESTS": constant(bus::MAX_PENDING_REQUESTS.into(), "Most requests one connection may have waiting for an answer at once; past it the broker answers `limit_exceeded`."),
        "MAX_STREAMS": constant(bus::MAX_STREAMS.into(), "Most streamed requests one connection may have open at once; they don't count against `MAX_PENDING_REQUESTS`."),
        "MAX_SUBSCRIPTIONS": constant(bus::MAX_SUBSCRIPTIONS.into(), "Most subscription patterns one connection may hold."),
        "MAX_NAMES": constant(bus::MAX_NAMES.into(), "Most service names one connection may claim."),
        "DB_SERVICE": constant(db::SERVICE.into(), "The name the database service claims."),
        "MAX_QUERY_LIMIT": constant(db::MAX_QUERY_LIMIT.into(), "Most documents one query returns; a larger `limit` is an error."),
        "DEFAULT_QUERY_LIMIT": constant(db::DEFAULT_QUERY_LIMIT.into(), "The `limit` a query gets when it names none."),
        "MAX_WATCHES": constant(db::MAX_WATCHES.into(), "Most live queries (`watch`) the database service keeps open at once, across every caller."),
        "MAX_BLOB_BYTES": constant(db::MAX_BLOB_BYTES.into(), "Largest blob `put_blob` takes."),
        "COMPOSITOR_SERVICE": constant(protocol::scene::SERVICE.into(), "The name the compositor claims."),
        "WINDOW_CONTENT": constant(protocol::scene::CONTENT.into(), "The node id of a window's content area in that window's commits."),
        "MAX_WINDOW_NODES": constant(protocol::scene::MAX_WINDOW_NODES.into(), "Most nodes one window may hold."),
        "SESSION_SERVICE": constant(protocol::session::SERVICE.into(), "The name session claims."),
        "PERM_SERVICE": constant(perm::SERVICE.into(), "The name permd claims."),
        "GRANTS_TOPIC": constant(perm::GRANTS_TOPIC.into(), "Where permd publishes every grant whenever they change."),
        "SECRETS_SERVICE": constant(secrets::SERVICE.into(), "The name secretsd claims."),
        "PTY_SERVICE": constant(pty::SERVICE.into(), "The name ptyd claims."),
        "NET_SERVICE": constant(net::SERVICE.into(), "The name netd claims."),
        "BACKUP_SERVICE": constant(backup::SERVICE.into(), "The name backupd claims."),
        "MAX_FRAME_BYTES": constant(bus::MAX_FRAME_BYTES.into(), "Largest frame the broker accepts, not counting the 4-byte length prefix."),
        "MAX_FDS_PER_MESSAGE": constant(protocol::unix::MAX_FDS_PER_MESSAGE.into(), "Most file descriptors one message may carry."),
    });

    let mut doc = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "idealos:protocol",
        "title": "idealos wire protocol",
        "description": "Bus frames are a 4-byte big-endian length followed by one UTF-8 JSON `Message`.",
        "$comment": "Generated from crates/protocol by crates/protocol-schema; do not edit. Run `make protocol`.",
        "x-constants": constants,
        "$defs": Value::Object(defs),
    });
    clean_descriptions(&mut doc);
    doc
}

/// A doc comment as `concat!` joined it: each line keeps rustdoc's leading space.
fn doc_text(doc: &str) -> String {
    doc.split('\n').map(str::trim).collect::<Vec<_>>().join("\n").trim().to_owned()
}

/// Rustdoc links (`` [`topic_matches`] ``) mean nothing outside Rust: keep just
/// the code span.
fn clean_descriptions(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                match v {
                    Value::String(text) if key == "description" => *text = strip_doc_links(text),
                    _ => clean_descriptions(v),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(clean_descriptions),
        _ => {}
    }
}

fn strip_doc_links(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("[`") {
        let Some(len) = rest[start..].find("`]") else { break };
        out.push_str(&rest[..start]);
        // Drop the crate path, keep the name: `crate::unix` -> `unix`.
        let name = &rest[start + 2..start + len];
        out.push('`');
        out.push_str(name.rsplit("::").next().unwrap_or(name));
        out.push('`');
        rest = &rest[start + len + 2..];
    }
    out.push_str(rest);
    out
}

fn render(doc: &Value) -> String {
    let mut text = serde_json::to_string_pretty(doc).expect("schema serializes");
    text.push('\n');
    text
}

fn main() {
    print!("{}", render(&document()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;

    /// Where the committed copy lives, relative to the workspace root.
    const SCHEMA_PATH: &str = "crates/protocol/schema.json";

    fn defs() -> Map<String, Value> {
        document()["$defs"].as_object().unwrap().clone()
    }

    /// The committed schema (and so the generated TypeScript) matches the Rust types.
    #[test]
    fn committed_schema_is_current() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
        let committed = std::fs::read_to_string(format!("{root}/{SCHEMA_PATH}")).unwrap_or_default();
        assert!(
            committed == render(&document()),
            "{SCHEMA_PATH} is out of date with crates/protocol; run `make protocol`"
        );
    }

    #[test]
    fn every_wire_type_is_exported() {
        let defs = defs();
        for name in [
            "Message",
            "Sender",
            "BusError",
            "BrokerErrorCode",
            "Commit",
            "SceneOp",
            "NodeKind",
            "HelloResult",
            "NameParams",
            "ListResult",
            "AdoptParams",
            "AdoptResult",
            "TapEvent",
            "Doc",
            "Query",
            "OrderBy",
            "Direction",
            "CreateParams",
            "IdParams",
            "UpdateParams",
            "SearchParams",
            "DocsResult",
        ] {
            assert!(defs.contains_key(name), "{name} missing");
        }
        let codes = defs["BrokerErrorCode"]["oneOf"].as_array().unwrap();
        assert_eq!(codes.len(), code::ALL.len());
        assert_eq!(codes[0]["const"], code::BAD_REQUEST);
        assert!(codes[0]["description"].as_str().unwrap().starts_with("The frame was valid JSON"));
    }

    #[test]
    fn rustdoc_links_become_code_spans() {
        assert_eq!(strip_doc_links("see [`crate::unix`] and [`x`]."), "see `unix` and `x`.");
        assert_eq!(strip_doc_links("a [`dangling"), "a [`dangling");
        assert!(!render(&document()).contains("[`"));
    }

    /// Optional fields are the ones serde may leave out; a client must send the rest.
    #[test]
    fn required_fields_follow_serde() {
        let defs = defs();
        let request = defs["Message"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["properties"]["kind"]["const"] == "request")
            .unwrap();
        assert_eq!(request["required"], json!(["kind", "id", "to", "method"]));
    }
}
