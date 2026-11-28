//! Parameter merge, classification and serialization styles.

use serde_json::{json, Map, Value};

use crate::compact::SchemaCompactor;
use crate::document::{SpecDocument, SpecKind};
use crate::error::{SpecError, SpecResult};
use crate::media::MediaKind;
use crate::security::{ApiKeyPlacement, ProtectedParam, SecuritySchemes};

/// The parameter serialization styles this release can encode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamStyle {
    /// `form` (query/header default) with its explode flag. Swagger 2.0
    /// `collectionFormat: csv` maps to explode=false, `multi` to explode=true.
    Form { explode: bool },
    /// `simple` (path default).
    Simple,
}

/// A wire parameter, grouped by location and ready for dispatch.
#[derive(Clone, Debug, PartialEq)]
pub struct ParamSpec {
    pub name: String,
    pub required: bool,
    pub style: ParamStyle,
    /// Compacted schema; shared definitions live in the operation's `$defs`.
    pub schema: Value,
}

/// A resolved parameter (local `$ref`s followed), before classification.
#[derive(Clone, Debug)]
pub struct MergedParam {
    pub location: String,
    pub name: String,
    pub required: bool,
    pub param: Value,
}

/// The classified parameter groups of one operation.
#[derive(Default)]
pub struct ParamGroups {
    pub path: Vec<ParamSpec>,
    pub query: Vec<ParamSpec>,
    pub headers: Vec<ParamSpec>,
    pub protected: Vec<ProtectedParam>,
}

/// Follow a parameter's local `$ref` (OAS3 `components/parameters`, Swagger
/// 2.0 top-level `parameters`) and return the concrete parameter object.
pub fn resolve_parameter(
    document: &SpecDocument,
    parameter: &Value,
    context: &str,
) -> SpecResult<Value> {
    let reference = match parameter.get("$ref").and_then(Value::as_str) {
        Some(reference) => reference,
        None => return Ok(parameter.clone()),
    };
    let resolved = crate::refs::resolve_pointer(&document.root, reference, context)?;
    if !resolved.get("name").is_some_and(Value::is_string) {
        return Err(SpecError::invalid(
            context,
            format!("parameter reference {reference} does not name a parameter object"),
        ));
    }
    Ok(resolved.clone())
}

/// Whether `candidate` and `existing` describe the same wire parameter.
///
/// Header names are case-insensitive on the wire, so a header override must
/// match regardless of casing; other locations compare exactly.
fn same_param(existing: &MergedParam, candidate: &MergedParam) -> bool {
    if existing.location != candidate.location {
        return false;
    }
    if existing.location == "header" {
        existing.name.eq_ignore_ascii_case(&candidate.name)
    } else {
        existing.name == candidate.name
    }
}

/// Merge path-level and operation-level parameters. Operation entries override
/// path entries with the same `(location, name)` pair, per both formats' rules.
pub fn merge_parameters(
    document: &SpecDocument,
    path_item: &Map<String, Value>,
    operation: &Map<String, Value>,
    context: &str,
) -> SpecResult<Vec<MergedParam>> {
    let raw_lists = [path_item.get("parameters"), operation.get("parameters")];
    let mut merged: Vec<MergedParam> = Vec::new();
    for raw_list in raw_lists.into_iter().flatten() {
        let Value::Array(parameters) = raw_list else {
            return Err(SpecError::invalid(context, "`parameters` must be an array"));
        };
        for raw in parameters {
            let candidate = merged_param(document, raw, context)?;
            match merged
                .iter_mut()
                .find(|existing| same_param(existing, &candidate))
            {
                // The operation-level entry wins; its casing is used.
                Some(existing) => *existing = candidate,
                None => merged.push(candidate),
            }
        }
    }
    Ok(merged)
}

fn merged_param(document: &SpecDocument, raw: &Value, context: &str) -> SpecResult<MergedParam> {
    let param = resolve_parameter(document, raw, context)?;
    let location = param
        .get("in")
        .and_then(Value::as_str)
        .ok_or_else(|| SpecError::invalid(context, "parameter without an `in` field"))?
        .to_string();
    let name = param
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| SpecError::invalid(context, "parameter without a `name` field"))?
        .to_string();
    let required = param.get("required") == Some(&Value::Bool(true));
    Ok(MergedParam {
        location,
        name,
        required,
        param,
    })
}

