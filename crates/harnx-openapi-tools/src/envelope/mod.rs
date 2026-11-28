//! Request bodies are direct values for a single JSON-family declaration.
//! All other bodies select a concrete, lowercase `content_type` (without
//! parameters) and its `encoding`: `json`, `text`, `form`, `multipart` or
//! `base64`. The corresponding required payload is `value`, `text`, `fields`,
//! `parts` or `base64`. An envelope may not carry a second payload.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::error::{SpecError, SpecResult};
use crate::media::{
    encoding_domains, excluding, json_patch_array_schema, MediaBranch, MediaKind, MediaMatch,
};

mod payload;

use payload::{fields_schema, parts_schema, text_schema};

/// How an operation's request body appears in the tool input schema.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyInput {
    /// `body` is the direct JSON value (single JSON-family media entry).
    Direct(Value),
    /// `body` is the discriminated envelope described in the module docs.
    Envelope(Value),
}

/// Build the public schema from the same branch keys and encoder rules used to
/// select a branch.
pub(crate) fn build_body_input(
    branches: &[MediaBranch],
    defs: &Map<String, Value>,
    context: &str,
) -> SpecResult<BodyInput> {
    if let [branch] = branches {
        if let Some(direct) = direct_json_body(branch) {
            return Ok(BodyInput::Direct(direct));
        }
    }
    let variants = envelope_variants(branches, defs, context)?;
    Ok(BodyInput::Envelope(envelope_schema(variants)))
}

fn direct_json_body(branch: &MediaBranch) -> Option<Value> {
    match branch.kind {
        MediaKind::Json => Some(branch.source_schema.clone()),
        MediaKind::JsonPatch => Some(json_patch_array_schema()),
        _ => None,
    }
}

fn envelope_variants(
    branches: &[MediaBranch],
    defs: &Map<String, Value>,
    context: &str,
) -> SpecResult<Vec<Value>> {
    let mut variants = Vec::new();
    for branch in branches {
        for (kind, domain) in branch_domains(branch, branches) {
            variants.push(envelope_variant(branch, kind, domain, defs, context)?);
        }
    }
    if variants.is_empty() {
        return Err(SpecError::invalid(
            context,
            "advertised request-body media entries have no supported encoder",
        ));
    }
    Ok(variants)
}

fn envelope_schema(mut variants: Vec<Value>) -> Value {
    let first = variants.remove(0);
    if variants.is_empty() {
        first
    } else {
        json!({"anyOf": std::iter::once(first).chain(variants).collect::<Vec<_>>()})
    }
}

fn branch_domains(branch: &MediaBranch, branches: &[MediaBranch]) -> Vec<(MediaKind, Value)> {
    let range = branch.matcher();
    if let MediaMatch::Exact(media) = range {
        return vec![(branch.kind, json!({"enum": [media]}))];
    }
    let shadowed: Vec<_> = branches
        .iter()
        .map(MediaBranch::matcher)
        .filter(|candidate| candidate.shadows(range))
        .collect();
    let excluded: Vec<_> = shadowed
        .iter()
        .map(|candidate| candidate.schema())
        .collect();
    let selected = excluding(range.schema(), &excluded);
    encoding_domains(range, &shadowed)
        .filter(|(kind, _)| branch.allows_kind(*kind))
        .map(|(kind, domain)| (kind, intersect(selected.clone(), domain)))
        .collect()
}

fn intersect(range: Value, encoder: Value) -> Value {
    if encoder == Value::Bool(true) || range == encoder {
        range
    } else {
        json!({"allOf": [range, encoder]})
    }
}

fn envelope_variant(
    branch: &MediaBranch,
    kind: MediaKind,
    mut domain: Value,
    defs: &Map<String, Value>,
    context: &str,
) -> SpecResult<Value> {
    let (encoding, name, payload) = payload_schema(kind, &branch.source_schema, defs, context)?;
    domain["type"] = json!("string");
    Ok(json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "content_type": domain,
            "encoding": {"type": "string", "enum": [encoding]},
            (name): payload
        },
        "required": ["content_type", "encoding", name]
    }))
}

fn payload_schema(
    kind: MediaKind,
    source: &Value,
    defs: &Map<String, Value>,
    context: &str,
) -> SpecResult<(&'static str, &'static str, Value)> {
    match kind {
        MediaKind::Json => Ok(("json", "value", source.clone())),
        MediaKind::JsonPatch => Ok(("json", "value", json_patch_array_schema())),
        MediaKind::Text | MediaKind::UrlencodedForm | MediaKind::Multipart => {
            named_payload_schema(kind, source, defs)
        }
        MediaKind::Base64 => Ok((
            "base64",
            "base64",
            json!({"type": "string", "description": "Base64-encoded request bytes."}),
        )),
        MediaKind::Wildcard => Err(SpecError::invalid(
            context,
            "media range has no concrete payload encoder",
        )),
    }
}

fn named_payload_schema(
    kind: MediaKind,
    source: &Value,
    defs: &Map<String, Value>,
) -> SpecResult<(&'static str, &'static str, Value)> {
    match kind {
        MediaKind::Text => Ok(("text", "text", text_schema(source, defs))),
        MediaKind::UrlencodedForm => Ok(("form", "fields", fields_schema(source, defs))),
        MediaKind::Multipart => Ok(("multipart", "parts", parts_schema(source, defs))),
        _ => Err(SpecError::invalid(
            "request body",
            "media kind does not use a named payload encoder",
        )),
    }
}
