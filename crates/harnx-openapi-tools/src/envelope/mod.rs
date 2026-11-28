//! Request-body input schemas: direct JSON values and the media envelope.
//!
//! Reviewed contract: for JSON-only bodies, `body` is the direct JSON value.
//! Anything else publishes a discriminated envelope:
//!
//! ```json
//! {
//!   "content_type": "<concrete media type>",
//!   "encoding": "json|text|form|multipart|base64",
//!   "value": <JSON value, when json>,
//!   "text": "<string, when text>",
//!   "fields": [{"name": "...", "value": "..."}, ...],
//!   "parts": [{"name": "...", "text": "..." | "base64": "...", ...}],
//!   "base64": "<bytes, when base64>"
//! }
//! ```
//!
//! The published schema enforces media precedence itself: an exact advertised
//! entry wins over matching wildcards, then `*/*`. When the JSON-value
//! sources disagree, `value` is constrained per selection through `if`/`then`
//! (which survives core conversion), instead of a `anyOf` union that would
//! let a wildcard `{}` erase an exact entry's strict schema.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::compact::is_binary_schema;
use crate::error::{SpecError, SpecResult};
use crate::media::{json_patch_array_schema, MediaBranch, MediaKind};

mod payload;

use payload::{fields_schema, parts_schema};

/// How an operation's request body appears in the tool input schema.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyInput {
    /// `body` is the direct JSON value (single JSON-family media entry).
    Direct(Value),
    /// `body` is the discriminated envelope described in the module docs.
    Envelope(Value),
}

/// Build the body input schema from an operation's media branches.
pub fn build_body_input(
    branches: &[MediaBranch],
    defs: &Map<String, Value>,
) -> SpecResult<BodyInput> {
    if branches.is_empty() {
        return Err(SpecError::invalid(
            "operation",
            "request body advertises no media entries, so no request can be constructed",
        ));
    }
    if branches.len() == 1 {
        return match branches[0].kind {
            MediaKind::Json => Ok(BodyInput::Direct(branches[0].source_schema.clone())),
            MediaKind::JsonPatch => Ok(BodyInput::Direct(json_patch_array_schema())),
            _ => Ok(BodyInput::Envelope(build_envelope(branches, defs))),
        };
    }
    Ok(BodyInput::Envelope(build_envelope(branches, defs)))
}

/// What the branches contribute to the envelope.
struct EnvelopeSources {
    encodings: Vec<&'static str>,
    concrete_types: Vec<String>,
    has_wildcard: bool,
    /// Exact JSON-family entries: `(media type, value schema)` pairs.
    json_entries: Vec<(String, Value)>,
    /// The wildcard entry's JSON value schema, when it describes one.
    wildcard_json: Option<Value>,
    fields_source: Option<Value>,
    parts_source: Option<Value>,
}

fn collect_sources(branches: &[MediaBranch], defs: &Map<String, Value>) -> EnvelopeSources {
    let mut sources = EnvelopeSources {
        encodings: Vec::new(),
        concrete_types: Vec::new(),
        has_wildcard: false,
        json_entries: Vec::new(),
        wildcard_json: None,
        fields_source: None,
        parts_source: None,
    };
    for branch in branches {
        collect_branch(&mut sources, branch, defs);
    }
    sources
}

/// Record what one media branch contributes to the envelope.
fn collect_branch(sources: &mut EnvelopeSources, branch: &MediaBranch, defs: &Map<String, Value>) {
    for encoding in branch.kind.encodings() {
        if !sources.encodings.contains(encoding) {
            sources.encodings.push(encoding);
        }
    }
    if branch.is_wildcard() {
        sources.has_wildcard = true;
    } else {
        sources.concrete_types.push(branch.media_type.clone());
    }
    match branch.kind {
        MediaKind::Json => push_unique_entry(
            &mut sources.json_entries,
            &branch.media_type,
            branch.source_schema.clone(),
        ),
        MediaKind::JsonPatch => push_unique_entry(
            &mut sources.json_entries,
            &branch.media_type,
            json_patch_array_schema(),
        ),
        MediaKind::Wildcard => collect_wildcard(sources, branch, defs),
        MediaKind::UrlencodedForm => keep_first(&mut sources.fields_source, branch),
        MediaKind::Multipart => keep_first(&mut sources.parts_source, branch),
        MediaKind::Text | MediaKind::Base64 => {}
    }
}

