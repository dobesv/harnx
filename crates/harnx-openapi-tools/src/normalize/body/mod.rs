//! Request-body construction for both spec kinds.

mod oas3;
mod swagger2;

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::compact::SchemaCompactor;
use crate::document::{SpecDocument, SpecKind};
use crate::envelope::{build_body_input, BodyInput};
use crate::error::{SpecError, SpecResult};
use crate::media::MediaBranch;
use crate::normalize::params::MergedParam;

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
    pub fn resolve_input(&mut self, defs: &Map<String, Value>, context: &str) -> SpecResult<()> {
        if self.branches.is_empty() {
            return Err(SpecError::invalid(
                "operation",
                "request body advertises no media entries",
            ));
        }
        for branch in &mut self.branches {
            branch.resolve_payload_kinds(defs);
        }
        self.input = build_body_input(&self.branches, defs, context)?;
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
        SpecKind::OpenApi3 => oas3::build_body(document, operation, context, compactor),
        SpecKind::Swagger2 => swagger2::build_body(document, merged, operation, context, compactor),
    }
}

pub(super) fn check_media_key<'a>(
    advertised: &mut BTreeMap<String, &'a str>,
    declaration: &'a str,
    branch: &MediaBranch,
    context: &str,
) -> SpecResult<()> {
    if let Some(previous) = advertised.insert(branch.media_type.clone(), declaration) {
        return Err(SpecError::invalid(
            context,
            format!(
            "ambiguous media entries {previous:?} and {declaration:?}: both canonicalize to {:?}",
            branch.media_type
        ),
        ));
    }
    Ok(())
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