/// Split merged parameters into groups, dropping spec-identified credential
/// parameters into `protected` and rejecting placements and styles this
/// release cannot encode.
///
/// Credential matching checks the parameter's location: only a header
/// parameter named by an `apiKey`-in-header scheme, a query parameter named by
/// an `apiKey`-in-query scheme, or a cookie parameter named by an
/// `apiKey`-in-cookie scheme is protected. An ordinary `path`/`query`/`header`
/// parameter that merely shares a credential's name stays a model argument.
pub fn classify_parameters(
    document: &SpecDocument,
    params: Vec<MergedParam>,
    schemes: &SecuritySchemes,
    context: &str,
    compactor: &mut SchemaCompactor<'_>,
) -> SpecResult<ParamGroups> {
    let mut groups = ParamGroups::default();
    for param in params {
        // Swagger 2.0 body/formData parameters belong to the body builder,
        // which publishes them as the `body` group; they are never model
        // arguments.
        if matches!(param.location.as_str(), "body" | "formData") {
            continue;
        }
        if let Some((scheme, placement)) = credential_scheme(schemes, &param) {
            groups.protected.push(ProtectedParam {
                name: param.name.clone(),
                scheme: scheme.clone(),
                placement,
            });
            continue;
        }
        let spec = param_spec(document, &param, compactor, context)?;
        match param.location.as_str() {
            "path" => {
                // The wire requires every path parameter; a `required: false`
                // declaration cannot make it optional.
                let mut spec = spec;
                spec.required = true;
                groups.path.push(spec);
            }
            "query" => groups.query.push(spec),
            "header" => groups.headers.push(spec),
            "cookie" => {
                return Err(SpecError::unsupported(
                    context,
                    format!(
                        "cookie parameter {:?} cannot be sent: this release supports no cookie \
                         injection",
                        param.name
                    ),
                ));
            }
            other => {
                return Err(SpecError::unsupported(
                    context,
                    format!(
                        "parameter {:?} has unsupported location {other:?}",
                        param.name
                    ),
                ));
            }
        }
    }
    Ok(groups)
}

/// The security scheme a parameter is a credential for, at its own location.
fn credential_scheme<'a>(
    schemes: &'a SecuritySchemes,
    param: &MergedParam,
) -> Option<(&'a String, ApiKeyPlacement)> {
    match param.location.as_str() {
        "header" => schemes
            .header_scheme(&param.name)
            .map(|scheme| (scheme, ApiKeyPlacement::Header)),
        "query" => schemes
            .query_scheme(&param.name)
            .map(|scheme| (scheme, ApiKeyPlacement::Query)),
        "cookie" => schemes
            .cookie_scheme(&param.name)
            .map(|scheme| (scheme, ApiKeyPlacement::Cookie)),
        _ => None,
    }
}

/// Build one classified parameter: compacted schema plus serialization style.
fn param_spec(
    document: &SpecDocument,
    param: &MergedParam,
    compactor: &mut SchemaCompactor<'_>,
    context: &str,
) -> SpecResult<ParamSpec> {
    let schema_source = param_schema_source(document, param, context)?;
    Ok(ParamSpec {
        name: param.name.clone(),
        required: param.required,
        style: parameter_style(document.kind, param, context)?,
        schema: compactor.compact(&schema_source)?,
    })
}

/// The JSON schema of a parameter. OAS3 parameters carry `schema` (or an
/// unsupported `content` map); Swagger 2.0 parameters carry the type keywords
/// inline, which are reshaped into a schema.
fn param_schema_source(
    document: &SpecDocument,
    param: &MergedParam,
    context: &str,
) -> SpecResult<Value> {
    match document.kind {
        SpecKind::OpenApi3 => {
            if let Some(schema) = param.param.get("schema") {
                return Ok(schema.clone());
            }
            if param.param.get("content").is_some() {
                return Err(SpecError::unsupported(
                    context,
                    format!(
                        "parameter {:?} uses content-based serialization, which is not supported",
                        param.name
                    ),
                ));
            }
            // A parameter without a schema accepts anything.
            Ok(json!({}))
        }
        SpecKind::Swagger2 => {
            if param.location == "body" {
                return Err(SpecError::invalid(
                    context,
                    format!(
                        "body parameter {:?} must be handled by the body builder, not a group",
                        param.name
                    ),
                ));
            }
            Ok(swagger2_param_schema(&param.param))
        }
    }
}

