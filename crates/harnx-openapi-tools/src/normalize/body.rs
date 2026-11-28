//! Request-body construction for both spec kinds.

use serde_json::{json, Map, Value};

use crate::compact::SchemaCompactor;
use crate::document::{SpecDocument, SpecKind};
use crate::envelope::{build_body_input, BodyInput};
use crate::error::{SpecError, SpecResult};
use crate::media::{classify_media, MediaBranch, MediaKind};
use crate::normalize::params::{
    json_consumes, parameter_style, swagger2_param_schema, MergedParam,
};

/// A Swagger 2.0 `formData` field, kept for dispatch.
#[derive(Clone, Debug, PartialEq)]
pub struct FormField {
    pub name: String,
    pub required: bool,
    pub collection_format: crate::normalize::params::ParamStyle,
}

/// A normalized request body: its media branches and how it appears in the
/// tool input schema.
#[derive(Clone, Debug)]
pub struct BodySpec {
    pub required: bool,
    pub branches: Vec<MediaBranch>,
    /// Filled in by the caller once all definitions are collected.
    pub input: BodyInput,
    pub form_fields: Vec<FormField>,
}

impl BodySpec {
    /// Resolve the published body input from the collected branches.
    pub fn resolve_input(&mut self, defs: &Map<String, Value>) -> SpecResult<()> {
        self.input = build_body_input(&self.branches, defs)?;
        Ok(())
    }
}

/// Build the body spec for an operation. Returns `None` for bodyless requests.
///
/// The merged parameter list is passed in so that Swagger 2.0 `body` and
/// `formData` parameters are honored wherever they are declared: on the
/// operation, on the path item, or through a resolved `$ref`.
pub fn build_body(
    document: &SpecDocument,
    merged: &[MergedParam],
    operation: &Map<String, Value>,
    context: &str,
    compactor: &mut SchemaCompactor<'_>,
) -> SpecResult<Option<BodySpec>> {
    match document.kind {
        SpecKind::OpenApi3 => build_oas3_body(document, operation, context, compactor),
        SpecKind::Swagger2 => build_swagger2_body(document, merged, operation, context, compactor),
    }
}

fn build_oas3_body(
    document: &SpecDocument,
    operation: &Map<String, Value>,
    context: &str,
    compactor: &mut SchemaCompactor<'_>,
) -> SpecResult<Option<BodySpec>> {
    let Some(request_body) = operation.get("requestBody") else {
        return Ok(None);
    };
    let request_body = if request_body.get("$ref").is_some() {
        let reference = request_body
            .get("$ref")
            .and_then(Value::as_str)
            .unwrap_or("");
        crate::refs::resolve_pointer(&document.root, reference, context)?.clone()
    } else {
        request_body.clone()
    };
    let required = request_body.get("required") == Some(&Value::Bool(true));
    let Some(content) = request_body.get("content").and_then(Value::as_object) else {
        return Err(SpecError::invalid(
            context,
            "request body without a `content` map",
        ));
    };
    let mut branches = Vec::new();
    for (media_type, entry) in content {
        let schema = compactor.compact(entry.get("schema").unwrap_or(&json!({})))?;
        let encoding = entry
            .get("encoding")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        branches.push(MediaBranch::new(media_type, schema, encoding));
    }
    Ok(Some(BodySpec {
        required,
        branches,
        input: BodyInput::Direct(Value::Null),
        form_fields: Vec::new(),
    }))
}

