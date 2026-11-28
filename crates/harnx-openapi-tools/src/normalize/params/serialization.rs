//! Parameter schema projection and serialization metadata.

use serde_json::{json, Map, Value};

use crate::document::SpecKind;
use crate::error::{SpecError, SpecResult};

use super::MergedParam;

/// The parameter serialization styles this release can encode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamStyle {
    /// `form` with its explode flag. Swagger 2.0 query/formData `csv` maps to
    /// explode=false and `multi` to explode=true.
    Form { explode: bool },
    /// `simple` (path/header default), retaining explode because it changes
    /// object serialization even though it has no effect on primitive/array values.
    Simple { explode: bool },
}

/// Reshape a Swagger 2.0 non-body parameter (type keywords inline) into a
/// JSON schema. Swagger's `type: "file"` maps to a binary string.
pub fn swagger2_param_schema(param: &Value) -> Value {
    let Some(source) = param.as_object() else {
        return json!({});
    };
    let mut schema = Map::new();
    for (key, value) in source {
        if is_parameter_metadata(key) {
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

fn is_parameter_metadata(key: &str) -> bool {
    matches!(
        key,
        "name" | "in" | "description" | "required" | "collectionFormat"
    ) || key.starts_with("x-")
}

/// The serialization style a parameter uses, with unsupported styles rejected
/// instead of silently mis-encoded.
pub fn parameter_style(
    kind: SpecKind,
    param: &MergedParam,
    context: &str,
) -> SpecResult<ParamStyle> {
    match kind {
        SpecKind::OpenApi3 => openapi3_parameter_style(param, context),
        SpecKind::Swagger2 => swagger2_parameter_style(param, context),
    }
}

fn openapi3_parameter_style(param: &MergedParam, context: &str) -> SpecResult<ParamStyle> {
    let style = openapi3_style_name(param, context)?;
    let explode = openapi3_explode(param, context)?;
    style_for_location(&param.location, &param.name, style, explode, context)
}

fn openapi3_style_name<'a>(param: &'a MergedParam, context: &str) -> SpecResult<&'a str> {
    match param.param.get("style") {
        Some(Value::String(style)) => Ok(style.as_str()),
        Some(_) => Err(SpecError::invalid(
            context,
            format!("parameter {:?} has a non-string `style`", param.name),
        )),
        None => default_openapi3_style(&param.location, &param.name, context),
    }
}

fn default_openapi3_style(location: &str, name: &str, context: &str) -> SpecResult<&'static str> {
    match location {
        "path" | "header" => Ok("simple"),
        "query" | "cookie" => Ok("form"),
        location => Err(SpecError::unsupported(
            context,
            format!("parameter {name:?} has unsupported location {location:?} for serialization"),
        )),
    }
}

fn openapi3_explode(param: &MergedParam, context: &str) -> SpecResult<Option<bool>> {
    match param.param.get("explode") {
        Some(Value::Bool(explode)) => Ok(Some(*explode)),
        Some(_) => Err(SpecError::invalid(
            context,
            format!("parameter {:?} has a non-boolean `explode`", param.name),
        )),
        None => Ok(None),
    }
}

fn style_for_location(
    location: &str,
    name: &str,
    style: &str,
    explode: Option<bool>,
    context: &str,
) -> SpecResult<ParamStyle> {
    match (location, style) {
        ("query", "form") | ("cookie", "form") => Ok(ParamStyle::Form {
            explode: explode.unwrap_or(true),
        }),
        ("query", "simple") | ("path", "simple") | ("header", "simple") => {
            Ok(ParamStyle::Simple {
                explode: explode.unwrap_or(false),
            })
        }
        (location, "form" | "simple") => Err(SpecError::unsupported(
            context,
            format!(
                "parameter {name:?} at {location:?} cannot use serialization style {style:?}; \
                 supported styles are query: form/simple, path/header: simple, cookie: form"
            ),
        )),
        (location, other) => Err(SpecError::unsupported(
            context,
            format!(
                "parameter {name:?} at {location:?} uses serialization style {other:?}, which is not supported"
            ),
        )),
    }
}

fn swagger2_parameter_style(param: &MergedParam, context: &str) -> SpecResult<ParamStyle> {
    let collection_format = swagger2_collection_format(param, context)?;
    match (param.location.as_str(), collection_format) {
        ("path" | "header", "csv") => Ok(ParamStyle::Simple { explode: false }),
        ("path" | "header", other) => Err(SpecError::unsupported(
            context,
            format!(
                "parameter {:?} at {:?} uses collectionFormat {other:?}; only csv/simple serialization is supported for path and header parameters",
                param.name, param.location
            ),
        )),
        (_, "csv") => Ok(ParamStyle::Form { explode: false }),
        (_, "multi") => Ok(ParamStyle::Form { explode: true }),
        (_, other) => Err(SpecError::unsupported(
            context,
            format!(
                "parameter {:?} uses collectionFormat {other:?}, which is not supported (csv and multi are)",
                param.name
            ),
        )),
    }
}

fn swagger2_collection_format<'a>(param: &'a MergedParam, context: &str) -> SpecResult<&'a str> {
    match param.param.get("collectionFormat") {
        Some(Value::String(format)) => Ok(format.as_str()),
        Some(_) => Err(SpecError::invalid(
            context,
            format!(
                "parameter {:?} has a non-string `collectionFormat`",
                param.name
            ),
        )),
        None => Ok("csv"),
    }
}