/// Reshape a Swagger 2.0 non-body parameter (type keywords inline) into a
/// JSON schema. Swagger's `type: "file"` is not a JSON Schema type; it maps
/// to binary string content.
pub fn swagger2_param_schema(param: &Value) -> Value {
    let Some(source) = param.as_object() else {
        return json!({});
    };
    let mut schema = Map::new();
    for (key, value) in source {
        if matches!(
            key.as_str(),
            "name" | "in" | "description" | "required" | "collectionFormat"
        ) || key.starts_with("x-")
        {
            continue;
        }
        if key == "type" && value == &json!("file") {
            schema.insert("type".to_string(), json!("string"));
            schema.insert("format".to_string(), json!("binary"));
        } else {
            schema.insert(key.clone(), value.clone());
        }
    }
    Value::Object(schema)
}

/// The serialization style a parameter uses, with unsupported styles rejected
/// instead of silently mis-encoded.
pub fn parameter_style(
    kind: SpecKind,
    param: &MergedParam,
    context: &str,
) -> SpecResult<ParamStyle> {
    match kind {
        SpecKind::OpenApi3 => {
            let style = param.param.get("style").and_then(Value::as_str);
            let explode = param.param.get("explode");
            let is_path = param.location == "path";
            match style.unwrap_or(if is_path { "simple" } else { "form" }) {
                "form" => Ok(ParamStyle::Form {
                    explode: explode.map(Value::as_bool) != Some(Some(false)),
                }),
                "simple" => Ok(ParamStyle::Simple),
                other => Err(SpecError::unsupported(
                    context,
                    format!(
                        "parameter {:?} uses serialization style {other:?}, which is not \
                         supported (form, simple and their defaults are)",
                        param.name
                    ),
                )),
            }
        }
        SpecKind::Swagger2 => {
            let collection_format = param
                .param
                .get("collectionFormat")
                .and_then(Value::as_str)
                .unwrap_or("csv");
            match collection_format {
                "csv" => Ok(ParamStyle::Form { explode: false }),
                "multi" => Ok(ParamStyle::Form { explode: true }),
                other => Err(SpecError::unsupported(
                    context,
                    format!(
                        "parameter {:?} uses collectionFormat {other:?}, which is not supported \
                         (csv and multi are)",
                        param.name
                    ),
                )),
            }
        }
    }
}

