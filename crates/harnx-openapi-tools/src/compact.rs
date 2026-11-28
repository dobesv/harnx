//! Compact schema transformation with cycle-safe local-reference rewriting.
//!
//! A catalog tool schema must be small (the whole catalog is one NATS KV
//! registration) but semantically complete. [`SchemaCompactor`] walks a source
//! schema and:
//!
//! - drops documentation-only keys (`description`, `example`/`examples`,
//!   `title`, `xml`, `externalDocs`, `x-*` vendor extensions);
//! - keeps every constraining keyword: `type`, `format`, `enum`, `const`,
//!   `pattern`, `minimum`/`maximum`, `minLength`/`maxLength`, `minItems`/
//!   `maxItems`, `uniqueItems`, `required`, `additionalProperties`,
//!   `patternProperties`, `nullable`, `default`, `discriminator`,
//!   `readOnly`/`writeOnly`, ...;
//! - replaces each local `$ref` with `#/$defs/<name>` and collects only the
//!   reachable definitions into a per-operation `$defs` map — including
//!   references that carry sibling keywords (the siblings describe this use,
//!   so cycles through them stay in the `$ref` form instead of inlining
//!   until the depth guard trips);
//! - rejects external references (`http://…`, `other.yaml#/…`, `#`) and
//!   unresolvable pointers with the operation context attached;
//! - never alters schema semantics. An upstream schema that contradicts
//!   itself (a `required` name it never declares while forbidding additional
//!   properties) is reported as a diagnostic and kept verbatim; deciding how
//!   to handle such operations is a planning decision, not a silent repair.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

#[path = "compact/compactor.rs"]
mod compactor;
#[path = "compact/definition_names.rs"]
mod definition_names;
#[path = "compact/diagnostics.rs"]
mod diagnostics;
pub(crate) mod materialization;

pub use compactor::SchemaCompactor;

#[cfg(test)]
use crate::error::SpecError;

/// How many leading `#/$defs/…` references to follow when deriving wire
/// semantics (property names, binary detection) from a schema.
const MAX_REF_CHAIN: usize = 16;

