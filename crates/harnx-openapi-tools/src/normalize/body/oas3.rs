//! OpenAPI 3.0 request-body resolution and media-branch construction.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use crate::compact::SchemaCompactor;
use crate::document::SpecDocument;
use crate::envelope::BodyInput;
use crate::error::{SpecError, SpecResult};
use crate::media::MediaBranch;

use super::{check_media_key, BodySpec};

pub(super) fn build_body(
    document: &SpecDocument,
    operation: &Map<String, Value>,
    context: &str,
    compactor: &mut SchemaCompactor<'_>,
) -> SpecResult<Option<BodySpec>> {
    let Some(raw_body) = operation.get("requestBody") else {
        return Ok(None);
    };
    let request_body = resolve_request_body(document, raw_body, context)?;
    let required = request_body.get("required") == Some(&Value::Bool(true));
    let branches = build_branches(&request_body, context, compactor)?;
    Ok(Some(BodySpec {
        required,
        branches,
        input: BodyInput::Direct(Value::Null),
        form_fields: Vec::new(),
    }))
}

fn resolve_request_body(
    document: &SpecDocument,
    request_body: &Value,
    context: &str,
) -> SpecResult<Value> {
    let Some(reference) = request_body.get("$ref").and_then(Value::as_str) else {
        return Ok(request_body.clone());
    };
    crate::refs::resolve_pointer(&document.root, reference, context).cloned()
}

fn build_branches<'a>(
    request_body: &'a Value,
    context: &str,
    compactor: &mut SchemaCompactor<'_>,
) -> SpecResult<Vec<MediaBranch>> {
    let content = request_content(request_body, context)?;
    let mut advertised: BTreeMap<String, &'a str> = BTreeMap::new();
    content
        .iter()
        .map(|(media_type, entry)| {
            build_branch(media_type, entry, context, compactor, &mut advertised)
        })
        .collect()
}

fn request_content<'a>(
    request_body: &'a Value,
    context: &str,
) -> SpecResult<&'a Map<String, Value>> {
    let Some(content) = request_body.get("content").and_then(Value::as_object) else {
        return Err(SpecError::invalid(
            context,
            "request body without a `content` map",
        ));
    };
    if content.is_empty() {
        return Err(SpecError::invalid(
            context,
            "request body advertises no media entries",
        ));
    }
    Ok(content)
}

fn build_branch<'a>(
    media_type: &'a str,
    entry: &Value,
    context: &str,
    compactor: &mut SchemaCompactor<'_>,
    advertised: &mut BTreeMap<String, &'a str>,
) -> SpecResult<MediaBranch> {
    let schema = compactor.compact(entry.get("schema").unwrap_or(&json!({})))?;
    let encoding = entry
        .get("encoding")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let branch = MediaBranch::new(media_type, schema, encoding, context)?;
    check_media_key(advertised, media_type, &branch, context)?;
    Ok(branch)
}