/// A wildcard entry contributes its schema to the JSON-value slot (when it
/// is not binary) and to both field/part name derivation.
fn collect_wildcard(
    sources: &mut EnvelopeSources,
    branch: &MediaBranch,
    defs: &Map<String, Value>,
) {
    // A wildcard entry with a non-binary source schema still describes a
    // JSON value when the caller selects a JSON type; binary wildcards
    // (direct or through local refs) do not.
    let describes_json =
        branch.source_schema != Value::Null && !is_binary_schema(&branch.source_schema, defs);
    if describes_json && sources.wildcard_json.is_none() {
        sources.wildcard_json = Some(branch.source_schema.clone());
    }
    keep_first(&mut sources.fields_source, branch);
    keep_first(&mut sources.parts_source, branch);
}

/// Store the branch's source schema unless one is already recorded.
fn keep_first(slot: &mut Option<Value>, branch: &MediaBranch) {
    if slot.is_none() {
        *slot = Some(branch.source_schema.clone());
    }
}

fn push_unique_entry(entries: &mut Vec<(String, Value)>, media_type: &str, schema: Value) {
    if !entries
        .iter()
        .any(|(existing_type, existing)| existing_type == media_type && *existing == schema)
    {
        entries.push((media_type.to_string(), schema));
    }
}

/// The `value` property plus the envelope `allOf` precedence constraints.
///
/// When every JSON-value source agrees on one schema, `value` is that schema
/// and no conditional constraints are needed. When they disagree, `value` is
/// unconstrained at the top level and `allOf` ties each selection to its own
/// schema: exact entries by `const`, the wildcard by exclusion of the exact
/// types. A top-level `anyOf` union would instead let a wildcard `{}` accept
/// anything, erasing an exact entry's strict schema.
fn value_property(sources: &EnvelopeSources) -> Option<(Value, Option<Value>)> {
    let mut distinct: Vec<Value> = sources
        .json_entries
        .iter()
        .map(|(_, schema)| schema.clone())
        .collect();
    if let Some(wildcard) = &sources.wildcard_json {
        distinct.push(wildcard.clone());
    }
    dedup_schemas(&mut distinct);
    if distinct.is_empty() {
        return None;
    }
    if distinct.len() == 1 {
        return Some((distinct.remove(0), None));
    }
    // Disagreeing sources: value stays permissive; allOf enforces per
    // content_type which schema applies.
    let mut constraints = Vec::new();
    for (media_type, schema) in &sources.json_entries {
        constraints.push(json!({
            "if": {
                "required": ["content_type"],
                "properties": {"content_type": {"const": media_type}}
            },
            "then": {"properties": {"value": schema}}
        }));
    }
    if let Some(wildcard) = &sources.wildcard_json {
        constraints.push(json!({
            "if": {
                "required": ["content_type"],
                "properties": {"content_type": {"not": {"enum": sources.concrete_types}}}
            },
            "then": {"properties": {"value": wildcard}}
        }));
    }
    Some((
        json!({"description": "JSON payload; the selected content type determines its schema (exact entries take precedence over wildcards)."}),
        Some(Value::Array(constraints)),
    ))
}

fn dedup_schemas(schemas: &mut Vec<Value>) {
    let mut seen: Vec<Value> = Vec::new();
    schemas.retain(|schema| {
        if seen.iter().any(|existing| existing == schema) {
            false
        } else {
            seen.push(schema.clone());
            true
        }
    });
}

