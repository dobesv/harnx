//! Swagger 2.0 body and `formData` normalization.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use crate::compact::SchemaCompactor;
use crate::document::{SpecDocument, SpecKind};
use crate::envelope::BodyInput;
use crate::error::{SpecError, SpecResult};
use crate::media::{MediaBranch, MediaKind};
use crate::normalize::params::{parameter_style, swagger2_param_schema, MergedParam};

use super::{check_media_key, BodySpec, FormField};

/// Swagger 2.0 has one `in: body` parameter or a set of `formData` fields.
/// The effective `consumes` list supplies the media alternatives for either.
pub(super) fn build_body(
    document: &SpecDocument,
    merged: &[MergedParam],
    operation: &Map<String, Value>,
    context: &str,
    compactor: &mut SchemaCompactor<'_>,
) -> SpecResult<Option<BodySpec>> {
    let body_param = merged.iter().find(|param| param.location == "body");
    let form_params: Vec<&MergedParam> = merged
        .iter()
        .filter(|param| param.location == "formData")
        .collect();
    if body_param.is_some() && !form_params.is_empty() {
        return Err(SpecError::invalid(
            context,
            "an operation cannot declare both a `body` and `formData` parameters",
        ));
    }
    let consumes = consumes_list(document, operation);
    build_declared_body(body_param, &form_params, &consumes, context, compactor)
}

fn build_declared_body(
    body_param: Option<&MergedParam>,
    form_params: &[&MergedParam],
    consumes: &[String],
    context: &str,
    compactor: &mut SchemaCompactor<'_>,
) -> SpecResult<Option<BodySpec>> {
    if let Some(body_param) = body_param {
        return build_json_body(body_param, consumes, context, compactor).map(Some);
    }
    if form_params.is_empty() {
        return Ok(None);
    }
    build_form_data_body(form_params, consumes, context, compactor)
}

fn build_json_body(
    body_param: &MergedParam,
    consumes: &[String],
    context: &str,
    compactor: &mut SchemaCompactor<'_>,
) -> SpecResult<BodySpec> {
    let schema = body_param
        .param
        .get("schema")
        .ok_or_else(|| SpecError::invalid(context, "body parameter without a schema"))?;
    let compacted = compactor.compact(schema)?;
    let branches = swagger_branches(consumes, "application/json", &compacted, context)?;
    Ok(BodySpec {
        required: body_param.required,
        branches,
        input: BodyInput::Direct(Value::Null),
        form_fields: Vec::new(),
    })
}

/// The effective `consumes` list: an operation-level list overrides the
/// global one, per the Swagger 2.0 rules.
fn consumes_list(document: &SpecDocument, operation: &Map<String, Value>) -> Vec<String> {
    let source = operation
        .get("consumes")
        .or_else(|| document.root.get("consumes"));
    source
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn swagger_branches(
    consumes: &[String],
    default: &str,
    source: &Value,
    context: &str,
) -> SpecResult<Vec<MediaBranch>> {
    let mut advertised = BTreeMap::new();
    let mut declarations: Vec<_> = consumes.iter().map(String::as_str).collect();
    if declarations.is_empty() {
        declarations.push(default);
    }
    declarations
        .into_iter()
        .map(|media| {
            let branch = MediaBranch::new(media, source.clone(), Map::new(), context)?;
            check_media_key(&mut advertised, media, &branch, context)?;
            Ok(branch)
        })
        .collect()
}

/// Build the ordered field/part representation of Swagger `formData`.
/// Explicit alternatives must all support this representation. In particular,
/// URL-encoded fields cannot carry a Swagger field of `type: file`.
fn build_form_data_body(
    form_params: &[&MergedParam],
    consumes: &[String],
    context: &str,
    compactor: &mut SchemaCompactor<'_>,
) -> SpecResult<Option<BodySpec>> {
    let has_file = form_params
        .iter()
        .any(|param| param.param.get("type") == Some(&json!("file")));
    let default = default_form_media(has_file);
    let (form_fields, source) = form_data_schema(form_params, context, compactor)?;
    let branches = swagger_branches(consumes, default, &source, context)?;
    validate_form_media(&branches, has_file, context)?;
    let required = form_fields.iter().any(|field| field.required);
    Ok(Some(BodySpec {
        required,
        branches,
        input: BodyInput::Direct(Value::Null),
        form_fields,
    }))
}

fn default_form_media(has_file: bool) -> &'static str {
    if has_file {
        "multipart/form-data"
    } else {
        "application/x-www-form-urlencoded"
    }
}

fn form_data_schema(
    form_params: &[&MergedParam],
    context: &str,
    compactor: &mut SchemaCompactor<'_>,
) -> SpecResult<(Vec<FormField>, Value)> {
    let mut form_fields = Vec::new();
    let mut properties = Map::new();
    let mut required = Vec::new();
    for param in form_params {
        properties.insert(param.name.clone(), swagger2_param_schema(&param.param));
        if param.required {
            required.push(json!(param.name));
        }
        form_fields.push(FormField {
            name: param.name.clone(),
            required: param.required,
            collection_format: parameter_style(SpecKind::Swagger2, param, context)?,
        });
    }
    let source = form_data_object(properties, required);
    compactor
        .compact(&source)
        .map(|compacted| (form_fields, compacted))
}

fn form_data_object(properties: Map<String, Value>, required: Vec<Value>) -> Value {
    let mut source = Map::new();
    source.insert("type".to_string(), json!("object"));
    source.insert("properties".to_string(), Value::Object(properties));
    if !required.is_empty() {
        source.insert("required".to_string(), Value::Array(required));
    }
    Value::Object(source)
}

fn validate_form_media(branches: &[MediaBranch], has_file: bool, context: &str) -> SpecResult<()> {
    for branch in branches {
        let compatible = branch.kind == MediaKind::Multipart
            || (!has_file && branch.kind == MediaKind::UrlencodedForm);
        if !compatible {
            return Err(incompatible_form_media(branch, has_file, context));
        }
    }
    Ok(())
}

fn incompatible_form_media(branch: &MediaBranch, has_file: bool, context: &str) -> SpecError {
    SpecError::unsupported(
        context,
        format!(
            "Swagger formData cannot be encoded as {:?}{}; declare compatible concrete form media types",
            branch.media_type,
            if has_file { " with file fields" } else { "" },
        ),
    )
}
