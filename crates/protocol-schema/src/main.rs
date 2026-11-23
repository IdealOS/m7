//! Prints the protocol crate's wire types as one JSON Schema document (draft
//! 2020-12). `make protocol` writes it to `crates/protocol/schema.json` and
//! generates `sdk/src/protocol.ts` from it with `tools/gen-protocol.ts`.
//!
//! Besides the types under `$defs`, the document carries the protocol's constants
//! under `x-constants`, which JSON Schema has no keyword for.

use protocol::bus::{self, code};
use serde_json::{Value, json};

fn document() -> Value {
    let mut g = schemars::generate::SchemaSettings::draft2020_12().into_generator();
    g.subschema_for::<protocol::Message>();
    g.subschema_for::<protocol::Commit>();
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
        for name in ["Message", "Sender", "BusError", "BrokerErrorCode", "Commit", "SceneOp", "NodeKind"] {
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
