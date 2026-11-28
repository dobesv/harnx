//! Tool input-schema assembly: parameter groups, body input and `$defs`.

use serde_json::{json, Map, Value};

use crate::error::{SpecError, SpecResult};

use super::{BodySpec, NormalizedOperation, ParamSpec};

/// Assemble the grouped tool input schema: `path`, `query`, `headers`, `body`
/// properties whose requiredness matches the wire contract, plus the
/// operation's reachable `$defs`.
pub(super) fn assemble_input_schema(
    operation: &NormalizedOperation,
    defs: &Map<String, Value>,
) -> SpecResult<Value> {
    let mut properties = Map::new();
    let mut required_groups: Vec<String> = Vec::new();

    if !operation.path_params.is_empty() {
        properties.insert("path".to_string(), params_group(&operation.path_params));
        required_groups.push("path".to_string());
    }
    insert_optional_group(
        &mut properties,
        &mut required_groups,
        "query",
        &operation.query_params,
    );
    insert_optional_group(
        &mut properties,
        &mut required_groups,
        "headers",
        &operation.header_params,
    );
    if let Some(body) = &operation.body {
        properties.insert("body".to_string(), body_input(body));
        if body.required {
            required_groups.push("body".to_string());
        }
    }

    let mut schema = Map::new();
    schema.insert("type".to_string(), json!("object"));
    schema.insert("additionalProperties".to_string(), json!(false));
    if !properties.is_empty() {
        schema.insert("properties".to_string(), Value::Object(properties));
    }
    if !required_groups.is_empty() {
        schema.insert(
            "required".to_string(),
            Value::Array(required_groups.into_iter().map(Value::String).collect()),
        );
    }
    if !defs.is_empty() {
        schema.insert("$defs".to_string(), Value::Object(defs.clone()));
    }
    Ok(Value::Object(schema))
}

/// The published body input: the direct JSON value or the media envelope.
fn body_input(body: &BodySpec) -> Value {
    match &body.input {
        crate::envelope::BodyInput::Direct(schema) => schema.clone(),
        crate::envelope::BodyInput::Envelope(schema) => schema.clone(),
    }
}

fn insert_optional_group(
    properties: &mut Map<String, Value>,
    required_groups: &mut Vec<String>,
    name: &str,
    params: &[ParamSpec],
) {
    if params.is_empty() {
        return;
    }
    properties.insert(name.to_string(), params_group(params));
    if params.iter().any(|param| param.required) {
        required_groups.push(name.to_string());
    }
}

fn params_group(params: &[ParamSpec]) -> Value {
    let mut properties = Map::new();
    let mut required: Vec<Value> = Vec::new();
    for param in params {
        properties.insert(param.name.clone(), param.schema.clone());
        if param.required {
            required.push(Value::String(param.name.clone()));
        }
    }
    let mut group = Map::new();
    group.insert("type".to_string(), json!("object"));
    group.insert("additionalProperties".to_string(), json!(false));
    group.insert("properties".to_string(), Value::Object(properties));
    if !required.is_empty() {
        group.insert("required".to_string(), Value::Array(required));
    }
    Value::Object(group)
}

/// Check that the path template's `{name}` placeholders and the declared path
/// parameters describe each other exactly.
pub(super) fn validate_path_template(
    operation: &NormalizedOperation,
    context: &str,
) -> SpecResult<()> {
    let template_names = path_template_names(&operation.path);
    let declared: std::collections::BTreeSet<&str> = operation
        .path_params
        .iter()
        .map(|param| param.name.as_str())
        .collect();
    let template: std::collections::BTreeSet<&str> =
        template_names.iter().map(String::as_str).collect();
    let missing: Vec<&str> = template
        .iter()
        .filter(|name| !declared.contains(*name))
        .copied()
        .collect();
    if !missing.is_empty() {
        return Err(SpecError::invalid(
            context,
            format!(
                "path template placeholder(s) {} have no matching path parameter",
                missing.join(", ")
            ),
        ));
    }
    let undeclared: Vec<&str> = declared
        .iter()
        .filter(|name| !template.contains(*name))
        .copied()
        .collect();
    if !undeclared.is_empty() {
        return Err(SpecError::invalid(
            context,
            format!(
                "path parameter(s) {} do not appear in the path template",
                undeclared.join(", ")
            ),
        ));
    }
    Ok(())
}

/// The `{name}` placeholders of a path template.
pub fn path_template_names(path: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = path;
    while let Some(start) = rest.find('{') {
        let Some(end) = rest[start..].find('}') else {
            break;
        };
        names.push(rest[start + 1..start + end].to_string());
        rest = &rest[start + end + 1..];
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_templates_list_placeholders() {
        harnx_core::require_nextest();
        assert_eq!(
            path_template_names("/rest/api/3/issue/{issueIdOrKey}/worklog/{id}"),
            vec!["issueIdOrKey".to_string(), "id".to_string()]
        );
        assert!(path_template_names("/plain/path").is_empty());
    }
}