/// Swagger 2.0 bodies: one `in: body` parameter (a JSON value sent with the
/// JSON-family `consumes` entry, when one is declared) or `formData`
/// parameters (multipart when a `type: file` field or a multipart
/// `consumes` entry demands it, URL-encoded otherwise).
fn build_swagger2_body(
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
    if let Some(body_param) = body_param {
        let schema = body_param
            .param
            .get("schema")
            .ok_or_else(|| SpecError::invalid(context, "body parameter without a schema"))?;
        let compacted = compactor.compact(schema)?;
        // The wire media type comes from `consumes`; a JSON-family entry
        // carries the value directly, anything else falls through to the
        // envelope's base64 encoder.
        let media = json_consumes(&consumes)
            .or_else(|| consumes.first().map(String::as_str))
            .unwrap_or("application/json");
        return Ok(Some(BodySpec {
            required: body_param.required,
            branches: vec![MediaBranch::new(media, compacted, Map::new())],
            input: BodyInput::Direct(Value::Null),
            form_fields: Vec::new(),
        }));
    }
    if form_params.is_empty() {
        return Ok(None);
    }
    build_form_data_body(&form_params, &consumes, context, compactor)
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

/// Build the body for `formData` parameters: multipart when a `type: file`
/// field or an advertised multipart `consumes` entry demands it, URL-encoded
/// fields otherwise. Swagger's `type: "file"` is not a JSON Schema type, so
/// the published schema carries binary strings instead (see
/// [`swagger2_param_schema`]).
fn build_form_data_body(
    form_params: &[&MergedParam],
    consumes: &[String],
    context: &str,
    compactor: &mut SchemaCompactor<'_>,
) -> SpecResult<Option<BodySpec>> {
    let has_file = form_params
        .iter()
        .any(|param| param.param.get("type") == Some(&json!("file")));
    let media = if has_file
        || consumes
            .iter()
            .any(|media| classify_media(media) == MediaKind::Multipart)
    {
        "multipart/form-data"
    } else {
        consumes
            .iter()
            .find(|media| classify_media(media) == MediaKind::UrlencodedForm)
            .map(String::as_str)
            .unwrap_or("application/x-www-form-urlencoded")
    };
    let mut form_fields = Vec::new();
    let mut properties = Map::new();
    let mut required = Vec::new();
    for param in form_params {
        let schema = swagger2_param_schema(&param.param);
        if param.required {
            required.push(json!(param.name));
        }
        let collection_format = parameter_style(SpecKind::Swagger2, param, context)?;
        form_fields.push(FormField {
            name: param.name.clone(),
            required: param.required,
            collection_format,
        });
        properties.insert(param.name.clone(), schema);
    }
    let mut source = Map::new();
    source.insert("type".to_string(), json!("object"));
    source.insert("properties".to_string(), Value::Object(properties));
    if !required.is_empty() {
        source.insert("required".to_string(), Value::Array(required));
    }
    let compacted = compactor.compact(&Value::Object(source))?;
    Ok(Some(BodySpec {
        required: form_fields.iter().any(|field| field.required),
        branches: vec![MediaBranch::new(media, compacted, Map::new())],
        input: BodyInput::Direct(Value::Null),
        form_fields,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{SpecDocument, SpecKind};
    use crate::normalize::params::ParamStyle;
    use serde_json::json;

    fn swagger_document(root: Value) -> SpecDocument {
        SpecDocument {
            kind: SpecKind::Swagger2,
            version: "2.0".to_string(),
            root,
        }
    }

    fn merged_param(location: &str, param: Value) -> MergedParam {
        let name = param["name"].as_str().unwrap_or_default().to_string();
        let required = param.get("required") == Some(&Value::Bool(true));
        MergedParam {
            location: location.to_string(),
            name,
            required,
            param,
        }
    }

    fn merged_list(location: &str, params: Vec<Value>) -> Vec<MergedParam> {
        params
            .into_iter()
            .map(|param| merged_param(location, param))
            .collect()
    }

    fn empty_operation() -> Map<String, Value> {
        Map::new()
    }

    fn build(document: &SpecDocument, merged: &[MergedParam]) -> Option<BodySpec> {
        let mut compactor = SchemaCompactor::new(&document.root, "POST /upload");
        build_body(
            document,
            merged,
            &empty_operation(),
            "POST /upload",
            &mut compactor,
        )
        .unwrap()
    }

    #[test]
    fn swagger2_form_params_become_urlencoded_branch_with_field_names() {
        harnx_core::require_nextest();
        let merged = merged_list(
            "formData",
            vec![
                json!({"name": "tags", "in": "formData", "type": "array", "items": {"type": "string"}, "collectionFormat": "multi"}),
            ],
        );
        let body = build(&swagger_document(json!({})), &merged).unwrap();
        assert!(!body.required);
        assert_eq!(
            body.form_fields
                .iter()
                .map(|field| (field.name.as_str(), field.required))
                .collect::<Vec<_>>(),
            vec![("tags", false)]
        );
        assert_eq!(
            body.form_fields[0].collection_format,
            ParamStyle::Form { explode: true }
        );
        assert_eq!(body.branches.len(), 1);
        assert_eq!(
            body.branches[0].media_type,
            "application/x-www-form-urlencoded"
        );
        assert!(body.branches[0].source_schema["required"].is_null());
    }

    #[test]
    fn swagger2_file_formdata_selects_multipart_and_binary_strings() {
        harnx_core::require_nextest();
        let merged = merged_list(
            "formData",
            vec![
                json!({"name": "file", "in": "formData", "type": "file", "required": true}),
                json!({"name": "note", "in": "formData", "type": "string"}),
            ],
        );
        let body = build(&swagger_document(json!({})), &merged).unwrap();
        // Multipart is required: a URL-encoded branch cannot carry a file.
        assert_eq!(body.branches[0].media_type, "multipart/form-data");
        assert!(body.required);
        // The published schema carries binary strings, never `type: "file"`.
        assert_eq!(
            body.branches[0].source_schema["properties"]["file"],
            json!({"type": "string", "format": "binary"})
        );
        assert_eq!(body.branches[0].source_schema["required"], json!(["file"]));
        assert_eq!(
            body.form_fields
                .iter()
                .map(|field| (field.name.as_str(), field.required))
                .collect::<Vec<_>>(),
            vec![("file", true), ("note", false)]
        );
    }

    #[test]
    fn swagger2_multipart_consumes_selects_multipart_without_file_fields() {
        harnx_core::require_nextest();
        let merged = merged_list(
            "formData",
            vec![json!({"name": "note", "in": "formData", "type": "string"})],
        );
        let document = swagger_document(json!({
            "consumes": ["multipart/form-data"]
        }));
        let body = build(&document, &merged).unwrap();
        assert_eq!(body.branches[0].media_type, "multipart/form-data");
    }

    #[test]
    fn swagger2_body_param_becomes_json_branch_and_honors_consumes() {
        harnx_core::require_nextest();
        let merged = vec![merged_param(
            "body",
            json!({"name": "body", "in": "body", "required": true,
                    "schema": {"type": "object", "properties": {"name": {"type": "string"}}}}),
        )];
        // No consumes declared: JSON is the default.
        let body = build(&swagger_document(json!({})), &merged).unwrap();
        assert!(body.required);
        assert_eq!(body.branches[0].media_type, "application/json");
        assert_eq!(
            body.branches[0].source_schema["properties"]["name"],
            json!({"type": "string"})
        );

        // A declared JSON-family consumes entry names the wire media type.
        let document = swagger_document(json!({
            "consumes": ["application/vnd.custom+json"]
        }));
        let body = build(&document, &merged).unwrap();
        assert_eq!(body.branches[0].media_type, "application/vnd.custom+json");
    }

    #[test]
    fn swagger2_body_and_formdata_cannot_coexist() {
        harnx_core::require_nextest();
        let mut merged = merged_list(
            "formData",
            vec![json!({"name": "note", "in": "formData", "type": "string"})],
        );
        merged.push(merged_param(
            "body",
            json!({"name": "payload", "in": "body", "schema": {"type": "string"}}),
        ));
        let empty_defs = json!({});
        let mut compactor = SchemaCompactor::new(&empty_defs, "POST /x");
        let error = build_body(
            &swagger_document(json!({})),
            &merged,
            &empty_operation(),
            "POST /x",
            &mut compactor,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("both a `body` and `formData`"),
            "{error}"
        );
    }
}