/// Media kind of the first `consumes` entry that selects JSON-family bodies,
/// if any.
pub fn json_consumes(consumes: &[String]) -> Option<&str> {
    consumes
        .iter()
        .find(|media| matches!(crate::media::classify_media(media), MediaKind::Json))
        .map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn swagger_document() -> SpecDocument {
        SpecDocument {
            kind: SpecKind::Swagger2,
            version: "2.0".to_string(),
            root: json!({}),
        }
    }

    fn query_param(location: &str, name: &str, param: Value) -> MergedParam {
        MergedParam {
            location: location.to_string(),
            name: name.to_string(),
            required: false,
            param,
        }
    }

    #[test]
    fn swagger2_file_parameters_become_binary_strings() {
        harnx_core::require_nextest();
        let schema = swagger2_param_schema(&json!({
            "name": "file", "in": "formData", "type": "file", "required": true
        }));
        assert_eq!(schema["type"], json!("string"));
        assert_eq!(schema["format"], json!("binary"));
        assert!(schema.get("name").is_none());
        assert!(schema.get("in").is_none());
        assert!(schema.get("required").is_none());
    }

    #[test]
    fn unsupported_styles_fail_with_context() {
        harnx_core::require_nextest();
        let deep_object = query_param(
            "query",
            "filter",
            json!({"name": "filter", "in": "query", "style": "deepObject", "schema": {"type": "object"}}),
        );
        let error = parameter_style(SpecKind::OpenApi3, &deep_object, "GET /things").unwrap_err();
        assert!(error.to_string().contains("deepObject"), "{error}");
        assert!(error.to_string().contains("GET /things"), "{error}");

        let ssv = query_param(
            "query",
            "ids",
            json!({"name": "ids", "in": "query", "type": "array", "collectionFormat": "ssv"}),
        );
        let error = parameter_style(SpecKind::Swagger2, &ssv, "GET /things").unwrap_err();
        assert!(error.to_string().contains("ssv"), "{error}");

        // Defaults are accepted: OAS3 form/simple.
        let limit = query_param(
            "query",
            "limit",
            json!({"name": "limit", "in": "query", "schema": {"type": "integer"}}),
        );
        assert_eq!(
            parameter_style(SpecKind::OpenApi3, &limit, "GET /things").unwrap(),
            ParamStyle::Form { explode: true }
        );
        let id = MergedParam {
            required: true,
            ..query_param(
                "path",
                "id",
                json!({"name": "id", "in": "path", "required": true, "schema": {"type": "string"}}),
            )
        };
        assert_eq!(
            parameter_style(SpecKind::OpenApi3, &id, "GET /things/{id}").unwrap(),
            ParamStyle::Simple
        );
    }

    #[test]
    fn header_parameter_overrides_are_case_insensitive() {
        harnx_core::require_nextest();
        let document = swagger_document();
        let path_item = json!({"parameters": [
            {"name": "X-Foo", "in": "header", "type": "string"}
        ]})
        .as_object()
        .unwrap()
        .clone();
        let operation = json!({"parameters": [
            {"name": "x-foo", "in": "header", "type": "integer"}
        ]})
        .as_object()
        .unwrap()
        .clone();
        let merged = merge_parameters(&document, &path_item, &operation, "GET /x").unwrap();
        assert_eq!(merged.len(), 1);
        // The operation-level entry wins and supplies the casing.
        assert_eq!(merged[0].name, "x-foo");
        assert_eq!(merged[0].param["type"], json!("integer"));
    }

    #[test]
    fn credential_matching_requires_the_schemes_own_location() {
        harnx_core::require_nextest();
        let root = json!({
            "openapi": "3.0.1",
            "paths": {},
            "components": {"securitySchemes": {
                "tokenHeader": {"type": "apiKey", "in": "header", "name": "token"},
                "tokenQuery": {"type": "apiKey", "in": "query", "name": "token"}
            }}
        });
        let document = SpecDocument::from_value(root).unwrap();
        let schemes = SecuritySchemes::collect(&document);
        let mut compactor = SchemaCompactor::new(&document.root, "GET /x");
        // A path parameter named `token` is an ordinary model argument, not a
        // credential for the header scheme.
        let path = query_param(
            "path",
            "token",
            json!({"name": "token", "in": "path", "required": true, "schema": {"type": "string"}}),
        );
        let query = query_param(
            "query",
            "verbose",
            json!({"name": "verbose", "in": "query", "schema": {"type": "boolean"}}),
        );
        let header = query_param(
            "header",
            "token",
            json!({"name": "token", "in": "header", "schema": {"type": "string"}}),
        );
        let groups = classify_parameters(
            &document,
            vec![path, query, header],
            &schemes,
            "GET /x/{token}",
            &mut compactor,
        )
        .unwrap();
        // The header credential is protected; the path and query arguments
        // survive as model arguments.
        assert_eq!(groups.protected.len(), 1);
        assert_eq!(groups.protected[0].placement, ApiKeyPlacement::Header);
        assert_eq!(groups.path.len(), 1);
        assert_eq!(groups.path[0].name, "token");
        assert_eq!(groups.query.len(), 1);
        assert!(groups.headers.is_empty());
    }

    #[test]
    fn query_api_key_parameters_are_protected_not_arguments() {
        harnx_core::require_nextest();
        let root = json!({
            "openapi": "3.0.1",
            "paths": {},
            "components": {"securitySchemes": {
                "keyQuery": {"type": "apiKey", "in": "query", "name": "apiKey"}
            }}
        });
        let document = SpecDocument::from_value(root).unwrap();
        let schemes = SecuritySchemes::collect(&document);
        let mut compactor = SchemaCompactor::new(&document.root, "GET /x");
        let api_key = query_param(
            "query",
            "apiKey",
            json!({"name": "apiKey", "in": "query", "schema": {"type": "string"}}),
        );
        let groups =
            classify_parameters(&document, vec![api_key], &schemes, "GET /x", &mut compactor)
                .unwrap();
        assert!(groups.query.is_empty());
        assert_eq!(groups.protected[0].scheme, "keyQuery");
        assert_eq!(groups.protected[0].placement, ApiKeyPlacement::Query);
    }
}