/// Build the multi-media (or non-JSON single-media) request body envelope.
fn build_envelope(branches: &[MediaBranch], defs: &Map<String, Value>) -> Value {
    let sources = collect_sources(branches, defs);
    let mut envelope = Map::new();
    let mut precedence_constraints = None;
    envelope.insert("content_type".to_string(), content_type_schema(&sources));
    envelope.insert(
        "encoding".to_string(),
        json!({"type": "string", "enum": sources.encodings, "description": "Which payload property carries the body."}),
    );
    if let Some((value, constraints)) = value_property(&sources) {
        envelope.insert("value".to_string(), value);
        // Schema-level constraints, not a payload property: keep `allOf` at
        // the top level of the envelope schema.
        precedence_constraints = constraints;
    }
    if sources.encodings.contains(&"text") {
        envelope.insert("text".to_string(), json!({"type": "string"}));
    }
    if sources.encodings.contains(&"form") {
        envelope.insert(
            "fields".to_string(),
            fields_schema(sources.fields_source.as_ref(), defs),
        );
    }
    if sources.encodings.contains(&"multipart") {
        envelope.insert(
            "parts".to_string(),
            parts_schema(sources.parts_source.as_ref(), defs),
        );
    }
    if sources.encodings.contains(&"base64") {
        envelope.insert(
            "base64".to_string(),
            json!({"type": "string", "description": "Base64-encoded request bytes."}),
        );
    }
    let mut schema = Map::new();
    schema.insert("type".to_string(), json!("object"));
    schema.insert("additionalProperties".to_string(), json!(false));
    if let Some(constraints) = precedence_constraints {
        schema.insert("allOf".to_string(), constraints);
    }
    schema.insert("properties".to_string(), Value::Object(envelope));
    schema.insert("required".to_string(), json!(["content_type", "encoding"]));
    schema.insert(
        "description".to_string(),
        json!("Request body envelope. `encoding` names the payload property; `content_type` is the concrete media type sent on the wire."),
    );
    Value::Object(schema)
}