/// First 8 hex characters of the SHA-256 of `input`, used as the stable first
/// disambiguator; callers still check each complete name for collisions.
pub fn short_hash(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    digest.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

/// Follow up to [`MAX_REF_CHAIN`] leading `#/$defs/…` references.
pub(crate) fn resolve_def_chain<'a>(schema: &'a Value, defs: &'a Map<String, Value>) -> &'a Value {
    let mut current = schema;
    for _ in 0..MAX_REF_CHAIN {
        let Some(reference) = current.get("$ref").and_then(Value::as_str) else {
            break;
        };
        let Some(name) = reference.strip_prefix("#/$defs/") else {
            break;
        };
        match defs.get(name) {
            Some(next) => current = next,
            None => break,
        }
    }
    current
}

/// Property names of an object-shaped schema, resolving one or more leading
/// `#/$defs/…` references through the operation's collected definitions.
///
/// Returns `None` unless the schema (after reference resolution) is a JSON
/// object with named properties; that is the only shape from which wire field
/// names may soundly be derived.
pub fn object_property_names(schema: &Value, defs: &Map<String, Value>) -> Option<Vec<String>> {
    let current = resolve_def_chain(schema, defs);
    current
        .get("properties")
        .and_then(Value::as_object)
        .filter(|properties| !properties.is_empty())
        .map(|properties| properties.keys().cloned().collect())
}

/// Whether a media-entry source schema describes raw bytes and therefore must
/// not be offered as a JSON `value` payload. Binary declarations behind a
/// local `$ref` are detected through the operation's collected definitions.
pub fn is_binary_schema(schema: &Value, defs: &Map<String, Value>) -> bool {
    let current = resolve_def_chain(schema, defs);
    let format = current.get("format").and_then(Value::as_str);
    format.is_some_and(|format| format == "binary" || format == "byte")
}

#[cfg(test)]
#[path = "compact/materialization_tests.rs"]
mod materialization_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn root() -> Value {
        json!({
            "components": {
                "schemas": {
                    "Node": {
                        "type": "object",
                        "description": "dropped",
                        "properties": {
                            "name": {"type": "string", "minLength": 1},
                            "children": {"type": "array", "items": {"$ref": "#/components/schemas/Node"}}
                        },
                        "required": ["name"]
                    },
                    "Pair": {"type": "object", "properties": {"a": {"$ref": "#/components/schemas/Node"}}},
                    "Tree": {"$ref": "#/components/schemas/Node"}
                },
                "parameters": {
                    "Node": {"name": "node", "in": "query", "schema": {"type": "string"}}
                }
            }
        })
    }

    #[test]
    fn rewrites_refs_collects_defs_and_drops_prose() {
        harnx_core::require_nextest();
        let root = root();
        let mut compactor = SchemaCompactor::new(&root, "TEST /test");
        let compacted = compactor
            .compact(&json!({"$ref": "#/components/schemas/Node"}))
            .unwrap();
        let defs = compactor.finish().unwrap();
        assert_eq!(compacted, json!({"$ref": "#/$defs/Node"}));
        let node = &defs["Node"];
        assert_eq!(node["type"], "object");
        assert!(node.get("description").is_none());
        assert_eq!(node["properties"]["name"]["minLength"], 1);
        assert_eq!(
            node["properties"]["children"]["items"],
            json!({"$ref": "#/$defs/Node"})
        );
    }

    #[test]
    fn prose_siblings_do_not_inline_the_target() {
        harnx_core::require_nextest();
        let root = root();
        let mut compactor = SchemaCompactor::new(&root, "TEST /test");
        let compacted = compactor
            .compact(&json!({"$ref": "#/components/schemas/Node", "description": "use text"}))
            .unwrap();
        let defs = compactor.finish().unwrap();
        assert_eq!(compacted, json!({"$ref": "#/$defs/Node"}));
        assert!(defs.contains_key("Node"));
    }

    #[test]
    fn recursive_refs_with_siblings_stay_cycle_safe() {
        harnx_core::require_nextest();
        // A schema whose every use of the recursive definition carries
        // sibling keywords used to inline the cycle until the depth guard
        // tripped; the $ref form must represent the recursion instead.
        let root = json!({
            "components": {"schemas": {
                "Loop": {
                    "type": "object",
                    "properties": {
                        "next": {"$ref": "#/components/schemas/Loop", "nullable": true},
                        "label": {"type": "string"}
                    },
                    "required": ["label"]
                }
            }}
        });
        let mut compactor = SchemaCompactor::new(&root, "TEST /loop");
        let compacted = compactor
            .compact(&json!({"$ref": "#/components/schemas/Loop"}))
            .unwrap();
        assert_eq!(compacted, json!({"$ref": "#/$defs/Loop"}));
        let defs = compactor.finish().unwrap();
        let next = &defs["Loop"]["properties"]["next"];
        assert_eq!(next["nullable"], json!(true));
        assert_eq!(next["$ref"], json!("#/$defs/Loop"));
    }

    #[test]
    fn name_collisions_get_hash_suffix() {
        harnx_core::require_nextest();
        let root = root();
        let mut compactor = SchemaCompactor::new(&root, "TEST /test");
        let schema = json!({
            "type": "object",
            "properties": {
                "node": {"$ref": "#/components/schemas/Node"},
                "param": {"$ref": "#/components/parameters/Node"}
            }
        });
        let compacted = compactor.compact(&schema).unwrap();
        let defs = compactor.finish().unwrap();
        let node_ref = compacted["properties"]["node"]["$ref"].as_str().unwrap();
        let param_ref = compacted["properties"]["param"]["$ref"].as_str().unwrap();
        assert_eq!(node_ref, "#/$defs/Node");
        let expected = format!(
            "#/$defs/Node_{}",
            short_hash("#/components/parameters/Node")
        );
        assert_eq!(param_ref, expected);
        assert!(defs.contains_key("Node"));
        assert!(defs.contains_key(param_ref.strip_prefix("#/$defs/").unwrap()));
    }

    #[test]
    fn chain_refs_resolve_through_defs() {
        harnx_core::require_nextest();
        let root = root();
        let mut compactor = SchemaCompactor::new(&root, "TEST /test");
        let compacted = compactor
            .compact(&json!({"$ref": "#/components/schemas/Tree"}))
            .unwrap();
        let defs = compactor.finish().unwrap();
        assert_eq!(compacted, json!({"$ref": "#/$defs/Tree"}));
        let names = object_property_names(&defs["Tree"], &defs).unwrap();
        assert_eq!(names, vec!["name".to_string(), "children".to_string()]);
    }

    #[test]
    fn external_refs_are_rejected_with_context() {
        harnx_core::require_nextest();
        let root = root();
        let mut compactor = SchemaCompactor::new(&root, "POST /upload");
        let error = compactor
            .compact(&json!({"$ref": "https://example.com/schema.json"}))
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("POST /upload"), "{message}");
        assert!(
            message.contains("https://example.com/schema.json"),
            "{message}"
        );
    }

    #[test]
    fn missing_pointers_are_unresolved_refs() {
        harnx_core::require_nextest();
        let root = root();
        let mut compactor = SchemaCompactor::new(&root, "GET /x");
        let error = compactor
            .compact(&json!({"$ref": "#/components/schemas/Missing"}))
            .unwrap_err();
        assert!(
            matches!(error, SpecError::UnresolvedRef { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn contradictory_required_names_are_diagnosed_not_repaired() {
        harnx_core::require_nextest();
        let root = json!({});
        let mut compactor = SchemaCompactor::new(&root, "POST /broken");
        let contradictory = json!({
            "type": "object",
            "required": ["ghost"],
            "properties": {"real": {"type": "string"}},
            "additionalProperties": false
        });
        let compacted = compactor.compact(&contradictory).unwrap();
        // Kept verbatim: no semantic weakening.
        assert_eq!(compacted["additionalProperties"], json!(false));
        assert_eq!(compacted["required"], json!(["ghost"]));
        let diagnostics = compactor.take_diagnostics();
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(
            diagnostics[0].contains("POST /broken"),
            "{}",
            diagnostics[0]
        );
        assert!(diagnostics[0].contains("ghost"), "{}", diagnostics[0]);
        assert!(compactor.take_diagnostics().is_empty());
    }

    #[test]
    fn pattern_properties_do_not_trigger_the_contradiction_diagnostic() {
        harnx_core::require_nextest();
        let root = json!({});
        let mut compactor = SchemaCompactor::new(&root, "POST /pattern");
        // `ghost` may be satisfied by the pattern; this schema is not
        // provably contradictory.
        let satisfiable = json!({
            "type": "object",
            "required": ["ghost"],
            "properties": {"real": {"type": "string"}},
            "patternProperties": {"^gh": true},
            "additionalProperties": false
        });
        compactor.compact(&satisfiable).unwrap();
        assert!(compactor.take_diagnostics().is_empty());
    }

    #[test]
    fn binary_schemas_are_detected_through_refs() {
        harnx_core::require_nextest();
        let direct = json!({"type": "string", "format": "binary"});
        assert!(is_binary_schema(&direct, &Map::new()));
        assert!(!is_binary_schema(&json!({"type": "string"}), &Map::new()));
        let defs = Map::from_iter([(
            "Bytes".to_string(),
            json!({"type": "string", "format": "binary"}),
        )]);
        assert!(is_binary_schema(&json!({"$ref": "#/$defs/Bytes"}), &defs));
    }
}