fn content_type_schema(sources: &EnvelopeSources) -> Value {
    if sources.has_wildcard {
        json!({
            "type": "string",
            "pattern": "^[A-Za-z0-9!#$&^_.+-]+/[A-Za-z0-9!#$&^_.+-]+$",
            "description": "Concrete media type to send. Must match an advertised entry, or a wildcard entry (exact entries take precedence)."
        })
    } else {
        json!({"type": "string", "enum": sources.concrete_types})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn branch(media_type: &str, schema: Value) -> MediaBranch {
        MediaBranch::new(media_type, schema, Map::new())
    }

    #[test]
    fn single_json_body_is_direct_value() {
        harnx_core::require_nextest();
        let branches = vec![branch("application/json", json!({"type": "string"}))];
        let input = build_body_input(&branches, &Map::new()).unwrap();
        assert_eq!(input, BodyInput::Direct(json!({"type": "string"})));
    }

    #[test]
    fn single_json_patch_body_is_patch_array_not_object_schema() {
        harnx_core::require_nextest();
        // Jira documents these entries with a wrong object schema.
        let branches = vec![branch(
            "application/json-patch+json",
            json!({"type": "object"}),
        )];
        let input = build_body_input(&branches, &Map::new()).unwrap();
        let BodyInput::Direct(schema) = input else {
            panic!("expected direct body");
        };
        assert_eq!(schema["type"], "array");
        assert_eq!(schema["items"]["properties"]["op"]["enum"][0], "add");
    }

    #[test]
    fn multi_media_bodies_get_envelope_with_concrete_types() {
        harnx_core::require_nextest();
        let branches = vec![
            branch("application/json", json!({"type": "string"})),
            branch("text/plain", json!({"type": "string"})),
        ];
        let BodyInput::Envelope(envelope) = build_body_input(&branches, &Map::new()).unwrap()
        else {
            panic!("expected envelope");
        };
        assert_eq!(
            envelope["properties"]["content_type"]["enum"],
            json!(["application/json", "text/plain"])
        );
        assert_eq!(
            envelope["properties"]["encoding"]["enum"],
            json!(["json", "text"])
        );
        assert_eq!(envelope["properties"]["value"], json!({"type": "string"}));
        assert_eq!(envelope["properties"]["text"], json!({"type": "string"}));
        assert!(envelope["properties"].get("fields").is_none());
        // Identical JSON sources need no conditional constraints.
        assert!(envelope.get("allOf").is_none());
    }

    #[test]
    fn wildcard_entry_offers_generic_encoders_and_concrete_selection() {
        harnx_core::require_nextest();
        let branches = vec![branch("*/*", json!({}))];
        let BodyInput::Envelope(envelope) = build_body_input(&branches, &Map::new()).unwrap()
        else {
            panic!("expected envelope");
        };
        let encodings: Vec<&str> = envelope["properties"]["encoding"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(
            encodings,
            vec!["json", "text", "form", "multipart", "base64"]
        );
        assert!(envelope["properties"]["content_type"]["pattern"].is_string());
        // No source object → free-form field/part names.
        assert_eq!(
            envelope["properties"]["fields"]["items"]["properties"]["name"],
            json!({"type": "string"})
        );
    }

    #[test]
    fn object_sources_constrain_field_and_part_names() {
        harnx_core::require_nextest();
        let source = json!({
            "type": "object",
            "properties": {"columns": {"type": "array", "items": {"type": "string"}}},
            "required": ["columns"]
        });
        let branches = vec![branch("*/*", source)];
        let BodyInput::Envelope(envelope) = build_body_input(&branches, &Map::new()).unwrap()
        else {
            panic!("expected envelope");
        };
        assert_eq!(
            envelope["properties"]["fields"]["items"]["properties"]["name"],
            json!({"type": "string", "enum": ["columns"]})
        );
        assert_eq!(
            envelope["properties"]["parts"]["items"]["properties"]["name"],
            json!({"type": "string", "enum": ["columns"]})
        );
    }

    #[test]
    fn exact_media_schema_is_enforced_over_a_permissive_wildcard() {
        harnx_core::require_nextest();
        // A wildcard `{}` used to union with the strict schema as
        // `anyOf: [strict, {}]`, which accepted anything for any selection.
        // The envelope must tie each selection to its own schema instead.
        let strict = json!({
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "required": ["name"],
            "additionalProperties": false
        });
        let branches = vec![
            branch("application/json", strict.clone()),
            branch("*/*", json!({})),
        ];
        let BodyInput::Envelope(envelope) = build_body_input(&branches, &Map::new()).unwrap()
        else {
            panic!("expected envelope");
        };
        assert!(envelope["properties"]
            .get("value")
            .is_some_and(Value::is_object));
        let constraints = envelope["allOf"]
            .as_array()
            .expect("precedence constraints");
        assert!(
            constraints
                .iter()
                .any(|constraint| constraint["if"]["properties"]["content_type"]
                    == json!({"const": "application/json"})
                    && constraint["then"]["properties"]["value"] == strict),
            "{constraints:?}"
        );
        // The published envelope rejects a strict-schema violation when the
        // exact entry is selected, and accepts anything when the wildcard is.
        let validator = jsonschema::validator_for(&envelope).unwrap();
        let exact_valid = json!({
            "content_type": "application/json", "encoding": "json",
            "value": {"name": "x"}
        });
        validator.validate(&exact_valid).unwrap();
        let exact_invalid = json!({
            "content_type": "application/json", "encoding": "json",
            "value": {"wrong": true}
        });
        assert!(validator.validate(&exact_invalid).is_err());
        let wildcard_valid = json!({
            "content_type": "text/csv", "encoding": "json",
            "value": {"anything": 1}
        });
        validator.validate(&wildcard_valid).unwrap();
    }

    #[test]
    fn identical_exact_and_wildcard_sources_stay_a_single_schema() {
        harnx_core::require_nextest();
        let schema = json!({
            "type": "object",
            "properties": {"columns": {"type": "array", "items": {"type": "string"}}},
            "required": ["columns"]
        });
        let branches = vec![
            branch("application/json", schema.clone()),
            branch("*/*", schema),
        ];
        let BodyInput::Envelope(envelope) = build_body_input(&branches, &Map::new()).unwrap()
        else {
            panic!("expected envelope");
        };
        assert_eq!(
            envelope["properties"]["value"]["required"],
            json!(["columns"])
        );
        assert!(envelope.get("allOf").is_none());
    }

    #[test]
    fn binary_sources_direct_or_through_refs_offer_no_json_value() {
        harnx_core::require_nextest();
        let branches = vec![branch("*/*", json!({"type": "string", "format": "binary"}))];
        let BodyInput::Envelope(envelope) = build_body_input(&branches, &Map::new()).unwrap()
        else {
            panic!("expected envelope");
        };
        assert!(envelope["properties"].get("value").is_none());
        assert!(envelope["properties"]["base64"].is_object());

        // The same conclusion when the binary format hides behind a local $ref.
        let defs = Map::from_iter([(
            "Bytes".to_string(),
            json!({"type": "string", "format": "binary"}),
        )]);
        let branches = vec![branch("*/*", json!({"$ref": "#/$defs/Bytes"}))];
        let BodyInput::Envelope(envelope) = build_body_input(&branches, &defs).unwrap() else {
            panic!("expected envelope");
        };
        assert!(envelope["properties"].get("value").is_none());
        assert!(envelope["properties"]["base64"].is_object());
    }

    #[test]
    fn empty_branches_are_rejected() {
        harnx_core::require_nextest();
        let error = build_body_input(&[], &Map::new()).unwrap_err();
        assert!(error.to_string().contains("media"), "{error}");
    }
}
